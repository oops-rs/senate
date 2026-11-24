import AppKit
import SenateCore
import SwiftUI
import WebKit

/// What the conversation page shows: a session's timeline or a thread.
enum ConversationSource: Hashable {
    case session(String)
    case thread(committee: String, root: UInt64)
    #if DEBUG
    /// Tests: these messages, as an agent's replies.
    case fixture([String])
    /// Tests: the rows set under this name (`fixtureRows`), as a session's.
    case fixtureRows(String)
    #endif
}

/// The conversation, as one web page in one `WKWebView` kept for the app's
/// life and shown by whichever page needs it (a session, a thread); the
/// header, the prompt card and the composer around it stay native.
///
/// What it draws: rows the app sends (what was typed, the agent's replies,
/// tool calls, to-dos, Senate messages, a thread's events). A message's
/// Markdown is parsed and rendered in the core (`MarkdownStore`,
/// senate-markdown): the page gets its blocks, then diffs naming the blocks
/// that changed, and each block's HTML (a fixed set of tags and classes,
/// text escaped) when it asks, near the viewport or in idle time.
///
/// Safety:
/// - The page, its stylesheets and the one script come only from the app
///   bundle, through the `senate-app:` scheme, which serves five known
///   files and the Content-Security-Policy as a response header: nothing
///   else loads (no network, images, fonts, frames, forms, workers) and
///   the page's own world runs no script at all.
/// - The script runs in a content world of its own ("senate"); the message
///   handler exists only there, so nothing in the page's world can call
///   the app or the bridge's functions. Data goes in as call arguments,
///   never as script.
/// - Where WebKit has Trusted Types, the one HTML sink (the renderer's
///   output) goes through one named policy, and the page requires it.
/// - A content rule list blocks every http(s), ws(s), file, ftp, data and
///   blob load, whatever the page says; if it does not compile, the page
///   is not loaded.
/// - Navigation away from the page, new windows, downloads and drops are
///   refused; the context menu keeps Copy, Look Up, Translate and Speech.
/// - The page sends back a fixed set of actions, each checked (shape, the
///   rows shown now, a rate) before anything happens; links open in the
///   browser only for http, https and mailto.
/// - The website data store is not persistent; if the page's process
///   ends, the page is loaded again and sent everything.
@MainActor
final class ConversationPage: NSObject {
    let web: PageWebView
    /// The view showing the web view now.
    weak var host: PageHost?
    private let bridge = WKContentWorld.world(name: "senate")
    private weak var store: CoreStore?
    private(set) var source: ConversationSource?
    private var ready = false

    /// Messages' Markdown, parsed and rendered in the core.
    nonisolated static let markdown: MarkdownStore = {
        let store = MarkdownStore()
        Task.detached(priority: .utility) { store.warm() }
        return store
    }()
    private static let worker = MarkdownWorker()

    /// What the page shows now: rows by id, in order, and the state line.
    private var sent: [UInt64: any Equatable] = [:]
    private var order: [UInt64] = []
    private var sentState: [String: Bool] = [:]
    /// What the page may name back: message keys, Senate cards' threads.
    private var keys: Set<String> = []
    private var cards: Set<String> = []
    /// A thread shows its latest events, a page more at a time.
    private var shownEvents = 300
    private static let eventPage = 300
    /// Message keys by source, the latest few sources kept in the core.
    private var keysBySource: [ConversationSource: Set<String>] = [:]
    private var recentSources: [ConversationSource] = []

    #if DEBUG
    /// Tests: rows to show under a name (`ConversationSource.fixtureRows`).
    static var fixtureRows: [String: [ConversationRow]] = [:]
    #endif

    /// The find bar's state, which its view reads.
    let find = PageFind()

    init(store: CoreStore?) {
        self.store = store
        let config = WKWebViewConfiguration()
        config.websiteDataStore = .nonPersistent()
        config.setURLSchemeHandler(BundleScheme(), forURLScheme: BundleScheme.scheme)
        config.preferences.javaScriptCanOpenWindowsAutomatically = false
        config.preferences.isElementFullscreenEnabled = false
        config.defaultWebpagePreferences.allowsContentJavaScript = true
        web = PageWebView(frame: .zero, configuration: config)
        super.init()
        config.userContentController.add(WeakHandler(self), contentWorld: bridge, name: "senate")
        if let url = Bundle.main.url(forResource: "conversation", withExtension: "js"),
           let source = try? String(contentsOf: url, encoding: .utf8) {
            config.userContentController.addUserScript(
                WKUserScript(source: source, injectionTime: .atDocumentEnd, forMainFrameOnly: true, in: bridge)
            )
        }
        web.page = self
        web.navigationDelegate = self
        web.uiDelegate = self
        web.underPageBackgroundColor = .clear
        web.setValue(false, forKey: "drawsBackground")
        web.allowsBackForwardNavigationGestures = false
        web.allowsMagnification = false
        web.allowsLinkPreview = false
        load()
        observe()
    }

