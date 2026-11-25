import Foundation

/// One scene document's side of the scene host contract (docs/observe.md,
/// "Scene host contract"), apart from the web view that carries it: the
/// messages `{host_version: 1, type, request_id?, data}` a scene sends are
/// checked and answered here, and what the shell has for it (status,
/// environment, Observe snapshot and frames) goes out through `send`.
///
/// Every message from the scene is untrusted: its shape, size, rate and
/// meaning are checked before anything happens, and nothing it says can
/// widen what the app lets it do. It gets no token, endpoint, session id or
/// app state; only what Observe already gave its own grant.
@MainActor
final class SceneSession {
    static let hostVersion = SceneManifest.hostVersion
    /// The largest message a scene may send, serialised.
    static let maxMessage = 64 * 1024
    /// Requests: a burst of 40, then 20 a second.
    static let burst = 40.0
    static let perSecond = 20.0

    struct Hooks {
        /// One message to the scene.
        var send: @MainActor ([String: Any]) -> Void
        /// Hand a link to the app's router (`gesture`: the app saw the click).
        var open: @MainActor (_ url: String, _ gesture: Bool) -> Bool
        /// Ask the person, natively, to change their card as shown.
        var confirmCard: @MainActor (_ request: CardRequest) async -> Bool
        /// Write the person's card (the app's confirmed path).
        var writeCard: @MainActor (_ document: String) async throws -> Void
        var clock: @MainActor () -> ContinuousClock.Instant = { .now }
    }

    /// A card change a scene asked for, as the person is shown it.
    struct CardRequest: Equatable {
        let scene: String
        let schema: String
        /// The scene's part, as JSON (`null`: remove it).
        let value: String
        /// The whole card that would be written.
        let document: String
    }

    let scene: InstalledScene
    let storage: SceneStorage
    var hooks: Hooks
    /// The scene said `ready` (in this document).
    private(set) var ready = false
    /// The scene has been given a snapshot it may build on.
    private(set) var hasSnapshot = false
    /// What it was last told (`status`), for the shell.
    private(set) var lastStatus: String?
    /// Called when the scene (re)announces itself: the shell starts its feeds.
    var onReady: @MainActor () -> Void = {}

    private var tokens = SceneSession.burst
    private var refilled: ContinuousClock.Instant?
    private var opens: [ContinuousClock.Instant] = []
    private var cardBusy = false
    private var cardDeclinedAt: ContinuousClock.Instant?
    /// The person's card as Observe last showed it (`senate.person.card`),
    /// when the grant shows the person at all.
    private var personCard: [String: Any]?
    private var personKnown = false

    init(scene: InstalledScene, storage: SceneStorage, hooks: Hooks) {
        self.scene = scene
        self.storage = storage
        self.hooks = hooks
    }

    /// What this shell offers a scene, as the handshake reports.
    var capabilities: [String] {
        ["open", "storage"] + (scene.manifest.cardWrites.isEmpty ? [] : ["card.write"])
    }

    /// The document went away (navigation, reload, crash): nothing more goes
    /// to it, and nothing it sent is answered, until a new `ready`.
    func reset() {
        ready = false
        hasSnapshot = false
        lastStatus = nil
    }

    // MARK: To the scene

    private func send(_ type: String, _ data: [String: Any], request: String? = nil) {
        guard ready else { return }
        var message: [String: Any] = ["host_version": Self.hostVersion, "type": type, "data": data]
        if let request { message["request_id"] = request }
        hooks.send(message)
    }

    private func result(_ request: String?, ok: Bool, value: Any? = nil, code: String? = nil) {
        guard let request else { return }
        var data: [String: Any] = ["ok": ok]
        if let value { data["value"] = value }
        if let code { data["code"] = code }
        send("result", data, request: request)
    }

    func status(_ state: String) {
        guard ["ready", "access_needed", "data_unavailable", "version_mismatch"].contains(state) else { return }
        if state != "ready" { hasSnapshot = false }
        lastStatus = state
        send("status", ["state": state])
    }

    func environment(_ environment: [String: Any]) {
        send("environment", environment)
    }

    /// Observe's snapshot. It is passed on only when Observe offers every
    /// capability the scene requires; otherwise the scene is told the
    /// versions do not match and gets nothing. True: delivered.
    @discardableResult
    func snapshot(_ snapshot: [String: Any]) -> Bool {
        guard ready else { return false }
        let offered = Set(snapshot["capabilities"] as? [String] ?? [])
        guard Set(scene.manifest.required).isSubset(of: offered) else {
            status("version_mismatch")
            return false
        }
        notePerson(snapshot["senate"])
        hasSnapshot = true
        send("observe.snapshot", ["snapshot": snapshot])
        return true
    }

