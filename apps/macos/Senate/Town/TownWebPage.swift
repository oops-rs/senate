import AppKit
import SwiftUI
import WebKit
import SenateCore

/// The web Town: the scene built from the top-level `town/` package (in the
/// bundle as `town/`), hosted under the scene host contract
/// (docs/observe.md, "Scene host contract"): one ordered channel of JSON
/// messages `{host_version: 1, type, request_id?, data}`, here bound to the
/// `scene` script handler (in) and `__senateScene(message)` (out). Shown
/// instead of the SpriteKit map while `SENATE_APP_TOWN_WEB=1`.
///
/// This is the shell; the page is content and knows nothing of the app:
/// - It sends `status`, `environment` (colour scheme, reduced motion,
///   locale, time zone, visibility, viewport), `observe.snapshot` and
///   `observe.event` frames. Until the app's native observe client lands,
///   the snapshot and frames are projected here from the store's town (the
///   same public shapes; no token, session id or private state goes in).
/// - It answers `storage.*` from a small per-scene store (16 KiB a value,
///   64 KiB in all) and declines `card.write` for now.
/// - `open {url}` is honoured only for a `senate://open/v1/…` link this
///   shell itself put into the page (every link maps to a destination it
///   knows), at most five a second.
///
/// Safety, as the conversation page:
/// - Everything comes from the bundle's `town/` folder through the
///   `senate-town:` scheme, a fixed set of file types, no path out of the
///   folder; the page comes with its Content-Security-Policy: scripts,
///   styles, images and its map only from that scheme (images also as
///   `data:`, which is how Phaser makes its blank textures), nothing else.
/// - A content rule list blocks every http(s), ws(s), file, ftp and blob
///   load whatever the page says; if it does not compile the page is not
///   loaded.
/// - Data goes in as call arguments, never as script.
/// - Navigation away and new windows are refused.
@MainActor
final class TownWebPage: NSObject {
    static let enabled = ProcessInfo.processInfo.environment["SENATE_APP_TOWN_WEB"] == "1"
    /// `SENATE_APP_TOWN_DEMO=1` (or `large`): the page's own dev shell replays its demo recording.
    static let demo: String? = ProcessInfo.processInfo.environment[TownScript.env].map { $0 == "large" ? "large" : "small" }
    /// The page on screen (snapshots, measurement).
    static weak var shown: TownWebPage?

    let web: WKWebView
    private weak var store: CoreStore?
    private let page: URL
    /// The page said `ready` (or, in the demo, finished loading).
    private(set) var ready = false
    private var state: TownState?
    /// Every link handed to the page, and where it leads.
    private var links: [String: TownDestination] = [:]
    private var opens: [ContinuousClock.Instant] = []
    private var feed: TownActivityFeed?
    private var cursor: UInt64 = 0
    private var storage: [String: String] = [:]
    private var colorScheme = "light"
    private var reduceMotion = false
    private var visible = false

    init(store: CoreStore) {
        self.store = store
        page = Self.demo.map { URL(string: "senate-town://town/index.html?demo=\($0)")! } ?? TownScheme.page
        let config = WKWebViewConfiguration()
        config.websiteDataStore = .nonPersistent()
        config.setURLSchemeHandler(TownScheme(), forURLScheme: TownScheme.scheme)
        config.preferences.javaScriptCanOpenWindowsAutomatically = false
        config.preferences.isElementFullscreenEnabled = false
        web = WKWebView(frame: .zero, configuration: config)
        super.init()
        if Self.demo == nil {
            config.userContentController.add(WeakTownHandler(self), contentWorld: .page, name: "scene")
        }
        web.navigationDelegate = self
        web.uiDelegate = self
        web.allowsBackForwardNavigationGestures = false
        web.allowsMagnification = false
        web.allowsLinkPreview = false
        Self.noNetwork { [weak self] list in
            guard let self else { return }
            guard let list else {
                Log.app.error("town page: the no-network rule list did not compile; the page is not loaded")
                return
            }
            web.configuration.userContentController.add(list)
            web.load(URLRequest(url: page))
        }
    }

    // MARK: Feeding the page

    /// The town as the store has it now, as a public snapshot.
    func show(_ state: TownState) {
        guard Self.demo == nil, state != self.state else { return }
        self.state = state
        send("observe.snapshot", ["snapshot": snapshot(state)])
    }

    func setOptions(reduceMotion: Bool? = nil, dark: Bool? = nil, visible: Bool? = nil) {
        if let reduceMotion { self.reduceMotion = reduceMotion }
        if let dark { colorScheme = dark ? "dark" : "light" }
        if let visible { self.visible = visible }
        sendEnvironment()
        if visible == true { startFeed() }
        if visible == false { feed?.stop() }
    }