    private func load() {
        Self.noNetwork { [weak self] list in
            guard let self else { return }
            guard let list else {
                Log.app.error("conversation page: the no-network rule list did not compile; the page is not loaded")
                return
            }
            let rules = web.configuration.userContentController
            rules.removeAllContentRuleLists()
            rules.add(list)
            web.load(URLRequest(url: BundleScheme.page))
        }
    }

    // MARK: What to show

    /// Show `source`: the page starts again when it changes.
    func show(_ source: ConversationSource) {
        guard source != self.source else { return }
        self.source = source
        sent = [:]
        order = []
        keys = []
        cards = []
        shownEvents = Self.eventPage
        if find.visible { closeFind() }
        remember(source)
        push()
        track()
    }

    #if DEBUG
    /// Tests: send the page what changed in the fixture rows, as a
    /// session's timeline change would.
    func pushFixture() { push() }
    #endif

    /// Keep the core's documents for the last few sources, not more.
    private func remember(_ source: ConversationSource) {
        recentSources.removeAll { $0 == source }
        recentSources.append(source)
        while recentSources.count > 4 {
            let old = recentSources.removeFirst()
            if let gone = keysBySource.removeValue(forKey: old) { Self.markdown.forget(keys: Array(gone)) }
        }
    }

    /// Follow what the source reads from: SwiftUI does not tell a view
    /// whose inputs are the same objects that their contents changed.
    private var generation = 0
    private func track() {
        generation += 1
        let mine = generation
        guard let store, let source else { return }
        withObservationTracking {
            switch source {
            case let .session(id):
                if let t = store.timeline, t.sessionId == id {
                    _ = (t.rows, t.loading, t.hasOlder, t.loadingOlder, t.working, t.available)
                }
            case .thread:
                _ = (store.thread, store.threadError)
            #if DEBUG
            case .fixture, .fixtureRows:
                break
            #endif
            }
        } onChange: { [weak self] in
            Task { @MainActor in
                guard let self, self.generation == mine else { return }
                self.push()
                self.track()
            }
        }
    }

    /// One row: its id, what it is now (to tell a change), and its data.
    private struct Row {
        let id: UInt64
        let value: any Equatable
        let encode: () -> [String: Any]
    }

    private func rows() -> (rows: [Row], state: [String: Bool])? {
        guard let source else { return nil }
        switch source {
        case let .session(id):
            guard let t = store?.timeline, t.sessionId == id else { return ([], [:]) }
            let rows = t.loading ? [] : t.rows
            return (rows.map { row in Row(id: row.id, value: row) { [unowned self] in encode(row, session: id, committee: t.committee) } },
                    ["hasOlder": t.hasOlder, "loadingOlder": t.loadingOlder, "working": t.working])
        case let .thread(committee, root):
            guard let view = store?.thread, view.committee == committee, view.root == root else { return ([], [:]) }
            let events = view.events.suffix(shownEvents)
            return (events.map { event in Row(id: event.seq, value: event) { [unowned self] in encode(event, committee: committee, root: root) } },
                    ["hasOlder": view.events.count > shownEvents, "loadingOlder": false, "working": false])
        #if DEBUG
        case let .fixtureRows(name):
            let rows = Self.fixtureRows[name] ?? []
            return (rows.map { row in Row(id: row.id, value: row) { [unowned self] in encode(row, session: "fixture", committee: nil) } },
                    ["hasOlder": false, "loadingOlder": false, "working": false])
        case let .fixture(texts):
            return (texts.enumerated().map { i, text in
                Row(id: UInt64(i + 1), value: text) { [unowned self] in
                    ["id": i + 1, "kind": "assistant", "thinking": [String](), "md": markdown("f/\(i)", text)]
                }
            }, ["hasOlder": false, "loadingOlder": false, "working": false])
        #endif
        }
    }

