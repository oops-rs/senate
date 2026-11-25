import Foundation
import Security
import SenateCore

/// A native client of the Senate's public Observe API (docs/observe.md): the
/// state snapshot, then the event stream (SSE) resumed from the last cursor,
/// a fresh snapshot after `409 resync_required` or `stream.reset`, and
/// backed-off reconnects. It holds the bearer; nothing it hands on carries it.
/// Snapshots and frames are passed on as the JSON objects they are (checked
/// for version, shape and size, not interpreted): the scene reads them.
///
/// Where it sends the bearer: only to the endpoint the daemon vouches for
/// over the app's own IPC, asked again before every (re)connect, of the
/// installation the grant was issued by, on a numeric loopback address, with
/// no redirects. A process that takes the port while the daemon is down gets
/// nothing: without the daemon's answer there is no request at all.
///
/// That answer is as trustworthy as the app's IPC to the daemon (bridge.sock).
/// Review 3 (N4): the socket's peer is not yet authenticated; once `guard`'s
/// socket identity lands, the app verifies the daemon end of that IPC as it
/// specifies, and this client inherits it.
@MainActor
final class ObserveClient {
    enum Status { case ready, accessNeeded, unavailable, versionMismatch }

    var onSnapshot: ([String: Any]) -> Void = { _ in }
    var onFrame: ([String: Any]) -> Void = { _ in }
    var onStatus: (Status) -> Void = { _ in }

    private let grant: SceneAccess.Grant
    /// The daemon's word on where Observe is now (app IPC).
    private let endpoint: () async throws -> ObserveEndpoint
    private var task: Task<Void, Never>?
    private let session: URLSession

    static let maxSnapshot = 4 * 1024 * 1024
    static let maxFrame = 64 * 1024

    init(grant: SceneAccess.Grant, endpoint: @escaping () async throws -> ObserveEndpoint) {
        self.grant = grant
        self.endpoint = endpoint
        let config = URLSessionConfiguration.ephemeral
        config.httpCookieStorage = nil
        config.urlCache = nil
        config.requestCachePolicy = .reloadIgnoringLocalCacheData
        config.timeoutIntervalForRequest = 30
        config.timeoutIntervalForResource = .infinity
        config.connectionProxyDictionary = [:]
        session = URLSession(configuration: config, delegate: NoRedirects(), delegateQueue: nil)
    }

    var running: Bool { task != nil }

    func start() {
        guard task == nil else { return }
        task = Task { [weak self] in await self?.run() }
    }

    func stop() {
        task?.cancel()
        task = nil
        session.invalidateAndCancel()
    }

    private static let cursor = try? NSRegularExpression(pattern: "^[A-Za-z0-9_-]{21}[AQgw]$")
    private static func validCursor(_ value: Any?) -> String? {
        guard let s = value as? String, let re = cursor,
              re.firstMatch(in: s, range: NSRange(s.startIndex..., in: s)) != nil else { return nil }
        return s
    }

    /// A base URL the bearer may go to: plain http on a numeric loopback
    /// address with a port, nothing else in it.
    nonisolated static func vouched(_ raw: String) -> URL? {
        guard let url = URL(string: raw), url.scheme == "http", let host = url.host(percentEncoded: false),
              url.port != nil, url.user == nil, url.password == nil, url.query == nil, url.fragment == nil,
              url.path.isEmpty || url.path == "/"
        else { return nil }
        let loopback = host == "::1" || host == "[::1]"
            || (host.hasPrefix("127.") && host.split(separator: ".").count == 4 && host.split(separator: ".").allSatisfy { UInt8($0) != nil })
        return loopback ? url : nil
    }

    private func request(_ base: URL, _ path: String, cursor: String? = nil) -> URLRequest? {
        guard let url = URL(string: path, relativeTo: base)?.absoluteURL else { return nil }
        var r = URLRequest(url: url)
        r.setValue("Bearer \(grant.token)", forHTTPHeaderField: "Authorization")
        if let cursor { r.setValue(cursor, forHTTPHeaderField: "Last-Event-ID") }
        return r
    }

    /// Seconds a 429/503 asks to wait (`Retry-After`), bounded.
    private static func retryAfter(_ response: URLResponse) -> Double? {
        guard let http = response as? HTTPURLResponse, http.statusCode == 429 || http.statusCode == 503 else { return nil }
        return retryAfterSeconds(http.value(forHTTPHeaderField: "Retry-After"))
    }

    /// A `Retry-After` value as whole seconds, 1–300, or nil: parsed
    /// defensively (only digits; `nan`, `inf`, negatives, dates and huge
    /// numbers never reach a sleep, which would trap on them).
    nonisolated static func retryAfterSeconds(_ value: String?) -> Double? {
        guard let value = value?.trimmingCharacters(in: .whitespaces), !value.isEmpty, value.count <= 6,
              value.allSatisfy(\.isASCII), value.allSatisfy(\.isNumber), let seconds = Int(value) else { return nil }
        return Double(min(max(seconds, 1), 300))
    }

