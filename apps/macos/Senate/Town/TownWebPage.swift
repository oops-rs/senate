import AppKit
import IOKit.ps
import SwiftUI
import WebKit
import SenateCore

/// The web Town: the scene built from the top-level `town/` package (in the
/// bundle as `town/`), hosted under the scene host contract
/// (docs/observe.md, "Scene host contract"): one ordered channel of JSON
/// messages `{host_version: 1, type, request_id?, data}`, here bound to the
/// `scene` script handler (in) and `__senateScene(message)` (out). The
/// Town tab (`TownPage`) shows it.
///
/// This is the shell; the page is content and knows nothing of the app:
/// - Data comes only from the Senate's public Observe API, through a native
///   client ([`ObserveClient`]) holding a grant the person creates with one
///   click (metadata only, every committee, this Senate's direct links),
///   kept in the Keychain. The page gets the snapshot and frames as Observe
///   sends them; never the bearer, never the app's own state.
/// - It answers `ready`, sends `status` and `environment` (colour scheme,
///   reduced motion, locale, time zone, visibility, viewport, and the two
///   additions: `focused`, `power`), keeps `storage.*` per scene (16 KiB a
///   value, 64 KiB in all) and declines `card.write` for now.
/// - Off screen (another page, the window hidden, occluded or minimised, the
///   displays asleep) the subscription is released; back on screen it starts
///   again from a fresh snapshot.
/// - Links: `open {url}` and a user-activated main-frame `senate:`
///   navigation both go to the app's one router (`store.links`), unchanged.
///
/// Safety, as the conversation page:
/// - Everything comes from the bundle's `town/` folder through the
///   `senate-town:` scheme, a fixed set of file types, no path out of the
///   folder; the page comes with its Content-Security-Policy: scripts,
///   styles and images only from that scheme (images also as `data:`),
///   nothing else.
/// - A content rule list blocks every http(s), ws(s), file, ftp and blob
///   load whatever the page says; if it does not compile the page is not
///   loaded.
/// - Data goes in as call arguments, never as script.
/// - Navigation away and new windows are refused.
@MainActor
final class TownWebPage: NSObject {
    /// `SENATE_APP_TOWN_DEMO=1` (or `large`): start on the demo (snapshots, trying the scene).
    static let demoEnv: String? = ProcessInfo.processInfo.environment["SENATE_APP_TOWN_DEMO"].map { $0 == "large" ? "large" : "small" }
    /// For the demo: `SENATE_APP_TOWN_TIME` (dawn, day, dusk, night), `SENATE_APP_TOWN_WEATHER`
    /// (clear, cloudy, rain, windy, fog) and `SENATE_APP_TOWN_SEASON` (spring, summer, autumn, winter)
    /// fix the time of day, the weather and the season instead of the clock's and the date's.
    static let demoLook: String = {
        let env = ProcessInfo.processInfo.environment
        let allowed: [(String, String, Set<String>)] = [
            ("SENATE_APP_TOWN_TIME", "time", ["dawn", "day", "dusk", "night"]),
            ("SENATE_APP_TOWN_WEATHER", "weather", ["clear", "cloudy", "rain", "windy", "fog"]),
            ("SENATE_APP_TOWN_SEASON", "season", ["spring", "summer", "autumn", "winter"]),
        ]
        return allowed.compactMap { key, name, values in env[key].flatMap { values.contains($0) ? "\(name)=\($0)" : nil } }.joined(separator: "&")
    }()
    /// The page on screen (snapshots, measurement).
    static weak var shown: TownWebPage?

    let web: WKWebView
    private weak var store: CoreStore?
    private let page: URL
    /// The page said `ready`.
    private(set) var ready = false
    private var opens: [ContinuousClock.Instant] = []
    /// The scene's own settings (`storage.*`), kept across launches for this scene.
    private var storage: [String: String] = UserDefaults.standard.dictionary(forKey: TownWebPage.storageKey) as? [String: String] ?? [:] {
        didSet { UserDefaults.standard.set(storage, forKey: TownWebPage.storageKey) }
    }
    private static let storageKey = "town.scene.storage"
    private var colorScheme = "light"
    private var reduceMotion = false
    private var visible = false
    private var displaysAsleep = false
    private var observers: [(NotificationCenter, NSObjectProtocol)] = []
    /// The live Observe subscription (not in the demo).
    private var client: ObserveClient?
    /// Playing the demo recording (`small` or `large`) instead of this Senate.
    private(set) var demo: String?
    /// Everything the scene is sent, also for the accessible list.
    var onSnapshot: ([String: Any]) -> Void = { _ in }
    var onFrame: ([String: Any]) -> Void = { _ in }
    /// Town has no grant for this Senate yet (or it was revoked): the view offers to create one.
    var onAccessNeeded: (Bool) -> Void = { _ in }
    private(set) var accessNeeded = false {
        didSet { if accessNeeded != oldValue { onAccessNeeded(accessNeeded) } }
    }

