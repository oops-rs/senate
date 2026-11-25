import AppKit
import IOKit.ps
import SwiftUI
import WebKit
import SenateCore

/// The scene host: one installed scene in a web view, given exactly the
/// scene host contract (docs/observe.md, "Scene host contract") over one
/// ordered channel, here the `scene` script handler (in) and
/// `__senateScene(message)` (out). The protocol itself is [`SceneSession`];
/// this is the web view, the Observe feed and the platform around it.
///
/// What a scene gets:
/// - Observe data only through a native client ([`ObserveClient`]) holding
///   this scene's own grant, created when the person allows it (metadata
///   only; this Senate's direct links only if the manifest asks), kept in
///   the Keychain. The page gets the snapshot and frames; never the bearer,
///   never the app's own state.
/// - `status`, `environment` (colour scheme, reduced motion, locale, time
///   zone, visibility, viewport, `focused`, `power`), its own `storage.*`,
///   `open {url}` to the app's one router, and `card.write` for schemas it
///   declared, after the person confirms.
/// - Off screen (another page, the window hidden, occluded or minimised, the
///   displays asleep) the subscription is released; back on screen it starts
///   again from a fresh snapshot.
///
/// What it is kept from, whatever its files say:
/// - Its files come only from its own bundle, through the `senate-scene:`
///   scheme at its own host (its identity), a fixed set of file types, no
///   path out of the bundle and no link followed; the page comes with a
///   strict Content-Security-Policy and Trusted Types.
/// - A content rule list blocks every http(s), ws(s), file, ftp and blob
///   load; if it does not compile the page is not loaded.
/// - Data goes in as call arguments, never as script.
/// - Navigation away from its entry and new windows are refused; camera,
///   microphone and the like are denied; storage is not persistent.
@MainActor
final class SceneWebPage: NSObject {
    /// The page on screen (snapshots, measurement).
    static weak var shown: SceneWebPage?

    let scene: InstalledScene
    let web: WKWebView
    let session: SceneSession
    private weak var store: CoreStore?
    private let page: URL
    private let senateKey: String
    /// When the person last clicked or pressed a key in the page (real user activation, seen natively).
    private var activatedAt: ContinuousClock.Instant?
    private var monitor: Any?
    private var colorScheme = "light"
    private var reduceMotion = false
    private var visible = false
    private var displaysAsleep = false
    private var observers: [(NotificationCenter, NSObjectProtocol)] = []
    /// The live Observe subscription.
    private var client: ObserveClient?
    /// What the scene was sent, also for the accessible list.
    var onSnapshot: ([String: Any]) -> Void = { _ in }
    var onFrame: ([String: Any]) -> Void = { _ in }
    /// This scene has no grant for this Senate yet (or it was revoked): the view offers to create one.
    var onAccessNeeded: (Bool) -> Void = { _ in }
    private(set) var accessNeeded = false {
        didSet { if accessNeeded != oldValue { onAccessNeeded(accessNeeded) } }
    }

    var ready: Bool { session.ready }

    init(scene: InstalledScene, store: CoreStore, storage: SceneStorage) {
        self.scene = scene
        self.store = store
        senateKey = store.state.senates.first?.key ?? "local"
        page = SceneScheme.url(scene: scene.id, path: scene.manifest.entry)
        let config = WKWebViewConfiguration()
        config.websiteDataStore = .nonPersistent()
        config.setURLSchemeHandler(SceneScheme(scene: scene), forURLScheme: SceneScheme.scheme)
        config.preferences.javaScriptCanOpenWindowsAutomatically = false
        config.preferences.isElementFullscreenEnabled = false
        config.preferences.isFraudulentWebsiteWarningEnabled = false
        config.mediaTypesRequiringUserActionForPlayback = .all
        config.allowsAirPlayForMediaPlayback = false
        config.suppressesIncrementalRendering = false
        web = SceneWKWebView(frame: .zero, configuration: config)
        var send: (([String: Any]) -> Void)?
        var open: ((String, Bool) -> Bool)?
        var confirm: ((SceneSession.CardRequest) async -> Bool)?
        var write: ((String) async throws -> Void)?
        session = SceneSession(scene: scene, storage: storage, hooks: .init(
            send: { send?($0) }, open: { open?($0, $1) ?? false },
            confirmCard: { await confirm?($0) ?? false }, writeCard: { try await write?($0) }
        ))
        super.init()
        send = { [weak self] in self?.post($0) }
        open = { [weak self] url, gesture in
            guard let self else { return false }
            defer { activatedAt = nil }
            return self.store?.links.open(url, provenance: gesture ? .gesture : .system) ?? false
        }
        confirm = { [weak self] in await self?.confirmCard($0) ?? false }
        write = { [weak self] document in
            guard let core = self?.store?.core else { throw CoreError.NotStarted }
            try await core.setProfileCard(document: document)
        }
        session.onReady = { [weak self] in
            guard let self else { return }
            client?.stop()
            client = nil
            sentOnScreen = false
            sendEnvironment()
        }
        config.userContentController.add(WeakSceneHandler(self), contentWorld: .page, name: "scene")
        (web as? SceneWKWebView)?.onWindow = { [weak self] in
            // After the move settles (SwiftUI places the view, then the window shows).
            DispatchQueue.main.async { self?.sendEnvironment() }
        }
        web.navigationDelegate = self
        web.uiDelegate = self
        web.allowsBackForwardNavigationGestures = false
        web.allowsMagnification = false
        web.allowsLinkPreview = false
        watchWindow()
        watchActivation()
        Self.noNetwork { [weak self] list in
            guard let self else { return }
            guard let list else {
                Log.app.error("scene page: the no-network rule list did not compile; the page is not loaded")
                return
            }
            web.configuration.userContentController.add(list)
            web.load(URLRequest(url: page))
        }
    }

