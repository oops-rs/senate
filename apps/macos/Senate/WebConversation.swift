import AppKit
import SenateCore
import SwiftUI
import WebKit

/// SPIKE (spike/web-conversation): the session's conversation as one web
/// page in a `WKWebView`, built from the same `ConversationRows`.
///
/// Safety: the page and its scripts come only from the app bundle, through
/// the `senate-app:` scheme; a strict Content-Security-Policy in the page
/// allows nothing else (no network, images, frames, forms); a content rule
/// list blocks every http(s) and file load; navigation away from the page is
/// cancelled; the website data store is not persistent. Data goes in as
/// call arguments (`callAsyncJavaScript`), never interpolated into script;
/// Markdown is parsed and rendered to HTML in the core (senate-markdown:
/// a fixed set of tags and classes, text escaped); the page inserts it. The page sends back only these
/// actions, each validated here: ready, older, render (a known message,
/// block ids), expand, link (opened by the app if http/https/mailto),
/// copy, thread.
struct WebConversation: NSViewRepresentable {
    let store: CoreStore
    let session: SessionLine
    let timeline: SessionTimeline?

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeNSView(context: Context) -> WKWebView {
        let c = context.coordinator
        Perf.mark("web view made")
        let config = WKWebViewConfiguration()
        config.websiteDataStore = .nonPersistent()
        config.setURLSchemeHandler(BundleScheme(), forURLScheme: BundleScheme.scheme)
        config.userContentController.add(WeakHandler(c), name: "senate")
        config.preferences.javaScriptCanOpenWindowsAutomatically = false
        config.defaultWebpagePreferences.allowsContentJavaScript = true
        let web = WKWebView(frame: .zero, configuration: config)
        web.navigationDelegate = c
        web.setValue(false, forKey: "drawsBackground")
        web.allowsBackForwardNavigationGestures = false
        web.allowsMagnification = false
        c.web = web
        // Block every network and file load, whatever the page says.
        // (The rule language has no alternation: one rule per scheme.) If
        // the list does not compile, the page is not loaded: fail closed.
        Self.noNetwork { list in
            guard let list else {
                Perf.report("web: the no-network rule list did not compile; page not loaded")
                return
            }
            config.userContentController.add(list)
            web.load(URLRequest(url: URL(string: "\(BundleScheme.scheme)://bundle/conversation.html")!))
        }
        c.observe()
        return web
    }