    /// Send the page what changed since it last heard: everything at first;
    /// rows read above (prepend); changed rows and rows after (upsert); else
    /// everything again.
    private func push() {
        guard ready, let (rows, state) = rows() else { return }
        let ids = rows.map(\.id)
        let changed = { (r: Row) in self.sent[r.id].map { !Self.same($0, r.value) } ?? false }
        var op: [String: Any]
        if order.isEmpty || ids.isEmpty {
            op = ["type": "reset", "rows": rows.map { $0.encode() }]
            op["first"] = firstScreen()
        } else if let (gone, at) = Self.prepended(order, ids) {
            // Older rows above; the page's first row or two may have joined
            // a new one among them (a run of commands that now starts in
            // the older page): those go, the rest stay where they are.
            let kept = order.count - gone
            let added = rows.suffix(ids.count - at - kept)
            op = ["type": "prepend", "rows": rows[..<at].map { $0.encode() },
                  "remove": order.prefix(gone).map { String($0) },
                  "changed": (rows[at...].filter(changed) + added).map { $0.encode() }]
        } else if ids.count >= order.count, Array(ids.prefix(order.count)) == order {
            let all = rows.filter(changed) + rows.suffix(ids.count - order.count)
            if all.isEmpty && state == sentState { return }
            op = ["type": "upsert", "rows": all.map { $0.encode() }]
        } else {
            op = ["type": "reset", "rows": rows.map { $0.encode() }]
            op["first"] = firstScreen()
        }
        op["state"] = state
        if op["type"] as? String == "reset" { Perf.mark("reset encoded (\(rows.count) rows)") }
        sent = Dictionary(uniqueKeysWithValues: rows.map { ($0.id, $0.value) })
        order = ids
        sentState = state
        call("window.senate.apply(op)", ["op": op])
    }

    /// Whether `ids` is `order` with rows read above it: the page's rows
    /// from the `gone`-th on (none or the first one or two absorbed into a
    /// row above) follow, in order, at `at`.
    static func prepended(_ order: [UInt64], _ ids: [UInt64]) -> (gone: Int, at: Int)? {
        for gone in 0..<min(3, order.count) {
            let kept = order[gone...]
            guard let first = kept.first, let at = ids.firstIndex(of: first), at > 0,
                  ids[at...].prefix(kept.count).elementsEqual(kept) else { continue }
            // What went is no longer a row of its own.
            guard order.prefix(gone).allSatisfy({ !ids.contains($0) }) else { return nil }
            return (gone, at)
        }
        return nil
    }

    private static func same(_ a: any Equatable, _ b: any Equatable) -> Bool {
        func equal<T: Equatable>(_ a: T, _ b: any Equatable) -> Bool { (b as? T) == a }
        return equal(a, b)
    }

    // MARK: Rows as the page reads them

    private func markdown(_ key: String, _ text: String) -> [String: Any] {
        keys.insert(key)
        if let source { keysBySource[source, default: []].insert(key) }
        return Self.encode(key: key, Self.markdown.update(key: key, text: text))
    }

    private func encode(_ row: ConversationRow, session: String, committee: String?) -> [String: Any] {
        let names = { (text: String) in Mentions.shared.plain(text, committee: committee) }
        var out: [String: Any] = ["id": row.id, "thinking": row.thinking]
        switch row.kind {
        case let .user(text): out["kind"] = "user"; out["text"] = text
        case let .assistant(text):
            out["kind"] = "assistant"
            out["md"] = markdown("s/\(session)/\(row.id)", names(text))
        case let .tools(members):
            out["kind"] = "group"
            out["members"] = members.map { encode($0, session: session, committee: committee) }
        case let .tool(call):
            out["kind"] = "tool"
            var tool: [String: Any] = ["callId": call.callId, "name": call.name, "kind": call.kind,
                                       "status": call.status, "title": call.title]
            if let o = call.output { tool["output"] = o }
            if let d = call.diff { tool["diff"] = d }
            if let e = call.exitCode { tool["exit"] = e }
            if let m = call.mcp { tool["mcp"] = m }
            out["tool"] = tool
        case let .todo(items):
            out["kind"] = "todo"
            out["todo"] = items.map { ["text": $0.text, "status": $0.status] }
        case let .notice(level, text): out["kind"] = "notice"; out["text"] = text; out["level"] = level
        case let .senate(m):
            out["kind"] = "senate"
            // Its body as Markdown, as every other message: the same
            // blocks, rendered lazily, links only through the page's handler.
            var s: [String: Any] = [:]
            out["md"] = markdown("s/\(session)/\(row.id)", names(m.text))
            if let f = m.from { s["from"] = names(f) }
            if let k = m.kindLabel { s["kindLabel"] = k }
            if let c = m.committee { s["committee"] = c }
            if let th = m.thread {
                s["thread"] = th
                if let c = m.committee { cards.insert("\(c)#\(th)") }
            }
            out["senate"] = s
        }
        return out
    }

    private func encode(_ event: ThreadEvent, committee: String, root: UInt64) -> [String: Any] {
        // A task status: one activity line, its checklist and links.
        if let s = event.status {
            return [
                "id": event.seq, "kind": "status",
                "by": Mentions.shared.plain(event.by, committee: committee),
                "when": event.at.formatted(.relative(presentation: .named)),
                "status": [
                    "state": s.state, "line": s.line,
                    "steps": s.steps.map { ["label": $0.label, "done": $0.done] },
                    "links": s.links.map { ["label": $0.label, "url": $0.url] },
                ] as [String: Any],
            ]
        }
        return [
            "id": event.seq, "kind": "event", "mine": event.mine,
            "by": Mentions.shared.plain(event.by, committee: committee),
            "kindLabel": event.kindLabel,
            "when": event.at.formatted(.relative(presentation: .named)),
            "md": markdown("t/\(committee)/\(root)/\(event.seq)", Mentions.shared.plain(event.body, committee: committee)),
        ]
    }