    /// The page is going away for good: release everything.
    func close() {
        client?.stop()
        client = nil
        if let monitor { NSEvent.removeMonitor(monitor) }
        monitor = nil
        for (center, token) in observers { center.removeObserver(token) }
        observers = []
        session.reset()
        web.configuration.userContentController.removeScriptMessageHandler(forName: "scene", contentWorld: .page)
        web.stopLoading()
    }

    // MARK: Feeding the page

    /// What the scene was last told about being on screen.
    private var sentOnScreen = false

    private func onScreenChanged(_ now: Bool) {
        guard now != sentOnScreen else { return }
        sentOnScreen = now
        if now { subscribe() } else { client?.stop(); client = nil }
    }

    /// Subscribe with this scene's grant for this Senate, once the daemon has
    /// vouched for it (issued to this scene, live, by the installation that
    /// answers now), or say access is needed.
    private var checking = false
    private func subscribe() {
        guard client == nil, !checking, session.ready, sentOnScreen, let core = store?.core else { return }
        guard let grant = SceneAccess.load(senateKey, scene: scene.id) else { return needAccess() }
        checking = true
        Task { [weak self] in
            let endpoint = try? await core.observeEndpoint()
            let grants = try? await core.observeGrants()
            guard let self else { return }
            checking = false
            guard let endpoint, let grants else {
                // No Observe here now (daemon down, or older): try again later.
                session.status("data_unavailable")
                DispatchQueue.main.asyncAfter(deadline: .now() + 5) { [weak self] in self?.subscribe() }
                return
            }
            guard SceneAccess.vouched(grant, grants: grants, installation: endpoint.id, scene: scene),
                  grant.topology == scene.manifest.wantsTopology
            else {
                SceneAccess.forget(senateKey, scene: scene.id)
                return needAccess()
            }
            start(grant, core: core)
        }
    }

    private func needAccess() {
        accessNeeded = true
        session.status("access_needed")
    }

    private func start(_ grant: SceneAccess.Grant, core: any AppCoreProtocol) {
        guard client == nil, sentOnScreen else { return }
        accessNeeded = false
        let client = ObserveClient(grant: grant) { try await core.observeEndpoint() }
        client.onSnapshot = { [weak self] snapshot in
            guard let self, session.snapshot(snapshot) else { return }
            onSnapshot(snapshot)
        }
        client.onFrame = { [weak self] frame in
            guard let self, session.frame(frame) else { return }
            onFrame(frame)
        }
        client.onStatus = { [weak self] status in
            guard let self else { return }
            switch status {
            case .ready: if session.hasSnapshot { session.status("ready") }
            case .unavailable: session.status("data_unavailable")
            case .versionMismatch: session.status("version_mismatch")
            case .accessNeeded:
                self.client = nil
                SceneAccess.forget(senateKey, scene: scene.id)
                needAccess()
            }
        }
        self.client = client
        client.start()
    }

    /// The person allowed this scene to see this Senate: a grant of exactly
    /// what it asks for, then the scene comes alive.
    func grantAccess() async throws {
        guard let core = store?.core else { throw CoreError.NotStarted }
        _ = try await SceneAccess.allow(scene, senate: senateKey, core: core)
        accessNeeded = false
        subscribe()
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
        guard session.ready else { return }
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
        session.environment(env)
    }

    /// The page can be seen: shown, in a window that isn't hidden, occluded or
    /// minimised, with the displays awake. Anything less pauses the scene.
    private var onScreen: Bool {
        guard visible, !displaysAsleep, !web.isHiddenOrHasHiddenAncestor, let window = web.window else { return false }
        return window.isVisible && !window.isMiniaturized && window.occlusionState.contains(.visible)
    }

