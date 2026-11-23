import AppKit
import Foundation
import Testing
import WebKit
@testable import Senate

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