    /// What a reset shows first (it opens at the foot): the last messages'
    /// blocks rendered, in the same call, so the first screen takes one
    /// trip rather than two.
    private func firstScreen() -> [[String: Any]] {
        guard let source else { return [] }
        let prefix: String
        let ids: [UInt64]
        switch source {
        case let .session(id):
            prefix = "s/\(id)/"
            ids = (store?.timeline?.rows ?? []).suffix(12).compactMap { row in
                if case .assistant = row.kind { return row.id }
                return nil
            }
        case let .thread(committee, root):
            prefix = "t/\(committee)/\(root)/"
            ids = (store?.thread?.events ?? []).suffix(3).map(\.seq)
        #if DEBUG
        case .fixture, .fixtureRows:
            return []
        #endif
        }
        return ids.suffix(3).compactMap { id in
            let key = prefix + String(id)
            guard keys.contains(key) else { return nil }
            let blocks = Self.markdown.blocks(key: key)
            return ["key": key, "r": Self.result(Self.markdown.render(key: key, ids: blocks, full: true), full: true)]
        }
    }

    private static func encode(key: String, _ u: MdUpdate) -> [String: Any] {
        ["key": key,
         "blocks": u.blocks.map { b -> [String: Any] in
             var o: [String: Any] = ["id": b.id, "kind": b.kind, "len": b.sourceLen, "lines": b.lines]
             if let l = b.language { o["lang"] = l }
             return o
         },
         "replaced": u.replaced, "appended": u.appended, "inserted": u.inserted, "removed": u.removed,
         "micros": u.micros]
    }

    private static func result(_ r: MdRender, full: Bool) -> [String: Any] {
        let blocks = r.blocks.map { b -> [String: Any] in
            var o: [String: Any] = ["id": b.id]
            if let h = b.html { o["html"] = h }
            if !b.code.isEmpty {
                o["code"] = b.code.map { ["index": $0.index, "from": $0.fromLine, "lines": $0.lines] }
            }
            return o
        }
        return ["blocks": blocks, "full": full, "highlighted": r.highlightedLines,
                "codeLines": r.codeLines, "micros": r.micros]
    }

    // MARK: Rendering, off the main thread, one request at a time

    private var renders: [(String, [String], Bool)] = []
    private var rendering = false

    private func render(key: String, ids: [String], full: Bool) {
        renders.append((key, ids, full))
        guard !rendering else { return }
        rendering = true
        Task { @MainActor in
            while !renders.isEmpty {
                let (key, ids, full) = renders.removeFirst()
                let r = await Self.worker.render(key: key, ids: ids, full: full)
                // A source shown since: its blocks are no longer wanted.
                guard keys.contains(key) else { continue }
                call("window.senate.blocks(key, r)", ["key": key, "r": Self.result(r, full: full)])
            }
            rendering = false
        }
    }

    actor MarkdownWorker {
        func render(key: String, ids: [String], full: Bool) -> MdRender {
            ConversationPage.markdown.render(key: key, ids: ids, full: full)
        }
    }

    // MARK: To the page

    private func call(_ js: String, _ args: [String: Any]) {
        guard ready else { return }
        web.callAsyncJavaScript(js, arguments: args, in: nil, in: bridge) { _ in }
    }

    private func sendTheme() {
        var vars: [String: String] = [:]
        let colors: [(String, Color)] = [
            ("fg", Theme.onSurface), ("dim", Theme.onSurfaceVariant), ("accent", Theme.primary),
            ("agent", Theme.agent), ("surface", Theme.surface), ("container", Theme.container),
            ("high", Theme.containerHigh), ("outline", Theme.outline), ("success", Theme.success),
            ("danger", Theme.danger), ("success-bg", Theme.successContainer),
            ("danger-bg", Theme.dangerContainer), ("committee", Theme.committee),
            ("attention", Theme.attention), ("attention-bg", Theme.attentionContainer),
            ("tok-keyword", Theme.Token.keyword), ("tok-string", Theme.Token.string),
            ("tok-number", Theme.Token.number), ("tok-comment", Theme.Token.comment),
            ("tok-type", Theme.Token.type), ("tok-name", Theme.Token.name),
            ("tok-operator", Theme.Token.operator_),
        ]
        for (prefix, name) in [("light", NSAppearance.Name.aqua), ("dark", .darkAqua)] {
            NSAppearance(named: name)?.performAsCurrentDrawingAppearance {
                for (key, color) in colors {
                    let c = NSColor(color).usingColorSpace(.sRGB) ?? .gray
                    vars["\(prefix)-\(key)"] = String(format: "rgb(%d %d %d)", Int(c.redComponent * 255),
                                                      Int(c.greenComponent * 255), Int(c.blueComponent * 255))
                }
            }
        }
        vars["content-font"] = Theme.contentFont
        call("window.senate.theme(vars)", ["vars": vars])
        call("window.senate.labels(l)", ["l": [
            "thinking": L("session.reasoning"), "copy": L("message.copy_code"), "commands": L("session.commands"),
            "earlier": L("session.earlier"), "working": L("status.working"),
        ]])
        if ProcessInfo.processInfo.environment["SENATE_APP_MD_DEBUG"] == "1" { call("window.senate.debug(true)", [:]) }
    }