    /// On battery or in Low Power Mode the scene can draw less (Low Power Mode wins).
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

    /// The person clicked or typed in the page within the last second (seen
    /// by the app, not claimed by the page).
    private var userActivated: Bool {
        guard let at = activatedAt else { return false }
        return ContinuousClock.now - at < .seconds(1)
    }

    /// Watch real clicks and keys in the page's window, over the page.
    private func watchActivation() {
        monitor = NSEvent.addLocalMonitorForEvents(matching: [.leftMouseUp, .keyDown]) { [weak self] event in
            MainActor.assumeIsolated {
                guard let self, let window = self.web.window, event.window === window else { return }
                let point = self.web.convert(event.locationInWindow, from: nil)
                let typing = (window.firstResponder as? NSView)?.isDescendant(of: self.web) == true
                if event.type == .keyDown ? typing : self.web.bounds.contains(point) {
                    self.activatedAt = .now
                }
            }
            return event
        }
    }

    /// The person's yes or no to a card change, asked natively over the page.
    private func confirmCard(_ request: SceneSession.CardRequest) async -> Bool {
        let alert = NSAlert()
        alert.alertStyle = .informational
        alert.messageText = L("scene.card.title", request.scene)
        let value = request.value.count > 600 ? String(request.value.prefix(600)) + "…" : request.value
        alert.informativeText = L("scene.card.body", request.schema, value)
        alert.addButton(withTitle: L("scene.card.decline"))
        let allow = alert.addButton(withTitle: L("scene.card.allow"))
        allow.keyEquivalent = ""
        guard let window = web.window else { return false }
        let response = await alert.beginSheetModal(for: window)
        return response == .alertSecondButtonReturn
    }

    /// Run a bit of script in the page (snapshots, measurement only).
    func evaluate(_ js: String, _ args: [String: Any] = [:]) async -> Any? {
        guard session.ready else { return nil }
        return try? await web.callAsyncJavaScript(js, arguments: args, in: nil, contentWorld: .page)
    }

    private func post(_ message: [String: Any]) {
        web.callAsyncJavaScript("window.__senateScene(m)", arguments: ["m": message], in: nil, in: .page) { result in
            if case let .failure(error) = result {
                Log.app.error("scene page: \(error.localizedDescription, privacy: .public)")
            }
        }
    }

    // MARK: What the page sends

    fileprivate func handle(_ message: WKScriptMessage) {
        // The main frame of this scene's own document only.
        let origin = message.frameInfo.securityOrigin
        guard message.frameInfo.isMainFrame, message.webView === web, origin.protocol == SceneScheme.scheme,
              origin.host == scene.id, message.frameInfo.request.url == page || message.frameInfo.request.url == nil
        else { return }
        session.receive(message.body, activated: userActivated)
    }

    // MARK: The rule list

    private static var compiled: WKContentRuleList?
    private static func noNetwork(_ done: @escaping @MainActor (WKContentRuleList?) -> Void) {
        if let compiled { return done(compiled) }
        let schemes = ["^https?:", "^wss?:", "^file:", "^ftp:", "^blob:"]
        let rules = "[" + schemes.map { #"{"trigger":{"url-filter":""# + $0 + #""},"action":{"type":"block"}}"# }
            .joined(separator: ",") + "]"
        WKContentRuleListStore.default().compileContentRuleList(forIdentifier: "senate-scene-no-network",
                                                                encodedContentRuleList: rules) { list, error in
            if let error { Log.app.error("scene page: rule list: \(error.localizedDescription, privacy: .public)") }
            MainActor.assumeIsolated {
                compiled = list
                done(list)
            }
        }
    }
}

extension SceneWebPage: WKNavigationDelegate, WKUIDelegate {
    func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction) async -> WKNavigationActionPolicy {
        // Links from the page go only through `open {url}` (the original
        // string, validated as sent, with the activation the app saw). Any
        // navigation, `senate:` included, is refused: the page itself, in
        // its main frame, and nothing else.
        action.request.url == page && action.targetFrame?.isMainFrame == true ? .allow : .cancel
    }

    func webView(_ webView: WKWebView, decidePolicyFor response: WKNavigationResponse) async -> WKNavigationResponsePolicy {
        response.canShowMIMEType && response.isForMainFrame ? .allow : .cancel
    }

    func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration,
                 for action: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? { nil }

    func webView(_ webView: WKWebView, requestMediaCapturePermissionFor origin: WKSecurityOrigin, initiatedByFrame frame: WKFrameInfo,
                 type: WKMediaCaptureType) async -> WKPermissionDecision { .deny }

