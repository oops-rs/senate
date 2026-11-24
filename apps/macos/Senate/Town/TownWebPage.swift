import AppKit
import SwiftUI
import WebKit
import SenateCore

/// The web Town: the Phaser page built from `apps/macos/TownWeb` (in the
/// bundle as `town/`), fed the town's state and the committees' activity,
/// sending back only what to open. Shown instead of the SpriteKit map
/// while `SENATE_APP_TOWN_WEB=1` (it replaces it once it is the one).
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
/// - The page's script runs in the page's own world (it is the whole page:
///   a game, not a document with content of its own); data goes in as
///   call arguments, never as script. It sends back `ready` and `open`
///   (a session, a thread, a committee, the hall), each checked against
///   what the town shows now and rate-limited before anything happens.
/// - Navigation away and new windows are refused.
@MainActor
final class TownWebPage: NSObject {
    static let enabled = ProcessInfo.processInfo.environment["SENATE_APP_TOWN_WEB"] == "1"
    /// `SENATE_APP_TOWN_DEMO=1` (or `large`): the page plays its scripted town, not the store's.
    static let demo: String? = ProcessInfo.processInfo.environment[TownScript.env].map { $0 == "large" ? "large" : "small" }
    /// The page on screen (snapshots, measurement).
    static weak var shown: TownWebPage?

    let web: WKWebView
    private weak var store: CoreStore?
    private(set) var ready = false
    private var state: TownState?
    /// What the page may name back, from what it was sent.
    private var sessions: Set<String> = []
    private var committees: Set<String> = []
    private var threads: Set<String> = []
    private var opens: [ContinuousClock.Instant] = []
    private var feed: TownActivityFeed?
    private var options: [String: Any] = [:]
    /// The page is shown: the activity feed should run (once the page is ready).
    private var wantsFeed = false