    init(store: CoreStore) {
        self.store = store
        demo = Self.demoEnv
        page = Self.demoEnv != nil && !Self.demoLook.isEmpty ? URL(string: "senate-town://town/index.html?\(Self.demoLook)")! : TownScheme.page
        let config = WKWebViewConfiguration()
        config.websiteDataStore = .nonPersistent()
        config.setURLSchemeHandler(TownScheme(), forURLScheme: TownScheme.scheme)
        config.preferences.javaScriptCanOpenWindowsAutomatically = false
        config.preferences.isElementFullscreenEnabled = false
        web = WKWebView(frame: .zero, configuration: config)
        super.init()
        config.userContentController.add(WeakTownHandler(self), contentWorld: .page, name: "scene")
        web.navigationDelegate = self
        web.uiDelegate = self
        web.allowsBackForwardNavigationGestures = false
        web.allowsMagnification = false
        web.allowsLinkPreview = false
        watchWindow()
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

    /// Which Senate the grant is kept for.
    private var senateKey: String { store?.state.senates.first?.key ?? "local" }

    /// What the scene was last told about being on screen.
    private var sentOnScreen = false

    /// On screen or not: off screen the demo and the live subscription stop
    /// (nothing runs in the page); back on screen they start again from a
    /// fresh snapshot.
    private func onScreenChanged(_ now: Bool) {
        guard now != sentOnScreen else { return }
        sentOnScreen = now
        if let demo {
            demoTask?.cancel()
            demoTask = nil
            if now { startDemo(demo) }
            return
        }
        if now { subscribe() } else { client?.stop(); client = nil }
    }

    /// Subscribe with the grant kept for this Senate, or say access is needed.
    private func subscribe() {
        guard client == nil, ready, sentOnScreen, demo == nil else { return }
        guard let grant = ObserveAccess.load(senateKey) else {
            accessNeeded = true
            send("status", ["state": "access_needed"])
            return
        }
        accessNeeded = false
        let client = ObserveClient(grant: grant)
        client.onSnapshot = { [weak self] in self?.deliver(snapshot: $0) }
        client.onFrame = { [weak self] in self?.deliver(frame: $0) }
        client.onStatus = { [weak self] status in
            guard let self else { return }
            switch status {
            case .ready: send("status", ["state": "ready"])
            case .unavailable: send("status", ["state": "data_unavailable"])
            case .versionMismatch: send("status", ["state": "version_mismatch"])
            case .accessNeeded:
                ObserveAccess.forget(senateKey)
                self.client = nil
                accessNeeded = true
                send("status", ["state": "access_needed"])
            }
        }
        self.client = client
        client.start()
    }

    /// One click: a metadata grant for Town (every committee, this Senate's
    /// direct links), created through the person-only path and kept in the
    /// Keychain. Then the town comes alive.
    func grantAccess() async throws {
        guard let core = store?.core else { throw CoreError.NotStarted }
        let issued = try await core.observeGrant(name: "Town")
        ObserveAccess.save(.init(url: issued.url, token: issued.token, id: issued.id, expiresAt: issued.expiresAt), senateKey)
        accessNeeded = false
        watchLive()
    }

    /// Drop the grant kept for this Senate (the snapshot hook cleans up after itself).
    func forgetAccess() {
        client?.stop()
        client = nil
        ObserveAccess.forget(senateKey)
    }

    func setOptions(reduceMotion: Bool? = nil, dark: Bool? = nil, visible: Bool? = nil) {
        if let reduceMotion { self.reduceMotion = reduceMotion }
        if let dark { colorScheme = dark ? "dark" : "light" }
        if let visible { self.visible = visible }
        sendEnvironment()
        // Shown again: the web view joins its window a moment after the page appears.
        if visible == true {
            for delay in [0.3, 1.0] {
                DispatchQueue.main.asyncAfter(deadline: .now() + delay) { [weak self] in self?.sendEnvironment() }
            }
        }
    }

    private func sendEnvironment() {
        guard ready else { return }
        let size = web.bounds.size
        defer { onScreenChanged(onScreen) }
        var env: [String: Any] = [
            "color_scheme": colorScheme, "reduced_motion": reduceMotion,
            "locale": Locale.current.identifier(.bcp47), "time_zone": TimeZone.current.identifier, "visible": onScreen,
            "viewport": ["width": size.width, "height": size.height],
            "safe_area": ["top": 0, "right": 0, "bottom": 0, "left": 0],
            "power": Self.power,
        ]
        // Only with a window is there a reliable answer; otherwise the field is left out.
        if let window = web.window { env["focused"] = window.isKeyWindow }
        send("environment", env)
    }

    /// The page can be seen: shown, in a window that isn't hidden, occluded or
    /// minimised, with the displays awake. Anything less pauses the scene.
    private var onScreen: Bool {
        guard visible, !displaysAsleep, let window = web.window else { return false }
        return window.isVisible && !window.isMiniaturized && window.occlusionState.contains(.visible)
    }

    /// On battery or in Low Power Mode the scene draws less (Low Power Mode wins).
    private static var power: String {
        if ProcessInfo.processInfo.isLowPowerModeEnabled { return "low_power" }
        let info = IOPSCopyPowerSourcesInfo()?.takeRetainedValue()
        let type = info.flatMap { IOPSGetProvidingPowerSourceType($0)?.takeUnretainedValue() as String? }
        return type == kIOPMBatteryPowerKey ? "battery" : "ac"
    }

    /// Key, occlusion, minimising, display sleep and power changes go to the scene as they happen.
    private func watchWindow() {
        let center = NotificationCenter.default, workspace = NSWorkspace.shared.notificationCenter
        let windowNotes: [Notification.Name] = [
            NSWindow.didBecomeKeyNotification, NSWindow.didResignKeyNotification, NSWindow.didChangeOcclusionStateNotification,
            NSWindow.didMiniaturizeNotification, NSWindow.didDeminiaturizeNotification,
        ]
        for name in windowNotes {
            observers.append((center, center.addObserver(forName: name, object: nil, queue: .main) { [weak self] note in
                let window = ObjectIdentifier(note.object as AnyObject)
                MainActor.assumeIsolated {
                    guard let self, let mine = self.web.window, ObjectIdentifier(mine) == window else { return }
                    self.sendEnvironment()
                }
            }))
        }
        observers.append((center, center.addObserver(forName: .NSProcessInfoPowerStateDidChange, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.sendEnvironment() }
        }))
        for (name, asleep) in [(NSWorkspace.screensDidSleepNotification, true), (NSWorkspace.screensDidWakeNotification, false)] {
            observers.append((workspace, workspace.addObserver(forName: name, object: nil, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated {
                    self?.displaysAsleep = asleep
                    self?.sendEnvironment()
                }
            }))
        }
    }