    private func sendEnvironment() {
        let size = web.bounds.size
        send("environment", [
            "color_scheme": colorScheme, "reduced_motion": reduceMotion,
            "locale": Locale.current.identifier(.bcp47), "time_zone": TimeZone.current.identifier, "visible": visible,
            "viewport": ["width": size.width, "height": size.height],
            "safe_area": ["top": 0, "right": 0, "bottom": 0, "left": 0],
        ])
    }

    /// The committees' activity, live, while the page is shown, as public frames.
    private func startFeed() {
        // Only once the page can take what arrives: the feed starts at the
        // log's head, so nothing it sends is missed or replayed.
        guard let store, ready, visible, Self.demo == nil else { return }
        if feed == nil {
            feed = TownActivityFeed(store: store, entries: { [weak self] entries in
                guard let self else { return }
                for frame in entries.compactMap(self.frame) { self.send("observe.event", ["frame": frame]) }
            }, deliver: { _ in })
        }
        feed?.start()
    }

    func evaluate(_ js: String, _ args: [String: Any] = [:]) async -> Any? {
        guard ready else { return nil }
        return try? await web.callAsyncJavaScript(js, arguments: args, in: nil, contentWorld: .page)
    }

    /// One message to the scene.
    private func send(_ type: String, _ data: [String: Any], request: String? = nil) {
        guard ready, Self.demo == nil else { return }
        var message: [String: Any] = ["host_version": 1, "type": type, "data": data]
        if let request { message["request_id"] = request }
        web.callAsyncJavaScript("window.__senateScene(m)", arguments: ["m": message], in: nil, in: .page) { result in
            if case let .failure(error) = result {
                Log.app.error("town page: \(error.localizedDescription, privacy: .public)")
            }
        }
    }

    // MARK: The public shapes (until the native observe client)

    private func link(_ url: String, _ to: TownDestination) -> String {
        links[url] = to
        return url
    }

    private func snapshot(_ town: TownState) -> [String: Any] {
        var senators: [[String: Any]] = []
        var threads: [[String: Any]] = []
        for c in town.committees {
            for s in c.senators {
                let url = TownLink.senator(committee: c.name, alias: s.name)
                senators.append([
                    "id": s.id, "committee_id": c.name, "alias": s.name,
                    "runtime": s.runtime == .other ? NSNull() as Any : s.runtime.rawValue, "kind": "agent",
                    "state": Self.state[s.mood] ?? "unknown", "last_active_at": NSNull(), "hosted": s.sessionId == nil ? "unknown" : "here",
                    "viewer_attached": NSNull(),
                    "link": link(url, s.sessionId.map { .session($0) } ?? .committee(c.name)),
                ])
            }
            for w in c.waiting {
                threads.append([
                    "committee_id": c.name, "root_seq": String(w.root), "kind": "question", "state": "open", "waits_on": "person",
                    "link": link(TownLink.thread(committee: c.name, root: w.root), .thread(c.name, w.root)),
                ])
            }
        }
        var workers: [String] = []
        if let sec = town.hall.secretary {
            workers.append(sec.id)
            senators.append([
                "id": sec.id, "committee_id": "", "alias": sec.name, "runtime": sec.runtime.rawValue, "kind": "agent",
                "state": Self.state[sec.mood] ?? "unknown", "last_active_at": NSNull(), "hosted": "here", "viewer_attached": NSNull(),
                "link": link(TownLink.desk, .secretary),
            ])
        }
        cursor += 1
        return [
            "schema_version": 1,
            "capabilities": ["state_patch", "desk", "app_links"],
            "as_of": ISO8601DateFormatter().string(from: .now),
            "cursor": "app-\(cursor)",
            "senate": ["id": "local", "name": "local", "person": town.person.map { $0 as Any } ?? NSNull(), "desk_workers": workers],
            "navigation": [
                "desk": link(TownLink.desk, .secretary), "approvals": link(TownLink.approvals, .approvals),
                "inbox": link(TownLink.inbox, .inbox), "history": link(TownLink.history, .history),
            ],
            "links": [], "projects": [], "approvals": [],
            "committees": town.committees.map { c in [
                "id": c.name, "name": c.name, "chair_id": c.senators.first(where: \.chair).map { $0.id as Any } ?? NSNull(), "archived": false,
                "project_id": NSNull(), "link": link(TownLink.committee(c.name), .committee(c.name)),
            ] as [String: Any] },
            "senators": senators,
            "threads": threads,
            "desk_items": (0..<town.hall.docket).map { ["committee_id": "", "root_seq": String($0 + 1), "kind": "question", "awaiting": "unknown"] },
        ]
    }

    private static let state: [TownMood: String] = [
        .working: "working", .idle: "idle", .waiting: "waiting_on_person", .paused: "paused", .stopped: "stopped",
    ]

