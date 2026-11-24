import AppKit
import Foundation
import Testing
import WebKit
@testable import Senate
import SenateCore

/// The conversation page in a real web view: what it renders, what it
/// lets out, what the page's own world can reach.
@MainActor
@Suite(.serialized)
struct ConversationPageTests {
    /// A page showing `messages`, every block rendered.
    private func page(_ messages: [String]) async throws -> ConversationPage {
        let page = ConversationPage(store: nil)
        for _ in 0..<500 where !page.isReady { try await Task.sleep(for: .milliseconds(20)) }
        try #require(page.isReady, "the page did not load")
        page.show(.fixture(messages))
        for _ in 0..<500 {
            if await page.evaluate("return document.querySelectorAll('.row').length") as? Int == messages.count { break }
            try await Task.sleep(for: .milliseconds(20))
        }
        _ = await page.evaluate("await window.senate.renderAll(); return true")
        return page
    }

    static let hostile = [
        "<script>window.hacked = 1</script>",
        "<img src=x onerror=\"window.hacked = 1\">",
        "<svg/onload=\"window.hacked = 1\">",
        "<iframe src=\"https://example.com\"></iframe>",
        "<a href=\"javascript:window.hacked = 1\">x</a> [y](javascript:window.hacked=1) [z](JaVaScRiPt:1)",
        "[d](data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==) [f](file:///etc/passwd)",
        "![image](https://example.com/a.png)",
        "<div style=\"background:url(https://example.com)\">x</div><style>body{display:none}</style>",
        "<form action=https://example.com><input name=a></form><object data=x></object>",
        "<base href=\"https://example.com/\"><meta http-equiv=refresh content=\"0;url=https://example.com\">",
        "| a | b |\n|---|---|\n| <img src=x onerror=1> | [x](javascript:1) |",
        "- [ ] <b onclick=\"window.hacked = 1\">task</b>\n- [x] done",
        "```html\n<script>window.hacked = 1</script>\n```",
        "> <script>window.hacked = 1</script>",
        "<details open ontoggle=\"window.hacked = 1\">x</details>",
    ]

    @Test func hostileMarkdownRendersOnlyTheFixedSet() async throws {
        let page = try await page(Self.hostile)
        let report = await page.evaluate("""
            const allowed = new Set(["DIV","SPAN","BUTTON","P","H1","H2","H3","H4","H5","H6","UL","OL","LI","PRE","CODE",
              "BLOCKQUOTE","TABLE","THEAD","TBODY","TR","TH","TD","HR","BR","STRONG","EM","DEL","A","MAIN"]);
            const bad = [];
            for (const e of document.querySelectorAll("#rows *")) {
              if (!allowed.has(e.tagName)) bad.push("tag " + e.tagName);
              for (const a of e.attributes) {
                if (a.name.startsWith("on")) bad.push("attribute " + a.name);
                if (a.name === "style" && !e.classList.contains("ph")) bad.push("style on " + e.tagName);
                if (a.name === "src") bad.push("src on " + e.tagName);
              }
              if (e.tagName === "A") {
                const h = (e.getAttribute("href") || "").toLowerCase();
                if (!/^(https?:|mailto:)/.test(h)) bad.push("href " + h);
              }
            }
            return bad.join("; ")
            """) as? String
        #expect(report == "", "\(report ?? "no answer")")
        let hacked = try await page.web.callAsyncJavaScript("return typeof window.hacked", arguments: [:], contentWorld: .page)
        #expect(hacked as? String == "undefined")
    }

    @Test func thePagesOwnWorldReachesNothing() async throws {
        let page = try await page(["Hello"])
        let seen = try await page.web.callAsyncJavaScript(
            "return [typeof (window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.senate), typeof window.senate].join('/')",
            arguments: [:], contentWorld: .page
        )
        #expect(seen as? String == "undefined/undefined")
    }

    @Test func nothingLoadsFromTheNetwork() async throws {
        let page = try await page(["Hello"])
        let fetch = await page.evaluate("try { await fetch('https://example.com/'); return 'loaded' } catch { return 'blocked' }")
        #expect(fetch as? String == "blocked")
        let image = await page.evaluate("""
            return await new Promise((r) => { const i = new Image(); i.onload = () => r('loaded'); i.onerror = () => r('blocked');
              i.src = 'https://example.com/a.png'; setTimeout(() => r('blocked'), 3000); })
            """)
        #expect(image as? String == "blocked")
        let socket = await page.evaluate("""
            return await new Promise((r) => { try { const s = new WebSocket('wss://example.com/'); s.onopen = () => r('open');
              s.onerror = () => r('blocked'); setTimeout(() => r('blocked'), 3000) } catch { r('blocked') } })
            """)
        #expect(socket as? String == "blocked")
        let file = await page.evaluate("try { await fetch('file:///etc/hosts'); return 'loaded' } catch { return 'blocked' }")
        #expect(file as? String == "blocked")
    }