    // MARK: Find, select all

    func showFind() {
        find.visible = true
        find.focus += 1
    }

    func search(_ query: String, forward: Bool = true, again: Bool = false) {
        find.query = query
        guard !query.isEmpty else {
            find.count = 0
            find.index = 0
            call("window.senate.find(null, true, false)", [:])
            return
        }
        call("window.senate.find(q, f, a)", ["q": query, "f": forward, "a": again])
    }

    func closeFind() {
        find.visible = false
        find.query = ""
        find.count = 0
        find.index = 0
        call("window.senate.find(null, true, false)", [:])
        web.window?.makeFirstResponder(web)
    }

    /// Show a search's match: the row it is in, its blocks rendered (a
    /// tool call opened), scrolled to the first match in it, highlighted.
    func reveal(row: UInt64, query: String) async {
        for _ in 0..<200 where !ready { try? await Task.sleep(for: .milliseconds(25)) }
        _ = await evaluate("await window.senate.reveal(id, q); return true", ["id": row, "q": query])
    }

    func selectAll() {
        call("window.senate.selectAll()", [:])
    }

    // MARK: From the page: a fixed set of actions, each checked

    /// At most so many of each action in a second.
    private static let rates: [String: Int] = [
        "ready": 4, "older": 4, "render": 400, "expand": 30, "link": 4, "copy": 10, "thread": 4, "found": 60,
    ]
    private var counted: [String: (start: ContinuousClock.Instant, count: Int)] = [:]

    private func allowed(_ action: String) -> Bool {
        guard let rate = Self.rates[action] else { return false }
        let now = ContinuousClock.now
        var c = counted[action] ?? (now, 0)
        if now - c.start > .seconds(1) { c = (now, 0) }
        c.count += 1
        counted[action] = c
        return c.count <= rate
    }

    fileprivate func handle(_ body: [String: Any]?, mainFrame: Bool) {
        guard mainFrame, let body, let action = body["action"] as? String, allowed(action) else { return }
        switch action {
        case "ready":
            Perf.mark("page ready")
            ready = true
            sent = [:]
            order = []
            sendTheme()
            push()
        case "older":
            switch source {
            case let .session(id):
                guard let t = store?.timeline, t.sessionId == id, t.hasOlder else { return }
                Task { await store?.loadOlder() }
            case let .thread(committee, root):
                guard let view = store?.thread, view.committee == committee, view.root == root,
                      view.events.count > shownEvents else { return }
                shownEvents += Self.eventPage
                push()
            default:
                return
            }
        case "render":
            guard let key = body["key"] as? String, keys.contains(key),
                  let ids = body["ids"] as? [String], !ids.isEmpty, ids.count <= 4000,
                  ids.allSatisfy(Self.isBlockId), let full = body["full"] as? Bool else { return }
            render(key: key, ids: ids, full: full)
        case "expand":
            // The page keeps what is open; the id must be a row shown now.
            guard let id = (body["id"] as? NSNumber)?.uint64Value, sent[id] != nil else { return }
        case "link":
            guard let href = body["href"] as? String, href.utf8.count < 4096, let url = URL(string: href),
                  let scheme = url.scheme?.lowercased(), ["http", "https", "mailto"].contains(scheme) else { return }
            NSWorkspace.shared.open(url)
        case "copy":
            guard let text = body["text"] as? String, text.utf8.count < 4_000_000 else { return }
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(text, forType: .string)
        case "thread":
            guard let committee = body["committee"] as? String,
                  let thread = (body["thread"] as? NSNumber)?.uint64Value,
                  cards.contains("\(committee)#\(thread)") else { return }
            store?.openThread(committee: committee, root: thread)
        case "found":
            guard find.visible, let count = (body["count"] as? NSNumber)?.intValue,
                  let index = (body["index"] as? NSNumber)?.intValue,
                  (0...1_000_000).contains(count), (0...count).contains(index) else { return }
            find.count = count
            find.index = index
        default:
            return
        }
    }