    /// An Observe frame, after the snapshot it follows. True: delivered.
    @discardableResult
    func frame(_ frame: [String: Any]) -> Bool {
        guard ready, hasSnapshot else { return false }
        if frame["kind"] as? String == "state.patch",
           let replace = (frame["data"] as? [String: Any])?["replace"] as? [String: Any], let senate = replace["senate"] {
            notePerson(senate)
        }
        send("observe.event", ["frame": frame])
        return true
    }

    private func notePerson(_ senate: Any?) {
        let person = (senate as? [String: Any])?["person"] as? [String: Any]
        personKnown = person != nil
        personCard = (person?["card"] as? [String: Any])?["value"] as? [String: Any]
    }

    // MARK: From the scene

    /// One message from the scene's main frame. `activated`: the app saw the
    /// person click or type in the page just now.
    func receive(_ body: Any, activated: Bool) {
        guard let message = body as? [String: Any],
              Set(message.keys).isSubset(of: ["host_version", "type", "request_id", "data"]),
              let version = message["host_version"] as? NSNumber, CFGetTypeID(version) != CFBooleanGetTypeID(),
              version.intValue == Self.hostVersion, version.doubleValue == 1,
              let type = message["type"] as? String, (1...64).contains(type.utf8.count)
        else { return }
        // A request id is a short string; anything else makes the message unreadable.
        var request: String?
        if let raw = message["request_id"] {
            guard let id = raw as? String, (1...64).contains(id.utf8.count), id.unicodeScalars.allSatisfy({ $0.isASCII && $0.value >= 0x20 })
            else { return }
            request = id
        }
        guard Self.plainJSON(message), let bytes = try? JSONSerialization.data(withJSONObject: message),
              bytes.count <= Self.maxMessage
        else { return result(ready ? request : nil, ok: false, code: "invalid_request") }
        let data: [String: Any]
        switch message["data"] {
        case nil: data = [:]
        case let object as [String: Any]: data = object
        default: return result(ready ? request : nil, ok: false, code: "invalid_request")
        }
        if type == "ready" { return handshake(data, request: request) }
        // Before the handshake (or after the document changed) nothing is answered.
        guard ready else { return }
        guard take() else { return result(request, ok: false, code: "quota") }
        switch type {
        case "open": open(data, request: request, activated: activated)
        case "storage.get", "storage.set", "storage.delete": store(type, data, request: request)
        case "card.write": writeCard(data, request: request)
        default: result(request, ok: false, code: "unsupported")
        }
    }

    private func take() -> Bool {
        let now = hooks.clock()
        if let refilled {
            let seconds = Double((now - refilled).components.seconds) + Double((now - refilled).components.attoseconds) / 1e18
            tokens = min(Self.burst, tokens + seconds * Self.perSecond)
        }
        refilled = now
        guard tokens >= 1 else { return false }
        tokens -= 1
        return true
    }

    private func handshake(_ data: [String: Any], request: String?) {
        guard let versions = data["supported_host_versions"] as? [Any],
              versions.contains(where: { ($0 as? NSNumber).map { CFGetTypeID($0) != CFBooleanGetTypeID() && $0.doubleValue == Double(Self.hostVersion) } == true })
        else {
            // It speaks no version this shell does: answered once, then silence.
            if let request {
                ready = true
                result(request, ok: false, code: "unsupported")
                status("version_mismatch")
                ready = false
            }
            return
        }
        // A (re)loaded document: it knows nothing yet.
        ready = true
        hasSnapshot = false
        tokens = Self.burst
        refilled = nil
        result(request, ok: true, value: ["version": Self.hostVersion, "capabilities": capabilities])
        status("data_unavailable")
        onReady()
    }

    private func open(_ data: [String: Any], request: String?, activated: Bool) {
        guard let url = data["url"] as? String, (1...2048).contains(url.utf8.count) else {
            return result(request, ok: false, code: "invalid_request")
        }
        // At most five a second: a page that floods is ignored.
        let now = hooks.clock()
        opens = opens.filter { now - $0 < .seconds(1) } + [now]
        guard opens.count <= 5 else { return result(request, ok: false, code: "quota") }
        // The app's one router validates the original string and routes. A
        // click the app itself saw counts as the person's gesture; anything
        // else is treated as a system delivery.
        let accepted = hooks.open(url, activated)
        result(request, ok: accepted, code: accepted ? nil : "invalid_request")
    }