    init(store: CoreStore) {
        self.store = store
        let config = WKWebViewConfiguration()
        config.websiteDataStore = .nonPersistent()
        config.setURLSchemeHandler(TownScheme(), forURLScheme: TownScheme.scheme)
        config.preferences.javaScriptCanOpenWindowsAutomatically = false
        config.preferences.isElementFullscreenEnabled = false
        web = WKWebView(frame: .zero, configuration: config)
        super.init()
        config.userContentController.add(WeakTownHandler(self), contentWorld: .page, name: "town")
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
            web.load(URLRequest(url: TownScheme.page))
        }
    }

    // MARK: Feeding the page

    /// The town as the store has it now (the page works out what changed).
    func show(_ state: TownState) {
        guard Self.demo == nil, state != self.state else { return }
        self.state = state
        sessions = Set(state.senators.values.compactMap(\.sessionId))
        committees = Set(state.committees.map(\.name))
        threads.formUnion(state.committees.flatMap(\.waiting).map(\.key))
        call("window.town.setState(state, [])", ["state": state.json])
    }

    func setOptions(reduceMotion: Bool? = nil, dark: Bool? = nil, visible: Bool? = nil) {
        if let reduceMotion { options["reduceMotion"] = reduceMotion }
        if let dark { options["dark"] = dark }
        if let visible { options["visible"] = visible }
        call("window.town.setOptions(o)", ["o": options])
        if let visible { wantsFeed = visible }
        if visible == true { startFeed() }
        if visible == false { feed?.stop() }
    }

    /// The committees' activity, live, while the page is shown: the entries
    /// as they come (the page maps them itself).
    private func startFeed() {
        // Only once the page can take what arrives: the feed starts at the
        // log's head, so nothing it sends is missed or replayed.
        guard let store, ready, wantsFeed, Self.demo == nil else { return }
        if feed == nil {
            feed = TownActivityFeed(store: store, entries: { [weak self] entries in
                guard let self else { return }
                for e in entries { self.threads.insert("\(e.committee):\(e.thread ?? e.seq)") }
                self.call("window.town.activity(entries)", ["entries": entries.map(\.json)])
            }, deliver: { _ in })
        }
        feed?.start()
    }

    func evaluate(_ js: String, _ args: [String: Any] = [:]) async -> Any? {
        guard ready else { return nil }
        return try? await web.callAsyncJavaScript(js, arguments: args, in: nil, contentWorld: .page)
    }

    private func call(_ js: String, _ args: [String: Any]) {
        guard ready else { return }
        web.callAsyncJavaScript(js, arguments: args, in: nil, in: .page) { result in
            if case let .failure(error) = result {
                Log.app.error("town page: \(error.localizedDescription, privacy: .public)")
            }
        }
    }

    // MARK: What the page sends

    fileprivate func handle(_ body: Any, mainFrame: Bool) {
        guard mainFrame, let body = body as? [String: Any], let action = body["action"] as? String else { return }
        switch action {
        case "ready":
            ready = true
            if !options.isEmpty { call("window.town.setOptions(o)", ["o": options]) }
            if let demo = Self.demo {
                call("window.town.demo(kind)", ["kind": demo])
                return
            }
            if let state {
                self.state = nil
                show(state)
            }
            startFeed()
        case "error":
            let message = (body["message"] as? String).map { String($0.prefix(500)) } ?? ""
            Log.app.error("town page: \(message, privacy: .public)")
            #if DEBUG
            print("town page error: \(message)")
            #endif
            return
        case "room":
            // Into a house or out (the navigation entry comes with the live wiring).
            return
        case "open":
            guard let kind = body["kind"] as? String, let id = body["id"] as? String, id.count <= 300 else { return }
            // At most five a second: a page that floods is ignored.
            let now = ContinuousClock.now
            opens = opens.filter { now - $0 < .seconds(1) } + [now]
            guard opens.count <= 5 else { return }
            open(kind, id)
        default:
            return
        }
    }

    private func open(_ kind: String, _ id: String) {
        guard let store else { return }
        switch kind {
        case "session" where sessions.contains(id):
            store.place = store.secretarySession?.sessionId == id ? .secretary : .session(id)
        case "committee" where committees.contains(id):
            store.place = .committee(id)
        case "thread" where threads.contains(id):
            guard let colon = id.lastIndex(of: ":"), let root = UInt64(id[id.index(after: colon)...]) else { return }
            store.openThread(committee: String(id[..<colon]), root: root)
        case "hall":
            store.place = .secretary
        case "history":
            store.place = .section(.history)
        case "approvals":
            store.place = .section(.approvals)
        case "past" where store.history.row(id) != nil:
            store.place = .past(id)
        default:
            Log.app.info("town page: refused open \(kind, privacy: .public)")
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
        action.request.url == TownScheme.page && action.targetFrame?.isMainFrame == true ? .allow : .cancel
    }

    func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration,
                 for action: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? { nil }

    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        ready = false
        webView.load(URLRequest(url: TownScheme.page))
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
        "html": "text/html", "js": "text/javascript", "css": "text/css", "png": "image/png", "tmj": "application/json", "json": "application/json",
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

// MARK: Plain data for the page

extension TownSenator {
    var json: [String: Any] {
        var o: [String: Any] = ["id": id, "name": name, "runtime": runtime.rawValue, "mood": mood.rawValue, "chair": chair]
        if let sessionId { o["sessionId"] = sessionId }
        if let doing { o["doing"] = doing }
        return o
    }
}

extension TownThreadRef {
    var json: [String: Any] {
        var o: [String: Any] = ["committee": committee, "root": root, "title": title]
        if let asker { o["asker"] = asker }
        return o
    }
}

extension TownState {
    var json: [String: Any] {
        var hallJSON: [String: Any] = ["docket": hall.docket]
        if let s = hall.secretary { hallJSON["secretary"] = s.json }
        var o: [String: Any] = [
            "committees": committees.map { ["name": $0.name, "senators": $0.senators.map(\.json), "waiting": $0.waiting.map(\.json)] },
            "hall": hallJSON,
        ]
        if let person { o["person"] = person }
        return o
    }
}

extension TownActivityEntry {
    var json: [String: Any] {
        var o: [String: Any] = ["committee": committee, "seq": seq, "kind": kind, "from": from, "to": to, "line": line]
        if let onBehalfOf { o["onBehalfOf"] = onBehalfOf }
        if let thread { o["thread"] = thread }
        if let state { o["state"] = state }
        return o
    }
}