    private static func isBlockId(_ id: String) -> Bool {
        guard id.utf8.count < 24, let colon = id.firstIndex(of: ":") else { return false }
        return !id[..<colon].isEmpty && id[..<colon].allSatisfy(\.isASCII) && id[..<colon].allSatisfy(\.isNumber)
            && !id[id.index(after: colon)...].isEmpty && id[id.index(after: colon)...].allSatisfy { $0.isASCII && $0.isNumber }
    }

    // MARK: Tests, measurement and snapshots

    private var observers: [NSObjectProtocol] = []

    private func observe() {
        let center = NotificationCenter.default
        observers.append(center.addObserver(forName: Perf.scrollToRow, object: nil, queue: .main) { [weak self] note in
            let id = note.object as? UInt64
            MainActor.assumeIsolated { if let id { self?.call("window.senate.scrollToRow(id)", ["id": id]) } }
        })
        observers.append(center.addObserver(forName: Perf.openCall, object: nil, queue: .main) { [weak self] note in
            let id = note.object as? String
            MainActor.assumeIsolated { if let id { self?.call("window.senate.openCall(id)", ["id": id]) } }
        })
        #if DEBUG
        observers.append(center.addObserver(forName: Snapshot.scrollToTop, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.call("window.senate.scrollToTop()", [:]) }
        })
        observers.append(center.addObserver(forName: Snapshot.expandTools, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.call("window.senate.expandAll()", [:]) }
        })
        observers.append(center.addObserver(forName: Snapshot.scrollToItem, object: nil, queue: .main) { [weak self] note in
            let id = note.object as? UInt64
            let at = (note.userInfo?["anchor"] as? String) ?? "top"
            MainActor.assumeIsolated {
                if let id { self?.call("window.senate.scrollToRow(id, at)", ["id": id, "at": at]) }
            }
        })
        #endif
        Perf.pageStats = { [weak self] in await self?.pageStats() ?? "" }
        Perf.pageShown = { [weak self] in
            for _ in 0..<5000 {
                if await self?.evaluate("return await window.senate.shown()") as? Bool == true { return }
                try? await Task.sleep(for: .milliseconds(1))
            }
        }
    }

    /// Run `js` in the bridge's world (tests, measurement).
    func evaluate(_ js: String, _ args: [String: Any] = [:]) async -> Any? {
        guard ready else { return nil }
        return try? await web.callAsyncJavaScript(js, arguments: args, in: nil, contentWorld: bridge)
    }

    /// Whether the page has loaded and said it is ready.
    var isReady: Bool { ready }

    private func pageStats() async -> String {
        let stats = await evaluate("return window.senate.stats()") as? [String: Any]
        let n = { (k: String) in (stats?[k] as? NSNumber)?.doubleValue ?? 0 }
        return String(format: "page frames %.0f, over 25 ms %.0f, worst %.1f ms", n("frames"), n("slow"), n("worst"))
    }

    // MARK: The rule list

    private static var compiled: WKContentRuleList?
    private static func noNetwork(_ done: @escaping @MainActor (WKContentRuleList?) -> Void) {
        if let compiled { return done(compiled) }
        // The rule language has no alternation: one rule per scheme.
        let schemes = ["^https?:", "^wss?:", "^file:", "^ftp:", "^data:", "^blob:"]
        let rules = "[" + schemes.map { #"{"trigger":{"url-filter":""# + $0 + #""},"action":{"type":"block"}}"# }
            .joined(separator: ",") + "]"
        WKContentRuleListStore.default().compileContentRuleList(forIdentifier: "senate-no-network",
                                                                encodedContentRuleList: rules) { list, error in
            if let error { Log.app.error("conversation page: rule list: \(error.localizedDescription, privacy: .public)") }
            MainActor.assumeIsolated {
                compiled = list
                done(list)
            }
        }
    }

    // MARK: Crash recovery

    private var crashes: [ContinuousClock.Instant] = []

    fileprivate func contentProcessEnded() {
        ready = false
        let now = ContinuousClock.now
        crashes = crashes.filter { now - $0 < .seconds(60) } + [now]
        // Again and again: stop, rather than loop.
        guard crashes.count <= 3 else {
            Log.app.error("conversation page: its process ended \(self.crashes.count) times in a minute; not reloading")
            return
        }
        load()
    }
}

// MARK: - Navigation, windows, downloads