    /// The demo: the scene's own recording (observe recording format: a header,
    /// then rows `{offset_ms, type, data}`), replayed in time and on a loop,
    /// exactly as live frames would arrive.
    private var demoTask: Task<Void, Never>?
    private func startDemo(_ kind: String) {
        guard demoTask == nil,
              let root = Bundle.main.url(forResource: "town", withExtension: nil),
              let text = try? String(contentsOf: root.appendingPathComponent("recordings/demo\(kind == "large" ? "-large" : "").ndjson"), encoding: .utf8)
        else { return }
        let rows = text.split(separator: "\n").dropFirst().compactMap { line in
            (try? JSONSerialization.jsonObject(with: Data(line.utf8))) as? [String: Any]
        }
        send("status", ["state": "ready"])
        demoTask = Task { [weak self] in
            while !Task.isCancelled {
                let start = ContinuousClock.now
                for row in rows {
                    let at = Duration.milliseconds((row["offset_ms"] as? NSNumber)?.int64Value ?? 0)
                    let wait = at - (ContinuousClock.now - start)
                    if wait > .zero { try? await Task.sleep(for: wait) }
                    if Task.isCancelled { return }
                    guard let self else { return }
                    switch row["type"] as? String {
                    case "snapshot": if let data = row["data"] as? [String: Any] { self.deliver(snapshot: data) }
                    case "frame": if let data = row["data"] as? [String: Any] { self.deliver(frame: data) }
                    default: break
                    }
                }
            }
        }
    }