    private func run() async {
        var cursor: String?
        var delay = 0.5
        while !Task.isCancelled {
            var wait: Double?
            do {
                // Where Observe is now, from the daemon itself, every time.
                guard let found = try? await endpoint() else { throw URLError(.cannotConnectToHost) }
                guard found.id == grant.installation, let base = Self.vouched(found.url) else {
                    // Another installation, or an address the bearer must not go to.
                    onStatus(.accessNeeded)
                    task = nil
                    return
                }
                if cursor == nil {
                    guard let r = request(base, "/v1/observe/state") else { throw URLError(.badURL) }
                    let (bytes, response) = try await session.bytes(for: r)
                    let code = (response as? HTTPURLResponse)?.statusCode ?? 0
                    if code == 401 || code == 403 { onStatus(.accessNeeded); task = nil; return }
                    wait = Self.retryAfter(response)
                    guard code == 200 else { throw URLError(.badServerResponse) }
                    var data = Data()
                    for try await byte in bytes {
                        data.append(byte)
                        if data.count > Self.maxSnapshot { throw URLError(.dataLengthExceedsMaximum) }
                    }
                    guard let snapshot = try JSONSerialization.jsonObject(with: data) as? [String: Any] else {
                        throw URLError(.cannotParseResponse)
                    }
                    guard snapshot["schema_version"] as? Int == 1 else { onStatus(.versionMismatch); task = nil; return }
                    guard let c = Self.validCursor(snapshot["cursor"]) else { throw URLError(.cannotParseResponse) }
                    cursor = c
                    onSnapshot(snapshot)
                    onStatus(.ready)
                }
                guard let r = request(base, "/v1/observe/events", cursor: cursor) else { throw URLError(.badURL) }
                let (bytes, response) = try await session.bytes(for: r)
                let code = (response as? HTTPURLResponse)?.statusCode ?? 0
                if code == 409 { cursor = nil; continue }
                if code == 401 || code == 403 { onStatus(.accessNeeded); task = nil; return }
                wait = Self.retryAfter(response)
                guard code == 200 else { throw URLError(.badServerResponse) }
                delay = 0.5
                // One JSON object on a `data:` line, after its `id:` line; comment
                // heartbeats (`:`) and blank lines carry nothing. Lines are bounded.
                var id: String?
                var line = Data()
                for try await byte in bytes {
                    if Task.isCancelled { return }
                    if byte != 0x0A {
                        line.append(byte)
                        if line.count > Self.maxFrame + 16 { throw URLError(.cannotParseResponse) }
                        continue
                    }
                    defer { line.removeAll(keepingCapacity: true) }
                    if line.last == 0x0D { line.removeLast() }
                    if line.starts(with: Data("id:".utf8)) {
                        id = String(decoding: line.dropFirst(3), as: UTF8.self).trimmingCharacters(in: .whitespaces)
                    } else if line.starts(with: Data("data:".utf8)) {
                        guard let frame = try JSONSerialization.jsonObject(with: line.dropFirst(5)) as? [String: Any],
                              frame["schema_version"] as? Int == 1
                        else { throw URLError(.cannotParseResponse) }
                        if frame["kind"] as? String == "stream.reset" {
                            cursor = nil
                            break
                        }
                        guard let c = Self.validCursor(frame["cursor"]), c == id else { throw URLError(.cannotParseResponse) }
                        id = nil
                        if c == cursor { continue }
                        cursor = c
                        onFrame(frame)
                    }
                }
            } catch {
                if Task.isCancelled { return }
                if (error as? URLError)?.code == .cannotParseResponse || (error as? URLError)?.code == .dataLengthExceedsMaximum { cursor = nil }
                onStatus(.unavailable)
            }
            try? await Task.sleep(for: .seconds(wait ?? delay * Double.random(in: 0.75...1.25)))
            delay = min(delay * 2, 30)
        }
    }
}

/// Redirects are refused: the bearer goes only where the daemon said.
private final class NoRedirects: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
    nonisolated func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse,
                                newRequest request: URLRequest, completionHandler: @escaping @Sendable (URLRequest?) -> Void) {
        completionHandler(nil)
    }
}

/// Scenes' observer grants: one per scene and Senate, created when the
/// person allows that scene (metadata only, every committee, this Senate's
/// direct links only if the scene asks), kept in the Keychain as the bearer,
/// its grant id, the name it was issued under and the installation that
/// issued it (never an address: that is asked of the daemon each time).
/// Before use it is checked against the daemon's own list of grants, so an
/// item planted in the Keychain is ignored.
enum SceneAccess {
    struct Grant: Codable, Equatable {
        var token: String
        var id: String
        /// The Observe installation that issued it (the grant works with no other).
        var installation: String
        var expiresAt: String
        /// The name it was issued under (`InstalledScene.grantName`).
        var name: String
        /// Whether it includes this Senate's direct links.
        var topology: Bool