extension ConversationPage: WKNavigationDelegate, WKUIDelegate {
    func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction,
                 decisionHandler: @escaping @MainActor (WKNavigationActionPolicy) -> Void) {
        let ok = action.request.url == BundleScheme.page && action.targetFrame?.isMainFrame == true
            && !action.shouldPerformDownload
        decisionHandler(ok ? .allow : .cancel)
    }

    func webView(_ webView: WKWebView, decidePolicyFor response: WKNavigationResponse,
                 decisionHandler: @escaping @MainActor (WKNavigationResponsePolicy) -> Void) {
        let ok = response.response.url == BundleScheme.page && response.isForMainFrame && response.canShowMIMEType
        decisionHandler(ok ? .allow : .cancel)
    }

    func webView(_ webView: WKWebView, navigationAction: WKNavigationAction, didBecome download: WKDownload) {
        download.cancel { _ in }
    }

    func webView(_ webView: WKWebView, navigationResponse: WKNavigationResponse, didBecome download: WKDownload) {
        download.cancel { _ in }
    }

    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        contentProcessEnded()
    }

    func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration,
                 for action: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? {
        nil
    }

    func webView(_ webView: WKWebView, runJavaScriptAlertPanelWithMessage message: String,
                 initiatedByFrame frame: WKFrameInfo) async {}

    func webView(_ webView: WKWebView, runJavaScriptConfirmPanelWithMessage message: String,
                 initiatedByFrame frame: WKFrameInfo) async -> Bool { false }

    func webView(_ webView: WKWebView, runJavaScriptTextInputPanelWithPrompt prompt: String,
                 defaultText: String?, initiatedByFrame frame: WKFrameInfo) async -> String? { nil }
}

// MARK: - The web view

/// The page's web view: its context menu trimmed, drops refused, and
/// ⌘F / ⌘G / ⌘A going to the page's own find and select-all (which render
/// every block first, so they reach the whole conversation read).
final class PageWebView: WKWebView {
    weak var page: ConversationPage?

    /// Only these of WebKit's own menu items.
    private static let kept = [
        "WKMenuItemIdentifierCopy", "WKMenuItemIdentifierLookUp", "WKMenuItemIdentifierTranslate",
        "WKMenuItemIdentifierSpeechMenu",
    ]

    override func willOpenMenu(_ menu: NSMenu, with event: NSEvent) {
        super.willOpenMenu(menu, with: event)
        for item in menu.items.reversed() {
            let id = item.identifier?.rawValue ?? ""
            if !item.isSeparatorItem && !Self.kept.contains(id) { menu.removeItem(item) }
        }
        // No separators at the ends or doubled.
        while menu.items.first?.isSeparatorItem == true { menu.removeItem(at: 0) }
        while menu.items.last?.isSeparatorItem == true { menu.removeItem(at: menu.items.count - 1) }
        var last = false
        for item in menu.items.reversed() {
            if item.isSeparatorItem && last { menu.removeItem(item) }
            last = item.isSeparatorItem
        }
    }

    override func draggingEntered(_ sender: any NSDraggingInfo) -> NSDragOperation { [] }
    override func draggingUpdated(_ sender: any NSDraggingInfo) -> NSDragOperation { [] }
    override func performDragOperation(_ sender: any NSDraggingInfo) -> Bool { false }

    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        let mods = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
        if mods == .command, event.charactersIgnoringModifiers == "f" {
            page?.showFind()
            return true
        }
        return super.performKeyEquivalent(with: event)
    }

    override func selectAll(_ sender: Any?) {
        page?.selectAll()
    }
}

/// Shows the page's one web view where a page wants it. A page left
/// behind (SwiftUI may keep its view a while, and update it) never takes
/// the web view back: only a host that comes into a window claims it.
struct ConversationView: NSViewRepresentable {
    let page: ConversationPage
    let source: ConversationSource

    func makeNSView(context: Context) -> PageHost {
        let host = PageHost()
        host.page = page
        host.source = source
        return host
    }

    func updateNSView(_ host: PageHost, context: Context) {
        host.source = source
        if page.host === host { page.show(source) }
    }

    static func dismantleNSView(_ host: PageHost, coordinator: ()) {
        if host.page?.host === host { host.page?.host = nil }
    }
}

final class PageHost: NSView {
    weak var page: ConversationPage?
    var source: ConversationSource?

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        guard window != nil, let page, let source else { return }
        page.host = self
        page.web.translatesAutoresizingMaskIntoConstraints = true
        page.web.autoresizingMask = [.width, .height]
        page.web.frame = bounds
        addSubview(page.web)
        page.show(source)
    }
}

/// The page's message handler, holding the page weakly (the user content
/// controller would otherwise keep it alive).
private final class WeakHandler: NSObject, WKScriptMessageHandler {
    weak var target: ConversationPage?
    init(_ target: ConversationPage) { self.target = target }
    func userContentController(_ c: WKUserContentController, didReceive message: WKScriptMessage) {
        let body = message.body as? [String: Any]
        let main = message.frameInfo.isMainFrame
        MainActor.assumeIsolated { target?.handle(body, mainFrame: main) }
    }
}