    /// An activity entry as a public `message.posted` frame (metadata only: no line).
    private func frame(_ e: TownActivityEntry) -> [String: Any]? {
        guard TownActivity.spoken.contains(e.kind) else { return nil }
        let ref = { (who: String) -> [String: Any] in ["kind": "senator", "id": TownText.senatorId(who)] }
        let root = e.thread ?? e.seq
        cursor += 1
        return [
            "schema_version": 1, "cursor": "app-\(cursor)", "at": ISO8601DateFormatter().string(from: .now),
            "kind": "message.posted", "committee_id": e.committee,
            "data": [
                "seq": String(e.seq), "message_kind": e.kind, "from": ref(e.onBehalfOf ?? e.from), "to": e.to.map(ref),
                "root_seq": String(root), "link": link(TownLink.thread(committee: e.committee, root: root), .thread(e.committee, root)),
            ] as [String: Any],
        ]
    }

    // MARK: What the page sends

    fileprivate func handle(_ body: Any, mainFrame: Bool) {
        guard mainFrame, let body = body as? [String: Any], body["host_version"] as? Int == 1,
              let type = body["type"] as? String else { return }
        let data = body["data"] as? [String: Any] ?? [:]
        let request = (body["request_id"] as? String).flatMap { $0.count <= 64 ? $0 : nil }
        switch type {
        case "ready":
            ready = true
            send("status", ["state": "ready"])
            sendEnvironment()
            if let state {
                self.state = nil
                show(state)
            }
            startFeed()
        case "open":
            guard let url = data["url"] as? String, url.utf8.count <= 2048 else { return }
            // At most five a second: a page that floods is ignored.
            let now = ContinuousClock.now
            opens = opens.filter { now - $0 < .seconds(1) } + [now]
            guard opens.count <= 5 else { return }
            open(url)
        case "storage.get", "storage.set", "storage.delete":
            guard let request else { return }
            guard let key = data["key"] as? String, key.count <= 64, key.allSatisfy({ $0.isASCII && ($0.isLetter || $0.isNumber || "-._~".contains($0)) }) else {
                return send("result", ["ok": false, "code": "invalid_request"], request: request)
            }
            switch type {
            case "storage.get":
                let value = storage[key].flatMap { try? JSONSerialization.jsonObject(with: Data($0.utf8), options: .fragmentsAllowed) }
                send("result", ["ok": true, "value": value ?? NSNull()], request: request)
            case "storage.set":
                guard let value = data["value"], let json = try? JSONSerialization.data(withJSONObject: value, options: .fragmentsAllowed),
                      json.count <= 16 * 1024,
                      storage.filter({ $0.key != key }).values.reduce(json.count, { $0 + $1.utf8.count }) <= 64 * 1024
                else { return send("result", ["ok": false, "code": "quota"], request: request) }
                storage[key] = String(decoding: json, as: UTF8.self)
                send("result", ["ok": true], request: request)
            default:
                storage[key] = nil
                send("result", ["ok": true], request: request)
            }
        case "card.write":
            if let request { send("result", ["ok": false, "code": "unsupported"], request: request) }
        case "error":
            let message = (data["message"] as? String).map { String($0.prefix(500)) } ?? ""
            Log.app.error("town page: \(message, privacy: .public)")
            #if DEBUG
            print("town page error: \(message)")
            #endif
        default:
            if let request { send("result", ["ok": false, "code": "unsupported"], request: request) }
        }
    }

    /// Only a link this shell put into the page leads anywhere.
    private func open(_ url: String) {
        guard let store, let to = links[url] else {
            Log.app.info("town page: refused an unknown link")
            return
        }
        switch to {
        case let .session(id):
            store.place = store.secretarySession?.sessionId == id ? .secretary : .session(id)
        case let .committee(name):
            store.place = .committee(name)
        case let .thread(committee, root):
            store.openThread(committee: committee, root: root)
        case .secretary:
            store.place = .secretary
        case .history:
            store.place = .section(.history)
        case .approvals:
            store.place = .section(.approvals)
        case .inbox:
            store.place = .section(.inbox)
        }
    }

    // MARK: The rule list

    private static var compiled: WKContentRuleList?
    private static func noNetwork(_ done: @escaping @MainActor (WKContentRuleList?) -> Void) {
        if let compiled { return done(compiled) }
        let schemes = ["^https?:", "^wss?:", "^file:", "^ftp:", "^blob:"]
        let rules = "[" + schemes.map { #"{"trigger":{"url-filter":""# + $0 + #""},"action":{"type":"block"}}"# }
            .joined(separator: ",") + "]"
        WKContentRuleListStore.default().compileContentRuleList(forIdentifier: "senate-town-no-network",
                                                                encodedContentRuleList: rules) { list, error in
            if let error { Log.app.error("town page: rule list: \(error.localizedDescription, privacy: .public)") }
            MainActor.assumeIsolated {
                compiled = list
                done(list)
            }
        }
    }
}

extension TownWebPage: WKNavigationDelegate, WKUIDelegate {
    func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction) async -> WKNavigationActionPolicy {
        // The page itself; nothing else.
        action.request.url == page && action.targetFrame?.isMainFrame == true ? .allow : .cancel
    }