        var expired: Bool {
            (ISO8601DateFormatter.withFractions.date(from: expiresAt) ?? ISO8601DateFormatter().date(from: expiresAt))
                .map { $0 <= Date() } ?? true
        }
    }

    private static let service = "dev.senate.app.scene.observe"

    /// The Keychain account: the Senate, then the scene.
    private static func account(_ senate: String, _ scene: String) -> String { "\(scene):\(senate)" }

    /// The grant kept for this scene and Senate, if any and still in date.
    static func load(_ senate: String, scene: String) -> Grant? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service,
            kSecAttrAccount as String: account(senate, scene), kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var out: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &out) == errSecSuccess, let data = out as? Data,
              let grant = try? JSONDecoder().decode(Grant.self, from: data), !grant.expired else { return nil }
        return grant
    }

    static func save(_ grant: Grant, _ senate: String, scene: String) {
        guard let data = try? JSONEncoder().encode(grant) else { return }
        let match: [String: Any] = [kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service,
                                    kSecAttrAccount as String: account(senate, scene)]
        SecItemDelete(match as CFDictionary)
        var add = match
        add[kSecValueData as String] = data
        add[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlockedThisDeviceOnly
        SecItemAdd(add as CFDictionary, nil)
    }

    static func forget(_ senate: String, scene: String) {
        let match: [String: Any] = [kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service,
                                    kSecAttrAccount as String: account(senate, scene)]
        SecItemDelete(match as CFDictionary)
    }

    /// Every Keychain item this scene has, for any Senate (a removed scene leaves none).
    static func forgetEverywhere(scene: String) {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service,
            kSecReturnAttributes as String: true, kSecMatchLimit as String: kSecMatchLimitAll,
        ]
        var out: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &out) == errSecSuccess, let items = out as? [[String: Any]] else { return }
        for item in items {
            guard let account = item[kSecAttrAccount as String] as? String, account.hasPrefix("\(scene):") else { continue }
            let match: [String: Any] = [kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service,
                                        kSecAttrAccount as String: account]
            SecItemDelete(match as CFDictionary)
        }
    }

    /// A kept grant the daemon really issued to this scene, still live, by
    /// the installation that answers now: anything else is not used.
    static func vouched(_ grant: Grant, grants: [ObserveGrantInfo], installation: String, scene: InstalledScene) -> Bool {
        grant.installation == installation && !grant.expired && grant.name.hasSuffix(" · \(scene.grantTag)")
            && grants.contains { $0.id == grant.id && $0.name == grant.name }
    }

    /// The daemon's grants that belong to this scene (by its tag).
    static func issued(to scene: InstalledScene, in grants: [ObserveGrantInfo]) -> [ObserveGrantInfo] {
        grants.filter { $0.name.hasSuffix(" · \(scene.grantTag)") }
    }

    /// One click: a grant for this scene in this Senate, of exactly the scope
    /// its manifest asks for, through the person-only path, kept in the
    /// Keychain. Its earlier grants are revoked: one live grant per scene.
    static func allow(_ scene: InstalledScene, senate: String, core: any AppCoreProtocol) async throws -> Grant {
        let issued = try await core.observeGrant(name: scene.grantName, topology: scene.manifest.wantsTopology)
        let endpoint = try await core.observeEndpoint()
        let grant = Grant(token: issued.token, id: issued.id, installation: endpoint.id, expiresAt: issued.expiresAt,
                          name: scene.grantName, topology: scene.manifest.wantsTopology)
        save(grant, senate, scene: scene.id)
        for old in Self.issued(to: scene, in: (try? await core.observeGrants()) ?? []) where old.id != issued.id {
            try? await core.observeRevoke(id: old.id)
        }
        return grant
    }

    /// Revoke every grant this scene holds on the daemon and drop what the
    /// Keychain keeps for it in this Senate.
    static func revoke(_ scene: InstalledScene, senate: String, core: (any AppCoreProtocol)?) async {
        if let core {
            for grant in Self.issued(to: scene, in: (try? await core.observeGrants()) ?? []) {
                try? await core.observeRevoke(id: grant.id)
            }
        }
        forget(senate, scene: scene.id)
    }

    /// For a scene being removed (by its identity, even when its files can no
    /// longer be read): its grants revoked and every Keychain item gone.
    static func removeAll(scene id: String, core: (any AppCoreProtocol)?) async {
        if let core {
            let tag = " · \(InstalledScene.grantTag(id))"
            for grant in ((try? await core.observeGrants()) ?? []) where grant.name.hasSuffix(tag) {
                try? await core.observeRevoke(id: grant.id)
            }
        }
        forgetEverywhere(scene: id)
    }
}

extension ISO8601DateFormatter {
    static let withFractions: ISO8601DateFormatter = {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return f
    }()
}