    private func store(_ type: String, _ data: [String: Any], request: String?) {
        guard let request else { return }
        guard let key = data["key"] as? String, SceneStorage.validKey(key) else {
            return result(request, ok: false, code: "invalid_request")
        }
        switch type {
        case "storage.get":
            let value = storage.get(key).flatMap { try? JSONSerialization.jsonObject(with: Data($0.utf8), options: .fragmentsAllowed) }
            result(request, ok: true, value: value ?? NSNull())
        case "storage.set":
            // Only plain JSON (no NaN, no dates), checked before serialising
            // (which would otherwise raise and take the app down).
            guard let value = data["value"], Self.plainJSON(value),
                  let json = try? JSONSerialization.data(withJSONObject: value, options: .fragmentsAllowed)
            else { return result(request, ok: false, code: "invalid_request") }
            guard storage.set(key, json: String(decoding: json, as: UTF8.self)) else {
                return result(request, ok: false, code: "quota")
            }
            result(request, ok: true)
        default:
            storage.delete(key)
            result(request, ok: true)
        }
    }

    /// `card.write {schema, value}`: the scene's part of the person's own
    /// profile card, under a schema its manifest declares, after the person
    /// says yes in the app. The rest of the card is kept as it is.
    private func writeCard(_ data: [String: Any], request: String?) {
        guard let request else { return }
        guard let schema = data["schema"] as? String, scene.manifest.cardWrites.contains(schema),
              let value = data["value"], value is NSNull || value is [String: Any], Self.plainJSON(value)
        else { return result(request, ok: false, code: "invalid_request") }
        // One at a time; after a no, a minute of quiet (no stream of prompts).
        let now = hooks.clock()
        if let at = cardDeclinedAt, now - at < .seconds(60) { return result(request, ok: false, code: "declined") }
        guard !cardBusy else { return result(request, ok: false, code: "quota") }
        // Without the person's row in this grant the rest of the card is unknown: nothing to merge into.
        guard personKnown else { return result(request, ok: false, code: "unavailable") }
        var card = personCard ?? [:]
        if value is NSNull { card.removeValue(forKey: schema) } else { card[schema] = value }
        guard let document = Self.cardDocument(card),
              let part = try? JSONSerialization.data(withJSONObject: value, options: [.fragmentsAllowed, .sortedKeys, .withoutEscapingSlashes])
        else { return result(request, ok: false, code: "quota") }
        let shown = CardRequest(scene: scene.manifest.name, schema: schema, value: String(decoding: part, as: UTF8.self),
                                document: document)
        cardBusy = true
        Task { [weak self] in
            guard let self else { return }
            defer { cardBusy = false }
            guard await hooks.confirmCard(shown) else {
                cardDeclinedAt = hooks.clock()
                return result(request, ok: false, code: "declined")
            }
            do {
                try await hooks.writeCard(document)
                result(request, ok: true)
            } catch {
                result(request, ok: false, code: "unavailable")
            }
        }
    }

    /// A whole card as the daemon takes it, within Observe's limits (2,048
    /// bytes compact, eight levels with the root, 128 keys and scalars), or
    /// nil. An empty card is cleared (`null`).
    nonisolated static func cardDocument(_ card: [String: Any]) -> String? {
        guard !card.isEmpty else { return "null" }
        func measure(_ value: Any, depth: Int) -> Int? {
            switch value {
            case let object as [String: Any]:
                guard depth <= 8 else { return nil }
                var n = 0
                for (_, v) in object {
                    guard let inner = measure(v, depth: depth + 1) else { return nil }
                    n += 1 + inner
                }
                return n
            case let array as [Any]:
                guard depth <= 8 else { return nil }
                var n = 0
                for v in array {
                    guard let inner = measure(v, depth: depth + 1) else { return nil }
                    n += inner
                }
                return n
            default: return 1
            }
        }
        guard plainJSON(card), let count = measure(card, depth: 1), count <= 128,
              let data = try? JSONSerialization.data(withJSONObject: card, options: [.sortedKeys, .withoutEscapingSlashes]), data.count <= 2048
        else { return nil }
        return String(decoding: data, as: UTF8.self)
    }

    /// JSON a scene may hand the app: strings, finite numbers, booleans,
    /// null, and arrays and objects of those (nesting bounded).
    nonisolated static func plainJSON(_ value: Any, depth: Int = 0) -> Bool {
        guard depth < 16 else { return false }
        switch value {
        case is NSNull, is String: return true
        case let n as NSNumber:
            return CFGetTypeID(n) == CFBooleanGetTypeID() || n.doubleValue.isFinite
        case let a as [Any]: return a.allSatisfy { plainJSON($0, depth: depth + 1) }
        case let o as [String: Any]: return o.values.allSatisfy { plainJSON($0, depth: depth + 1) }
        default: return false
        }
    }
}