    private func deliver(snapshot: [String: Any]) {
        onSnapshot(snapshot)
        send("observe.snapshot", ["snapshot": snapshot])
    }

    private func deliver(frame: [String: Any]) {
        onFrame(frame)
        send("observe.event", ["frame": frame])
    }

    /// The person chose to watch the demo (no access yet, or a Senate without Observe).
    func watchDemo(_ kind: String = "small") {
        client?.stop()
        client = nil
        demo = kind
        accessNeeded = false
        guard ready, sentOnScreen else { return }
        demoTask?.cancel()
        demoTask = nil
        startDemo(kind)
    }

    /// Back from the demo to this Senate.
    func watchLive() {
        demoTask?.cancel()
        demoTask = nil
        demo = nil
        subscribe()
    }

    func evaluate(_ js: String, _ args: [String: Any] = [:]) async -> Any? {
        guard ready else { return nil }
        return try? await web.callAsyncJavaScript(js, arguments: args, in: nil, contentWorld: .page)
    }

    /// One message to the scene.
    private func send(_ type: String, _ data: [String: Any], request: String? = nil) {
        guard ready else { return }
        var message: [String: Any] = ["host_version": 1, "type": type, "data": data]
        if let request { message["request_id"] = request }
        web.callAsyncJavaScript("window.__senateScene(m)", arguments: ["m": message], in: nil, in: .page) { result in
            if case let .failure(error) = result {
                Log.app.error("town page: \(error.localizedDescription, privacy: .public)")
            }
        }
    }

    // MARK: What the page sends

    fileprivate func handle(_ body: Any, mainFrame: Bool) {
        guard mainFrame, let body = body as? [String: Any], body["host_version"] as? Int == 1,
              let type = body["type"] as? String else { return }
        let data = body["data"] as? [String: Any] ?? [:]
        let request = (body["request_id"] as? String).flatMap { $0.count <= 64 ? $0 : nil }
        switch type {
        case "ready":
            // A (re)loaded page: it knows nothing yet.
            guard (data["supported_host_versions"] as? [Int])?.contains(1) == true else {
                if let request { send("result", ["ok": false, "code": "unsupported"], request: request) }
                return
            }
            ready = true
            sentOnScreen = false
            demoTask?.cancel()
            demoTask = nil
            client?.stop()
            client = nil
            if let request { send("result", ["ok": true, "value": ["version": 1, "capabilities": ["open", "storage"]]], request: request) }
            send("status", ["state": "data_unavailable"])
            // The demo or the live subscription starts when (and while) the scene is on screen.
            sendEnvironment()
        case "open":
            guard let url = data["url"] as? String, url.utf8.count <= 2048 else {
                if let request { send("result", ["ok": false, "code": "invalid_request"], request: request) }
                return
            }
            // At most five a second: a page that floods is ignored.
            let now = ContinuousClock.now
            opens = opens.filter { now - $0 < .seconds(1) } + [now]
            guard opens.count <= 5 else {
                if let request { send("result", ["ok": false, "code": "quota"], request: request) }
                return
            }
            // The app's one router validates and routes; a message carries no proof of a click.
            let accepted = store?.links.open(url, provenance: .system) ?? false
            if let request { send("result", accepted ? ["ok": true] : ["ok": false, "code": "invalid_request"], request: request) }
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
        default:
            // card.write and anything else: declined.
            if let request { send("result", ["ok": false, "code": "unsupported"], request: request) }
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
        // A clicked `senate:` link: cancelled here, handed to the app's one router.
        if let url = action.request.url, url.scheme == "senate", action.targetFrame?.isMainFrame == true,
           action.navigationType == .linkActivated {
            store?.links.open(url: url, provenance: .gesture)
            return .cancel
        }
        // The page itself; nothing else.
        return action.request.url == page && action.targetFrame?.isMainFrame == true ? .allow : .cancel
    }

    func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration,
                 for action: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? { nil }

    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        ready = false
        client?.stop()
        client = nil
        demoTask?.cancel()
        demoTask = nil
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
        "html": "text/html", "js": "text/javascript", "css": "text/css", "json": "application/json",
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