    func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration,
                 for action: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? { nil }

    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        // The demo's page is its own shell and never says `ready` to this one.
        if Self.demo != nil { ready = true }
    }

    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        ready = false
        webView.load(URLRequest(url: page))
    }
}

private final class WeakTownHandler: NSObject, WKScriptMessageHandler {
    weak var target: TownWebPage?
    init(_ target: TownWebPage) { self.target = target }

    func userContentController(_ c: WKUserContentController, didReceive message: WKScriptMessage) {
        let body = message.body
        let main = message.frameInfo.isMainFrame
        MainActor.assumeIsolated { target?.handle(body, mainFrame: main) }
    }
}

/// `senate-town://town/<path>`: the web Town's files from the bundle's
/// `town/` folder, and nothing else; the page comes with its policy.
private final class TownScheme: NSObject, WKURLSchemeHandler {
    static let scheme = "senate-town"
    static let page = URL(string: "senate-town://town/index.html")!
    private static let types: [String: String] = [
        "html": "text/html", "js": "text/javascript", "css": "text/css", "png": "image/png", "tmj": "application/json", "json": "application/json", "ndjson": "application/x-ndjson",
    ]
    static let policy = [
        "default-src 'none'", "script-src senate-town:", "style-src senate-town:", "img-src senate-town: data:",
        "connect-src senate-town:", "font-src 'none'", "media-src 'none'", "object-src 'none'", "frame-src 'none'",
        "child-src 'none'", "worker-src 'none'", "manifest-src 'none'", "base-uri 'none'", "form-action 'none'",
        "frame-ancestors 'none'", "require-trusted-types-for 'script'", "trusted-types 'none'",
    ].joined(separator: "; ")

    func webView(_ webView: WKWebView, start task: any WKURLSchemeTask) {
        guard let url = task.request.url, url.host == "town",
              let root = Bundle.main.url(forResource: "town", withExtension: nil)?.standardizedFileURL,
              let type = Self.types[url.pathExtension.lowercased()]
        else { return task.didFailWithError(URLError(.fileDoesNotExist)) }
        let file = root.appendingPathComponent(url.path).standardizedFileURL
        guard file.path.hasPrefix(root.path + "/"), let data = try? Data(contentsOf: file) else {
            return task.didFailWithError(URLError(.fileDoesNotExist))
        }
        var headers = ["Content-Type": type, "X-Content-Type-Options": "nosniff", "Cache-Control": "no-store"]
        if type == "text/html" { headers["Content-Security-Policy"] = Self.policy }
        task.didReceive(HTTPURLResponse(url: url, statusCode: 200, httpVersion: nil, headerFields: headers)!)
        task.didReceive(data)
        task.didFinish()
    }

    func webView(_ webView: WKWebView, stop task: any WKURLSchemeTask) {}
}

/// The web Town in a view.
struct TownWebView: NSViewRepresentable {
    let page: TownWebPage
    func makeNSView(context: Context) -> WKWebView { page.web }
    func updateNSView(_ view: WKWebView, context: Context) {}
}

// MARK: Links

/// Where a link the shell handed the page leads.
enum TownDestination: Hashable {
    case session(String), committee(String), thread(String, UInt64), secretary, history, approvals, inbox
}

/// The canonical `senate://open/v1/…` links (docs/observe.md), for the public
/// shapes this shell projects.
enum TownLink {
    static let desk = "senate://open/v1/desk"
    static let approvals = "senate://open/v1/approvals"
    static let inbox = "senate://open/v1/inbox"
    static let history = "senate://open/v1/history"
    static func committee(_ name: String) -> String { "senate://open/v1/committee/\(encode(name))" }
    static func thread(committee: String, root: UInt64) -> String { "senate://open/v1/thread/\(encode(committee))/\(root)" }
    static func senator(committee: String, alias: String) -> String { "senate://open/v1/senator/\(encode("\(committee)::\(alias)"))" }

    /// Every byte percent-encoded except ASCII letters, digits and `-._~`.
    static func encode(_ value: String) -> String {
        var out = ""
        for byte in value.utf8 {
            let c = Character(UnicodeScalar(byte))
            if byte < 0x80, c.isLetter || c.isNumber || "-._~".contains(c) { out.append(c) } else { out += String(format: "%%%02X", byte) }
        }
        return out
    }
}