    func webView(_ webView: WKWebView, runOpenPanelWith parameters: WKOpenPanelParameters, initiatedByFrame frame: WKFrameInfo) async -> [URL]? { nil }

    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        session.reset()
        client?.stop()
        client = nil
        webView.load(URLRequest(url: page))
    }

    func webView(_ webView: WKWebView, didStartProvisionalNavigation navigation: WKNavigation!) {
        // A new document: the old one's channel is dropped until it says `ready` again.
        session.reset()
        client?.stop()
        client = nil
        sentOnScreen = false
    }
}

private final class WeakSceneHandler: NSObject, WKScriptMessageHandler {
    weak var target: SceneWebPage?
    init(_ target: SceneWebPage) { self.target = target }

    func userContentController(_ c: WKUserContentController, didReceive message: WKScriptMessage) {
        MainActor.assumeIsolated { target?.handle(message) }
    }
}

/// `senate-scene://<scene id>/<path>`: one installed scene's files, from its
/// bundle folder, and nothing else. Each web view has its own handler bound
/// to its one scene, so a request for another host is refused, and every
/// file is opened without following links and checked again as it is read.
final class SceneScheme: NSObject, WKURLSchemeHandler {
    static let scheme = "senate-scene"

    /// The page's Content-Security-Policy: scripts, styles, images, fonts and
    /// fetches only from the scene's own scheme (images also as `data:`),
    /// nothing else; Trusted Types with no policy, so no markup from strings.
    static let policy = [
        "default-src 'none'", "script-src senate-scene:", "style-src senate-scene:", "img-src senate-scene: data:",
        "font-src senate-scene:", "connect-src senate-scene:", "media-src 'none'", "object-src 'none'", "frame-src 'none'",
        "child-src 'none'", "worker-src 'none'", "manifest-src 'none'", "base-uri 'none'", "form-action 'none'",
        "frame-ancestors 'none'", "require-trusted-types-for 'script'", "trusted-types 'none'",
    ].joined(separator: "; ")

    let scene: InstalledScene

    init(scene: InstalledScene) { self.scene = scene }

    static func url(scene: String, path: String) -> URL {
        URL(string: "\(scheme)://\(scene)/\(path)")!
    }

    /// The file a request names, as bytes and type, or nil.
    nonisolated static func resolve(_ url: URL, scene: String, root: URL) -> (Data, String)? {
        guard url.scheme == scheme, url.host(percentEncoded: true) == scene, url.user == nil, url.port == nil,
              url.query == nil, url.fragment == nil
        else { return nil }
        // The raw path: no escapes at all are accepted (scene paths never need one).
        let raw = url.path(percentEncoded: true)
        guard raw.hasPrefix("/"), !raw.contains("%") else { return nil }
        let path = String(raw.dropFirst())
        guard ScenePath.validRelative(path), let type = SceneBundle.contentType(path),
              let data = try? SceneBundle.read(root, path)
        else { return nil }
        return (data, type)
    }

    func webView(_ webView: WKWebView, start task: any WKURLSchemeTask) {
        guard let url = task.request.url, let (data, type) = Self.resolve(url, scene: scene.id, root: scene.root) else {
            return task.didFailWithError(URLError(.fileDoesNotExist))
        }
        var headers = ["Content-Type": type, "X-Content-Type-Options": "nosniff", "Cache-Control": "no-store",
                       "Content-Length": String(data.count)]
        if type == "text/html" { headers["Content-Security-Policy"] = Self.policy }
        task.didReceive(HTTPURLResponse(url: url, statusCode: 200, httpVersion: nil, headerFields: headers)!)
        task.didReceive(data)
        task.didFinish()
    }

    func webView(_ webView: WKWebView, stop task: any WKURLSchemeTask) {}
}

/// A scene's web view, with no context menu (its Look Up, Translate,
/// Search With and Services items would hand the scene's text to other apps
/// or the web).
final class SceneWKWebView: WKWebView {
    /// Joined or left a window (or the window changed): visibility is checked again.
    var onWindow: () -> Void = {}

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        onWindow()
    }

    override func viewDidHide() {
        super.viewDidHide()
        onWindow()
    }

    override func viewDidUnhide() {
        super.viewDidUnhide()
        onWindow()
    }

    override func willOpenMenu(_ menu: NSMenu, with event: NSEvent) {
        menu.removeAllItems()
    }

    override func menu(for event: NSEvent) -> NSMenu? { nil }
}

/// A scene's web view in a view.
struct SceneWebView: NSViewRepresentable {
    let page: SceneWebPage
    func makeNSView(context: Context) -> WKWebView { page.web }
    func updateNSView(_ view: WKWebView, context: Context) {}
}