    private static var compiled: WKContentRuleList?
    private static func noNetwork(_ done: @escaping @MainActor (WKContentRuleList?) -> Void) {
        if let compiled { return done(compiled) }
        let schemes = ["^https?:", "^wss?:", "^file:", "^ftp:", "^data:", "^blob:"]
        let rules = "[" + schemes.map { #"{"trigger":{"url-filter":""# + $0 + #""},"action":{"type":"block"}}"# }
            .joined(separator: ",") + "]"
        WKContentRuleListStore.default().compileContentRuleList(forIdentifier: "senate-no-network",
                                                                encodedContentRuleList: rules) { list, error in
            if let error { Perf.report("web: rule list: \(error)") }
            MainActor.assumeIsolated {
                compiled = list
                done(list)
            }
        }
    }

    func updateNSView(_ web: WKWebView, context: Context) {
        context.coordinator.update(store: store, session: session, timeline: timeline)
    }

    @MainActor
    final class Coordinator: NSObject, WKNavigationDelegate, WKScriptMessageHandler {
        weak var web: WKWebView?
        var store: CoreStore?
        var session: SessionLine?
        weak var timeline: SessionTimeline?
        private var ready = false
        /// What the page shows: rows by id, in order.
        private var sent: [UInt64: ConversationRow] = [:]
        private var order: [UInt64] = []
        private var sentState: [String: Bool] = [:]
        /// Messages' Markdown, parsed and rendered in the core; kept across
        /// sessions and web views (the page asks again for what it lost).
        nonisolated static let markdown: MarkdownStore = {
            let store = MarkdownStore()
            // The highlighter's syntaxes load on first use: now, off the main thread.
            Task.detached(priority: .utility) { store.warm() }
            return store
        }()
        /// Rendering runs here, off the main thread, one request at a time
        /// (so the page gets its patches in the order the core made them).
        private static let worker = MarkdownWorker()
        /// The message keys the page was given (what `render` may name).
        private var keys: Set<String> = []
        nonisolated(unsafe) private var observers: [NSObjectProtocol] = []

        func observe() {
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
                // Until a frame of the page has rows in it.
                for _ in 0..<5000 {
                    let n = try? await self?.web?.callAsyncJavaScript(
                        "return await new Promise(r => requestAnimationFrame(() => r(document.querySelector('.blk') ? document.querySelectorAll('.blk:not(.ph)').length : 0)))",
                        arguments: [:], in: nil, contentWorld: .page) as? NSNumber
                    if (n?.intValue ?? 0) > 0 { return }
                    try? await Task.sleep(for: .milliseconds(1))
                }
            }
        }

        isolated deinit {
            for o in observers { NotificationCenter.default.removeObserver(o) }
        }

        func update(store: CoreStore, session: SessionLine, timeline: SessionTimeline?) {
            self.store = store
            let changed = timeline !== self.timeline || self.session?.sessionId != session.sessionId
            if changed {
                self.timeline = timeline
                sent = [:]
                order = []
                keys = []
            }
            self.session = session
            push()
            if changed { track() }
        }

        /// Follow the timeline itself: SwiftUI does not call updateNSView
        /// when only the timeline's contents change (the view's inputs are
        /// the same objects).
        private var generation = 0
        private func track() {
            generation += 1
            let mine = generation
            guard let t = timeline else { return }
            withObservationTracking {
                _ = (t.rows, t.loading, t.hasOlder, t.loadingOlder, t.working)
            } onChange: { [weak self] in
                Task { @MainActor in
                    guard let self, self.generation == mine else { return }
                    self.push()
                    self.track()
                }
            }
        }

        private func call(_ js: String, _ args: [String: Any]) {
            web?.callAsyncJavaScript(js, arguments: args, in: nil, in: .page) { _ in }
        }

        /// Send the page what changed since it last heard: everything at
        /// first; rows read above (prepend); changed rows and rows after
        /// (upsert); else everything again.
        private func push() {
            guard ready, let t = timeline else { return }
            let rows = t.loading ? [] : t.rows
            let ids = rows.map(\.id)
            let state: [String: Bool] = ["hasOlder": t.hasOlder, "loadingOlder": t.loadingOlder, "working": t.working]
            var op: [String: Any]
            if order.isEmpty || ids.isEmpty {
                op = ["type": "reset", "rows": rows.map(encode)]
                op["first"] = firstScreen(rows)
            } else if let first = order.first, let at = ids.firstIndex(of: first), at > 0,
                      Array(ids[at...].prefix(order.count)) == order {
                let changed = rows[at...].filter { sent[$0.id] != nil && sent[$0.id] != $0 }
                let added = rows.suffix(ids.count - at - order.count)
                op = ["type": "prepend", "rows": rows[..<at].reversed().map(encode),
                      "changed": (changed + added).map(encode)]
            } else if ids.count >= order.count, Array(ids.prefix(order.count)) == order {
                let changed = rows.filter { sent[$0.id] != nil && sent[$0.id] != $0 }
                let added = rows.suffix(ids.count - order.count)
                let all = changed + added
                if all.isEmpty && state == sentState { return }
                op = ["type": "upsert", "rows": all.map(encode)]
            } else {
                op = ["type": "reset", "rows": rows.map(encode)]
                op["first"] = firstScreen(rows)
            }
            op["state"] = state
            if op["type"] as? String == "reset" { Perf.mark("reset encoded (\(rows.count) rows)") }
            sent = Dictionary(uniqueKeysWithValues: rows.map { ($0.id, $0) })
            order = ids
            sentState = state
            call("window.senate.apply(op)", ["op": op])
        }

        private func encode(_ row: ConversationRow) -> [String: Any] {
            let names = { (text: String) in Mentions.shared.plain(text, committee: self.session?.committee) }
            var out: [String: Any] = ["id": row.id, "thinking": row.thinking]
            switch row.kind {
            case let .user(text): out["kind"] = "user"; out["text"] = text
            case let .assistant(text):
                out["kind"] = "assistant"
                let key = "\(session?.sessionId ?? "")/\(row.id)"
                keys.insert(key)
                out["md"] = Self.encode(key: key, Self.markdown.update(key: key, text: names(text)))
            case let .tool(call):
                out["kind"] = "tool"
                var tool: [String: Any] = ["callId": call.callId, "name": call.name, "kind": call.kind,
                                           "status": call.status, "title": call.title]
                if let o = call.output { tool["output"] = o }
                if let d = call.diff { tool["diff"] = d }
                if let e = call.exitCode { tool["exit"] = e }
                out["tool"] = tool
            case let .todo(items):
                out["kind"] = "todo"
                out["todo"] = items.map { ["text": $0.text, "status": $0.status] }
            case let .notice(level, text): out["kind"] = "notice"; out["text"] = text; out["level"] = level
            case let .senate(m):
                out["kind"] = "senate"
                var s: [String: Any] = ["text": names(m.text)]
                if let f = m.from { s["from"] = names(f) }
                if let k = m.kindLabel { s["kindLabel"] = k }
                if let c = m.committee { s["committee"] = c }
                if let th = m.thread { s["thread"] = th }
                out["senate"] = s
            }
            return out
        }

        /// What a reset shows first (it opens at the foot): the last
        /// messages' blocks rendered, in the same call, so the first screen
        /// takes one trip rather than two.
        private func firstScreen(_ rows: [ConversationRow]) -> [[String: Any]] {
            let keys = rows.suffix(12).compactMap { row -> String? in
                if case .assistant = row.kind { return "\(session?.sessionId ?? "")/\(row.id)" }
                return nil
            }.suffix(3)
            return keys.map { key in
                let ids = Self.markdown.update(key: key, text: textOf(key)).blocks.map(\.id)
                return ["key": key, "r": result(Self.markdown.render(key: key, ids: ids, full: true), full: true)]
            }
        }

        private func textOf(_ key: String) -> String {
            guard let id = UInt64(key.split(separator: "/").last ?? ""),
                  let row = timeline?.rows.first(where: { $0.id == id }), case let .assistant(text) = row.kind
            else { return "" }
            return Mentions.shared.plain(text, committee: session?.committee)
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

        /// The page asks for blocks' HTML (whole, or what changed since).
        private func render(key: String, ids: [String], full: Bool) {
            renders.append((key, ids, full))
            guard !rendering else { return }
            rendering = true
            Task { @MainActor in
                while !renders.isEmpty {
                    let (key, ids, full) = renders.removeFirst()
                    let r = await Self.worker.render(key: key, ids: ids, full: full)
                    deliver(key: key, full: full, r)
                }
                rendering = false
            }
        }
        private var renders: [(String, [String], Bool)] = []
        private var rendering = false

        private func deliver(key: String, full: Bool, _ r: MdRender) {
            call("window.senate.blocks(key, r)", ["key": key, "r": result(r, full: full)])
        }

        private func result(_ r: MdRender, full: Bool) -> [String: Any] {
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

        /// Rendering, off the main thread.
        actor MarkdownWorker {
            func render(key: String, ids: [String], full: Bool) -> MdRender {
                Coordinator.markdown.render(key: key, ids: ids, full: full)
            }
        }

        // MARK: From the page: a fixed set of actions, each validated.

        nonisolated func userContentController(_ controller: WKUserContentController, didReceive message: WKScriptMessage) {
            let body = message.body as? [String: Any]
            let frame = message.frameInfo.isMainFrame
            MainActor.assumeIsolated { handle(body, mainFrame: frame) }
        }

        private func handle(_ body: [String: Any]?, mainFrame: Bool) {
            guard mainFrame, let body, let action = body["action"] as? String else { return }
            switch action {
            case "ready":
                Perf.mark("page ready")
                ready = true
                sendTheme()
                push()
            case "older":
                guard timeline?.hasOlder == true else { return }
                Task { await store?.loadOlder() }
            case "render":
                guard let key = body["key"] as? String, keys.contains(key),
                      let ids = body["ids"] as? [String], !ids.isEmpty, ids.count <= 2000,
                      ids.allSatisfy({ $0.count < 24 && $0.range(of: "^[0-9]+:[0-9]+$", options: .regularExpression) != nil }),
                      let full = body["full"] as? Bool else { return }
                render(key: key, ids: ids, full: full)
            case "expand":
                break // the page keeps it; nothing to size natively
            case "link":
                guard let href = body["href"] as? String, href.count < 4096, let url = URL(string: href),
                      let scheme = url.scheme?.lowercased(), ["http", "https", "mailto"].contains(scheme) else { return }
                NSWorkspace.shared.open(url)
            case "copy":
                guard let text = body["text"] as? String, text.utf8.count < 4_000_000 else { return }
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(text, forType: .string)
            case "thread":
                guard let committee = body["committee"] as? String,
                      committee.range(of: "^[a-z0-9][a-z0-9_-]{0,63}$", options: .regularExpression) != nil,
                      let thread = (body["thread"] as? NSNumber)?.uint64Value, thread > 0 else { return }
                store?.openThread(committee: committee, root: thread)
            default:
                return
            }
        }

        private func sendTheme() {
            var vars: [String: String] = [:]
            let colors: [(String, Color)] = [
                ("fg", Theme.onSurface), ("dim", Theme.onSurfaceVariant), ("accent", Theme.primary),
                ("agent", Theme.agent), ("surface", Theme.surface), ("container", Theme.container),
                ("high", Theme.containerHigh), ("outline", Theme.outline), ("success", Theme.success),
                ("danger", Theme.danger), ("success-bg", Theme.successContainer),
                ("danger-bg", Theme.dangerContainer), ("committee", Theme.committee),
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
            if ProcessInfo.processInfo.environment["SENATE_APP_MD_DEBUG"] == "1" { call("window.senate.debug(true)", [:]) }

            call("window.senate.labels(l)", ["l": ["thinking": L("session.reasoning"), "copy": L("message.copy_code"),
                                                   "earlier": L("session.earlier"), "working": L("status.working")]])
        }

        // MARK: Navigation: the page itself, nothing else.

        func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction,
                     decisionHandler: @escaping @MainActor (WKNavigationActionPolicy) -> Void) {
            let ok = action.request.url?.absoluteString == "\(BundleScheme.scheme)://bundle/conversation.html"
                && action.targetFrame?.isMainFrame == true
            decisionHandler(ok ? .allow : .cancel)
        }

        // MARK: Measurement

        func pageStats() async -> String {
            guard let web else { return "" }
            let stats = try? await web.callAsyncJavaScript("return window.senate.stats()", arguments: [:],
                                                           in: nil, contentWorld: .page) as? [String: Any]
            let frames = (stats?["frames"] as? NSNumber)?.intValue ?? 0
            let slow = (stats?["slow"] as? NSNumber)?.intValue ?? 0
            let worst = (stats?["worst"] as? NSNumber)?.doubleValue ?? 0
            var rss = ""
            if let pid = web.value(forKey: "_webProcessIdentifier") as? Int32, pid > 0 {
                let p = Process()
                p.executableURL = URL(fileURLWithPath: "/bin/ps")
                p.arguments = ["-o", "rss=", "-p", String(pid)]
                let pipe = Pipe()
                p.standardOutput = pipe
                try? p.run()
                p.waitUntilExit()
                let kb = Int(String(data: pipe.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8)?
                    .trimmingCharacters(in: .whitespacesAndNewlines) ?? "") ?? 0
                rss = ", WebContent RSS \(kb / 1024) MB"
            }
            let a = { (k: String) in (stats?[k] as? NSNumber)?.doubleValue ?? 0 }
            return String(format: "page frames %d, over 25 ms %d, worst %.1f ms%@; apply worst %.0f ms, reset apply %.0f ms (first screen %.0f ms), reset arrived %.0f ms after ready, next frame %.0f ms after it",
                          frames, slow, worst, rss, a("applyWorst"), a("applyResetMs"), a("firstMs"), a("arrivedMs"), a("frameAfterMs"))
        }
    }
}

/// The page's message handler, holding its coordinator weakly (the user
/// content controller would otherwise keep it alive).
private final class WeakHandler: NSObject, WKScriptMessageHandler {
    weak var target: (any WKScriptMessageHandler)?
    init(_ target: any WKScriptMessageHandler) { self.target = target }
    func userContentController(_ c: WKUserContentController, didReceive message: WKScriptMessage) {
        target?.userContentController(c, didReceive: message)
    }
}

/// `senate-app://bundle/<name>`: files from the app bundle's resources, and
/// nothing else (no path components, a fixed set of types).
private final class BundleScheme: NSObject, WKURLSchemeHandler {
    static let scheme = "senate-app"
    private static let types = ["html": "text/html", "css": "text/css", "js": "text/javascript"]

    func webView(_ webView: WKWebView, start task: any WKURLSchemeTask) {
        let url = task.request.url
        let name = url?.lastPathComponent ?? ""
        let ext = (name as NSString).pathExtension
        guard url?.host == "bundle", url?.pathComponents.count == 2, let type = Self.types[ext],
              let file = Bundle.main.url(forResource: (name as NSString).deletingPathExtension, withExtension: ext),
              let data = try? Data(contentsOf: file)
        else {
            task.didFailWithError(URLError(.fileDoesNotExist))
            return
        }
        task.didReceive(HTTPURLResponse(url: url!, statusCode: 200, httpVersion: nil,
                                        headerFields: ["Content-Type": "\(type); charset=utf-8"])!)
        task.didReceive(data)
        task.didFinish()
    }

    func webView(_ webView: WKWebView, stop task: any WKURLSchemeTask) {}
}