    @Test func navigationAndWindowsAreRefused() async throws {
        let page = try await page(["Hello"])
        let opened = await page.evaluate("return window.open('https://example.com/') === null")
        #expect(opened as? Bool == true)
        _ = await page.evaluate("location.href = 'https://example.com/'; return true")
        try await Task.sleep(for: .milliseconds(500))
        #expect(page.web.url?.absoluteString == "senate-app://bundle/conversation.html")
        let rows = await page.evaluate("return document.querySelectorAll('.row').length") as? Int
        #expect(rows == 1)
    }

    @Test func copyIsPlainTextWithListMarkers() async throws {
        let page = try await page([
            "A paragraph with **strong** text.\n\n1. one\n2. two\n   - nested\n\n- [x] done\n- [ ] open\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```rust\nlet a = 1;\n```",
        ])
        let text = await page.evaluate("return window.senate.plainText()") as? String
        #expect(text == """
            A paragraph with strong text.

            1. one
            2. two
              - nested

            - [x] done
            - [ ] open

            a\tb
            1\t2

            let a = 1;
            """)
    }

    @Test func trustedTypesGuardTheOneSinkWhereWebKitHasThem() async throws {
        let page = try await page(["**bold**"])
        let bold = await page.evaluate("return document.querySelector('#rows strong')?.textContent") as? String
        #expect(bold == "bold")
        let has = try await page.web.callAsyncJavaScript("return typeof window.trustedTypes", arguments: [:], contentWorld: .page)
        if has as? String == "object" {
            // The page requires them: a raw string into a sink is refused.
            let raw = try await page.web.callAsyncJavaScript(
                "try { document.createElement('div').innerHTML = '<b>x</b>'; return 'allowed' } catch { return 'refused' }",
                arguments: [:], contentWorld: .page
            )
            #expect(raw as? String == "refused")
        }
    }
}

extension ConversationPageTests {
    @Test func framesAndObserversRunInTheBridgeWorld() async throws {
        let page = try await page(["Hello"])
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 600, height: 400), styleMask: [.titled], backing: .buffered, defer: false)
        window.contentView = page.web
        window.orderFrontRegardless()
        defer { window.orderOut(nil) }
        try await Task.sleep(for: .milliseconds(300))
        let raf = await page.evaluate("return await Promise.race([new Promise(r => requestAnimationFrame(() => r('frame'))), new Promise(r => setTimeout(() => r('none'), 2000))])")
        #expect(raf as? String == "frame")
        let io = await page.evaluate("""
            return await Promise.race([new Promise(r => { const o = new IntersectionObserver(() => r('observed'));
              o.observe(document.querySelector('.row')) }), new Promise(r => setTimeout(() => r('none'), 2000))])
            """)
        #expect(io as? String == "observed")
    }
}

extension ConversationPageTests {
    @Test func findAndSelectAllReachEveryBlock() async throws {
        let long = (1...40).map { "Paragraph \($0) of a long reply, with the word needle once." }.joined(separator: "\n\n")
        let page = try await page([long, "No match here.", "Another needle, and a NEEDLE."])
        _ = await page.evaluate("await window.senate.find('needle', true, false); return true")
        let found = await page.evaluate("return CSS.highlights.get('find').size") as? Int
        #expect(found == 42)
        _ = await page.evaluate("await window.senate.find('needle', true, true); return true")
        let now = await page.evaluate("return CSS.highlights.get('find-now').size") as? Int
        #expect(now == 1)
        _ = await page.evaluate("await window.senate.find(null, true, false); return true")
        let cleared = await page.evaluate("return CSS.highlights.has('find')") as? Bool
        #expect(cleared == false)
        _ = await page.evaluate("await window.senate.selectAll(); return true")
        let selected = await page.evaluate("return window.getSelection().toString()") as? String ?? ""
        #expect(selected.contains("Paragraph 40 of a long reply"))
        #expect(selected.contains("Another needle"))
    }
}

extension ConversationPageTests {
    /// Commands back to back are one closed row; find and select-all still
    /// reach what is inside, and find opens the group at a match.
    @Test func aGroupOfCommandsIsClosedButFoundAndCopied() async throws {
        let page = ConversationPage(store: nil)
        for _ in 0..<500 where !page.isReady { try await Task.sleep(for: .milliseconds(20)) }
        let call = { (seq: UInt64, title: String, status: String) in
            TimelineEntry(seq: seq, at: nil, truncated: false, body: .tool(call: ToolCall(
                callId: "c\(seq)", name: "Bash", kind: "shell", status: status, statusLabel: status,
                title: title, cwd: nil, output: "output of \(title)", exitCode: status == "failed" ? 1 : 0, diff: nil, mcp: nil)))
        }
        let rows = ConversationRows.rows([
            call(1, "cargo build", "completed"), call(2, "cargo test needle", "failed"), call(3, "git status", "running"),
        ])
        ConversationPage.fixtureRows["group"] = rows
        page.show(.fixtureRows("group"))
        for _ in 0..<100 {
            if await page.evaluate("return document.querySelectorAll('.k-group').length") as? Int == 1 { break }
            try await Task.sleep(for: .milliseconds(20))
        }
        let head = await page.evaluate("return document.querySelector('.group-head').textContent") as? String ?? ""
        #expect(head.contains("3"), "the count: \(head)")
        #expect(head.contains("git status"), "the running one: \(head)")
        #expect(head.contains("✕ 1"), "a failure: \(head)")
        let closed = await page.evaluate("return document.querySelector('.group-members').hidden") as? Bool
        #expect(closed == true)
        _ = await page.evaluate("await window.senate.find('needle', true, false); return true")
        let opened = await page.evaluate("return document.querySelector('.group-members').hidden") as? Bool
        #expect(opened == false, "find opened the group at its match")
        _ = await page.evaluate("await window.senate.find(null, true, false); return true")
        let text = await page.evaluate("return window.senate.plainText()") as? String ?? ""
        #expect(text.contains("cargo build") && text.contains("git status"))
    }
}

extension ConversationPageTests {
    /// Rows read from an older page land above, oldest first.
    @Test func olderRowsArriveInOrder() async throws {
        let page = try await page(["latest"])
        _ = await page.evaluate("""
            window.senate.apply({ type: "reset", rows: [{ id: 10, kind: "user", text: "c" }], state: {} });
            window.senate.apply({ type: "prepend", rows: [{ id: 1, kind: "user", text: "a" }, { id: 2, kind: "user", text: "b" }], state: {} });
            return true
            """)
        let order = await page.evaluate("return Array.from(document.querySelectorAll('.row')).map(r => r.textContent).join('')") as? String
        #expect(order == "abc")
    }
}

extension ConversationPageTests {
    /// A search's match on a command inside a closed group: the group opens,
    /// the command opens, and the match in its output is the current one.
    @Test func aMatchInsideAGroupIsRevealed() async throws {
        let page = ConversationPage(store: nil)
        for _ in 0..<500 where !page.isReady { try await Task.sleep(for: .milliseconds(20)) }
        let call = { (seq: UInt64, title: String) in
            TimelineEntry(seq: seq, at: nil, truncated: false, body: .tool(call: ToolCall(
                callId: "c\(seq)", name: "Bash", kind: "shell", status: "completed", statusLabel: "completed",
                title: title, cwd: nil, output: "output of \(title)", exitCode: 0, diff: nil, mcp: nil)))
        }
        let rows = ConversationRows.rows([call(1, "cargo build"), call(2, "cargo test needle"), call(3, "git status")])
        #expect(ConversationRows.rowId(for: 2, in: rows) == 2)
        ConversationPage.fixtureRows["reveal-group"] = rows
        page.show(.fixtureRows("reveal-group"))
        for _ in 0..<100 {
            if await page.evaluate("return document.querySelectorAll('.k-group').length") as? Int == 1 { break }
            try await Task.sleep(for: .milliseconds(20))
        }
        await page.reveal(row: 2, query: "needle")
        let groupOpen = await page.evaluate("return !document.querySelector('.group-members').hidden") as? Bool
        #expect(groupOpen == true, "the group opened")
        let output = await page.evaluate("""
            return document.querySelector('[data-id="m2"]').textContent.includes('output of cargo test needle')
            """) as? Bool
        #expect(output == true, "the command opened, its output shown")
        let current = await page.evaluate("""
            const h = CSS.highlights.get('find-now');
            if (!h) return '';
            const [range] = [...h];
            return range.startContainer.parentElement.closest('[data-id]').dataset.id
            """) as? String
        #expect(current == "m2", "the match in that command is the current one")
    }
}

extension ConversationPageTests {
    /// A long session: messages with code and tables, and runs of commands.
    static func longSession(_ seqs: ClosedRange<UInt64>) -> [TimelineEntry] {
        seqs.map { seq in
            switch seq % 5 {
            case 0, 1:
                return TimelineEntry(seq: seq, at: nil, truncated: false, body: .tool(call: ToolCall(
                    callId: "c\(seq)", name: "Bash", kind: "shell", status: "completed", statusLabel: "completed",
                    title: "cargo test step \(seq)", cwd: nil, output: "ok \(seq)", exitCode: 0, diff: nil, mcp: nil)))
            case 2:
                return TimelineEntry(seq: seq, at: nil, truncated: false, body: .user(text: "Message \(seq): go on."))
            default:
                let text = """
                    ## Reply \(seq)

                    Some prose for \(seq), long enough to wrap over a line or two in the reading column.

                    ```swift
                    let value\(seq) = \(seq)
                    print(value\(seq))
                    ```

                    | a | b |
                    |---|---|
                    | \(seq) | \(seq * 2) |
                    """
                return TimelineEntry(seq: seq, at: nil, truncated: false, body: .assistant(text: text))
            }
        }
    }

    /// Older rows arrive while the reader is scrolled near the top, and
    /// went on scrolling while they loaded: the row they are looking at
    /// stays where it is on screen, even when the older page's last
    /// commands join the first group shown (its row is new).
    @Test func olderRowsKeepTheRowInViewWhereItIs() async throws {
        let page = ConversationPage(store: nil)
        for _ in 0..<500 where !page.isReady { try await Task.sleep(for: .milliseconds(20)) }
        // The view starts with a command (51); the older page ends with one
        // (50): together they are a group, a row the page has not seen.
        let current = ConversationRows.rows(Self.longSession(51...120))
        ConversationPage.fixtureRows["older"] = current
        page.show(.fixtureRows("older"))
        for _ in 0..<200 {
            if await page.evaluate("return document.querySelectorAll('.row').length") as? Int == current.count { break }
            try await Task.sleep(for: .milliseconds(20))
        }
        _ = await page.evaluate("await window.senate.renderAll(); return true")
        // Near the top, then a little further while the older page loads.
        _ = await page.evaluate("document.scrollingElement.scrollTop = 600; return true")
        try await Task.sleep(for: .milliseconds(50))
        _ = await page.evaluate("document.scrollingElement.scrollTop = 420; return true")
        try await Task.sleep(for: .milliseconds(50))
        let probe = """
            const rows = Array.from(document.querySelectorAll('#rows > .row'));
            const r = rows.find(r => r.getBoundingClientRect().top >= 0);
            return r.dataset.id + '@' + Math.round(r.getBoundingClientRect().top)
            """
        let before = await page.evaluate(probe) as? String ?? ""
        let id = before.split(separator: "@").first.map(String.init) ?? ""

        ConversationPage.fixtureRows["older"] = ConversationRows.rows(Self.longSession(1...120))
        page.pushFixture()
        try await Task.sleep(for: .milliseconds(100))
        let after = await page.evaluate("""
            const r = document.querySelector('#rows > [data-id="\(id)"]');
            return r ? '\(id)@' + Math.round(r.getBoundingClientRect().top) : 'gone'
            """) as? String
        let top = { (s: String?) in s.flatMap { Int($0.split(separator: "@").last ?? "") } }
        #expect(after?.hasPrefix(id + "@") == true, "still on the page: \(after ?? "nil")")
        if let was = top(before), let now = top(after) {
            #expect(abs(now - was) <= 1, "the row in view stays put: \(before) → \(after ?? "nil")")
        }
        let count = await page.evaluate("return document.querySelectorAll('#rows > .row').length") as? Int
        #expect(count == ConversationRows.rows(Self.longSession(1...120)).count)
    }
}

extension ConversationPageTests {
    /// What counts as rows read above: the page's rows following on, with
    /// none, one or two of its first rows absorbed into a new row above.
    @Test func prependedRowsAreToldApartFromAReset() {
        #expect(ConversationPage.prepended([10, 11, 12], [1, 2, 10, 11, 12]) ?? (-1, -1) == (0, 2))
        #expect(ConversationPage.prepended([10, 11, 12], [1, 2, 10, 11, 12, 13]) ?? (-1, -1) == (0, 2))
        // The first row (a command) joined a group that starts above it.
        #expect(ConversationPage.prepended([51, 52, 53], [1, 50, 52, 53]) ?? (-1, -1) == (1, 2))
        // Nothing read above, or rows in another order: not a prepend.
        #expect(ConversationPage.prepended([10, 11], [10, 11, 12]) == nil)
        #expect(ConversationPage.prepended([10, 11, 12], [1, 11, 10, 12]) == nil)
        #expect(ConversationPage.prepended([10, 11, 12], [1, 12]) ?? (-1, -1) == (2, 1), "two absorbed")
        #expect(ConversationPage.prepended([10, 11, 12, 13], [1, 13]) == nil, "more than two: start again")
    }
}

extension ConversationPageTests {
    /// A task status is one activity line (never a bare "status"), with
    /// its checklist ticked as done and its links.
    @Test func aStatusIsAnActivityLine() async throws {
        let page = try await page(["latest"])
        _ = await page.evaluate("""
            window.senate.apply({ type: "reset", state: {}, rows: [
              { id: 2, kind: "status", by: "announceseats", when: "2 days ago", status: {
                state: "processing", line: "working on it — Cover the uplink reconnect cycle",
                steps: [{ label: "Reproduce", done: true }, { label: "Fix", done: false }],
                links: [{ label: "PR", url: "https://example.test/pr/1" }] } } ] });
            return true
            """)
        let line = await page.evaluate("return document.querySelector('.k-status .status-line').textContent") as? String ?? ""
        #expect(line.contains("announceseats") && line.contains("working on it — Cover the uplink reconnect cycle"))
        let done = await page.evaluate("return document.querySelectorAll('.k-status .status-steps li.done').length") as? Int
        #expect(done == 1)
        let link = await page.evaluate("return document.querySelector('.k-status .status-links a').getAttribute('href')") as? String
        #expect(link == "https://example.test/pr/1")
    }
}

extension ConversationPageTests {
    /// The page never scrolls sideways: long unbroken tokens wrap, wide
    /// tables and code scroll inside their own box, a long command title
    /// is cut, at every width down to the window's narrowest.
    @Test func thePageNeverScrollsSideways() async throws {
        let token = String(repeating: "x", count: 400)
        let table = "| " + (1...30).map { "column\($0)" }.joined(separator: " | ") + " |\n|"
            + String(repeating: "---|", count: 30) + "\n| " + (1...30).map { "v\($0)" }.joined(separator: " | ") + " |"
        let code = "```\n" + String(repeating: "let wide = 1; ", count: 60) + "\n```"
        let page = try await page(["A token: \(token)", table, code, "`\(token)`"])
        let call = { (seq: UInt64, title: String) in
            TimelineEntry(seq: seq, at: nil, truncated: false, body: .tool(call: ToolCall(
                callId: "c\(seq)", name: "Bash", kind: "shell", status: "completed", statusLabel: "completed",
                title: title, cwd: nil, output: token, exitCode: 0, diff: nil, mcp: nil)))
        }
        let rows = ConversationRows.rows([
            call(1, "cargo test " + token), call(2, "git status " + token),
            TimelineEntry(seq: 3, at: nil, truncated: false, body: .user(text: token)),
            call(4, "ls " + token),
        ])
        ConversationPage.fixtureRows["wide"] = rows
        for width in [360.0, 480.0, 700.0, 1000.0] {
            page.web.setFrameSize(NSSize(width: width, height: 600))
            try await Task.sleep(for: .milliseconds(80))
            let sizes = await page.evaluate("""
                const s = document.scrollingElement;
                return s.scrollWidth + '/' + s.clientWidth
                """) as? String ?? ""
            let parts = sizes.split(separator: "/").compactMap { Int($0) }
            #expect(parts.count == 2 && parts[0] <= parts[1], "messages at \(width): \(sizes)")
        }
        let guarded = await page.evaluate("""
            const s = getComputedStyle(document.documentElement);
            return s.overscrollBehaviorX + '/' + s.overflowX
            """) as? String
        #expect(guarded == "none/hidden", "no sideways bounce: \(guarded ?? "nil")")
        page.show(.fixtureRows("wide"))
        for _ in 0..<100 {
            if await page.evaluate("return document.querySelectorAll('.k-group').length") as? Int == 1 { break }
            try await Task.sleep(for: .milliseconds(20))
        }
        _ = await page.evaluate("""
            document.querySelectorAll('.k-group .tool-head, .k-tool .tool-head').forEach(h => h.click());
            return true
            """)
        for width in [360.0, 700.0] {
            page.web.setFrameSize(NSSize(width: width, height: 600))
            try await Task.sleep(for: .milliseconds(80))
            let sizes = await page.evaluate("""
                const s = document.scrollingElement;
                return s.scrollWidth + '/' + s.clientWidth
                """) as? String ?? ""
            let parts = sizes.split(separator: "/").compactMap { Int($0) }
            #expect(parts.count == 2 && parts[0] <= parts[1], "commands at \(width): \(sizes)")
        }
    }
}

extension ConversationPageTests {
    /// A Senate card's body is Markdown, as every other message: inline
    /// code, a fenced block and a list, not literal backticks.
    @Test func aSenateCardsBodyIsMarkdown() async throws {
        let page = ConversationPage(store: nil)
        for _ in 0..<500 where !page.isReady { try await Task.sleep(for: .milliseconds(20)) }
        let text = "Ping `ibot` about it:\n\n- first\n- second\n\n```sh\ncargo test -p ibot\n```"
        let rows = ConversationRows.rows([
            TimelineEntry(seq: 1, at: nil, truncated: false, body: .senate(message: SenateLine(
                committee: "senate", seq: 140, thread: 140, from: "wendell", kind: "note",
                kindLabel: "note", text: text))),
        ])
        ConversationPage.fixtureRows["card"] = rows
        page.show(.fixtureRows("card"))
        for _ in 0..<200 {
            if await page.evaluate("return document.querySelectorAll('.senate').length") as? Int == 1 { break }
            try await Task.sleep(for: .milliseconds(20))
        }
        _ = await page.evaluate("await window.senate.renderAll(); return true")
        let shape = await page.evaluate("""
            const b = document.querySelector('.senate .senate-body');
            return [b.querySelectorAll(':not(pre) > code').length, b.querySelectorAll('li').length,
                    b.querySelectorAll('pre').length, b.textContent.includes('`')].join(',')
            """) as? String
        #expect(shape == "1,2,1,false", "inline code, two items, a block, no backticks: \(shape ?? "nil")")
    }
}

extension ConversationPageTests {
    /// An MCP call reads as its mark and `server.tool`, its raw name in
    /// the tooltip; a run of them carries the mark too.
    @Test func anMcpCallReadsAsServerDotTool() async throws {
        let page = ConversationPage(store: nil)
        for _ in 0..<500 where !page.isReady { try await Task.sleep(for: .milliseconds(20)) }
        let call = { (seq: UInt64, name: String, title: String) in
            TimelineEntry(seq: seq, at: nil, truncated: false, body: .tool(call: ToolCall(
                callId: "c\(seq)", name: name, kind: "other", status: "completed", statusLabel: "completed",
                title: title, cwd: nil, output: nil, exitCode: nil, diff: nil, mcp: "senate.answer")))
        }
        let rows = ConversationRows.rows([
            call(1, "mcp__senate__answer", "senate.answer"),
            TimelineEntry(seq: 2, at: nil, truncated: false, body: .user(text: "and then")),
            call(3, "mcp__senate__answer", "thread 2419"), call(4, "mcp__senate__answer", "senate.answer"),
        ])
        ConversationPage.fixtureRows["mcp"] = rows
        page.show(.fixtureRows("mcp"))
        for _ in 0..<200 {
            if await page.evaluate("return document.querySelectorAll('.row').length") as? Int == 3 { break }
            try await Task.sleep(for: .milliseconds(20))
        }
        let single = await page.evaluate("""
            const h = document.querySelector('.k-tool .tool-head');
            return [!!h.querySelector('svg.mcp-mark'), h.querySelector('.title').textContent,
                    h.querySelector('.title').title].join('|')
            """) as? String
        #expect(single == "true|senate.answer|mcp__senate__answer")
        let group = await page.evaluate("""
            const h = document.querySelector('.group-head');
            return [!!h.querySelector('svg.mcp-mark'), h.querySelector('.title').textContent].join('|')
            """) as? String
        #expect(group == "true|senate.answer", "\(group ?? "nil")")
    }
}