/// `senate-app://bundle/<name>`: the page's files from the app bundle,
/// and nothing else; the page comes with its Content-Security-Policy.
private final class BundleScheme: NSObject, WKURLSchemeHandler {
    static let scheme = "senate-app"
    static let page = URL(string: "senate-app://bundle/conversation.html")!
    private static let files: [String: String] = [
        "conversation.html": "text/html", "conversation.css": "text/css", "style-senate.css": "text/css",
    ]
    /// The page's world runs no script; styles only from here; nothing
    /// else at all. The one HTML sink goes through the "senate" policy.
    static let policy = [
        "default-src 'none'", "script-src 'none'", "style-src senate-app:", "img-src 'none'",
        "font-src 'none'", "connect-src 'none'", "media-src 'none'", "object-src 'none'",
        "frame-src 'none'", "child-src 'none'", "worker-src 'none'", "manifest-src 'none'",
        "base-uri 'none'", "form-action 'none'", "frame-ancestors 'none'",
        "require-trusted-types-for 'script'", "trusted-types senate",
    ].joined(separator: "; ")

    func webView(_ webView: WKWebView, start task: any WKURLSchemeTask) {
        guard let url = task.request.url, url.host == "bundle", url.pathComponents.count == 2,
              let type = Self.files[url.lastPathComponent],
              let file = Bundle.main.url(forResource: url.deletingPathExtension().lastPathComponent,
                                         withExtension: url.pathExtension),
              let data = try? Data(contentsOf: file)
        else {
            task.didFailWithError(URLError(.fileDoesNotExist))
            return
        }
        var headers = ["Content-Type": "\(type); charset=utf-8", "X-Content-Type-Options": "nosniff"]
        if type == "text/html" { headers["Content-Security-Policy"] = Self.policy }
        task.didReceive(HTTPURLResponse(url: url, statusCode: 200, httpVersion: nil, headerFields: headers)!)
        task.didReceive(data)
        task.didFinish()
    }

    func webView(_ webView: WKWebView, stop task: any WKURLSchemeTask) {}
}

/// The find bar over the conversation: what it looks for, how many it
/// found, which one is shown.
@Observable
final class PageFind {
    var visible = false
    var query = ""
    var count = 0
    var index = 0
    /// Bumped to put the cursor in the field again.
    var focus = 0
}

/// The conversation where a page shows it, with its find bar (⌘F).
struct ConversationPane: View {
    let store: CoreStore
    let source: ConversationSource

    var body: some View {
        let page = store.page
        ConversationView(page: page, source: source)
            .overlay(alignment: .topTrailing) {
                if page.find.visible { FindBar(page: page).padding(Theme.Space.s) }
            }
            .background {
                // ⌘F, ⌘G and ⇧⌘G wherever the focus is on the page.
                Group {
                    Button("") { page.showFind() }.keyboardShortcut("f")
                    Button("") { page.search(page.find.query, forward: true, again: true) }.keyboardShortcut("g")
                    Button("") { page.search(page.find.query, forward: false, again: true) }
                        .keyboardShortcut("g", modifiers: [.command, .shift])
                }
                .opacity(0)
                .accessibilityHidden(true)
            }
    }
}

/// Look for text in the conversation read so far.
private struct FindBar: View {
    let page: ConversationPage
    @State private var query = ""
    @FocusState private var focused: Bool

    var body: some View {
        let find = page.find
        HStack(spacing: Theme.Space.xs) {
            Image(systemName: "magnifyingglass").foregroundStyle(Theme.onSurfaceVariant)
            TextField(L("find.placeholder"), text: $query)
                .textFieldStyle(.plain)
                .frame(width: 180)
                .focused($focused)
                .onSubmit { page.search(query, forward: true, again: true) }
                .onChange(of: query) { _, q in page.search(q) }
                .onKeyPress(.escape) { page.closeFind(); return .handled }
            Text(verbatim: find.query.isEmpty ? "" : find.count == 0 ? L("find.none") : L("find.count", Int64(find.index), Int64(find.count)))
                .font(Theme.Fonts.meta)
                .foregroundStyle(Theme.onSurfaceVariant)
                .monospacedDigit()
            Button { page.search(query, forward: false, again: true) } label: { Image(systemName: "chevron.up") }
                .buttonStyle(.borderless).disabled(find.count == 0)
                .help(L("find.previous"))
            Button { page.search(query, forward: true, again: true) } label: { Image(systemName: "chevron.down") }
                .buttonStyle(.borderless).disabled(find.count == 0)
                .help(L("find.next"))
            Button { page.closeFind() } label: { Image(systemName: "xmark.circle.fill") }
                .buttonStyle(.borderless)
                .foregroundStyle(Theme.onSurfaceVariant)
                .help(L("find.done"))
        }
        .padding(.horizontal, Theme.Space.s)
        .padding(.vertical, 6)
        .background(.regularMaterial, in: Capsule())
        .overlay { Capsule().stroke(Theme.outline) }
        .onAppear { query = find.query; focused = true }
        .onChange(of: find.focus) { focused = true }
    }
}
