import AppKit
import Foundation
import Testing
import WebKit
@testable import Senate
import SenateCore

/// The scene host contract, as the fixture scene and a scripted scene see it:
/// the handshake, the snapshot before events, the environment, storage and
/// its quotas, `open` to the app's router, card writes asked and declined,
/// and the shell's refusals (shape, size, rate, stale documents).
@MainActor
@Suite struct SceneHostTests {
    /// A session on the fixture's manifest with every hook recorded.
    final class Harness {
        var sent: [[String: Any]] = []
        var opened: [(String, Bool)] = []
        var openAnswer = true
        var confirms: [SceneSession.CardRequest] = []
        var confirmAnswer = true
        var written: [String] = []
        var now = ContinuousClock.now
        let session: SceneSession

        @MainActor
        init(manifest: SceneManifest? = nil, storage: SceneStorage? = nil) throws {
            let manifest = try manifest ?? SceneManifest.parse(SceneFixture.manifest)
            let scene = InstalledScene(id: String(repeating: "c", count: 32), manifest: manifest, root: SceneFixture.folder,
                                       digest: "", installedAt: Date(), development: false)
            let storage = storage ?? SceneStorage(folder: SceneFixture.scratch("storage"), senate: "local")
            var me: Harness?
            session = SceneSession(scene: scene, storage: storage, hooks: .init(
                send: { me?.sent.append($0) },
                open: { url, gesture in me?.opened.append((url, gesture)); return me?.openAnswer ?? false },
                confirmCard: { request in me?.confirms.append(request); return me?.confirmAnswer ?? false },
                writeCard: { me?.written.append($0) },
                clock: { me?.now ?? .now }
            ))
            me = self
        }

        func say(_ type: String, _ data: Any? = nil, id: String? = nil, activated: Bool = false) {
            var m: [String: Any] = ["host_version": 1, "type": type]
            if let data { m["data"] = data }
            if let id { m["request_id"] = id }
            session.receive(m, activated: activated)
        }

        func types() -> [String] { sent.compactMap { $0["type"] as? String } }

        /// The result for a request id: (ok, value, code).
        func result(_ id: String) -> (Bool, Any?, String?)? {
            guard let m = sent.last(where: { $0["type"] as? String == "result" && $0["request_id"] as? String == id }),
                  let d = m["data"] as? [String: Any] else { return nil }
            return (d["ok"] as? Bool ?? false, d["value"], d["code"] as? String)
        }

        func code(_ id: String) -> String? { result(id)?.2 }

        func ready() { say("ready", ["supported_host_versions": [1]], id: "hello") }

        /// Let a scheduled task (a card write) finish.
        func settle() async { for _ in 0..<20 { await Task.yield() } }
    }

    static let snapshot: [String: Any] = [
        "schema_version": 1, "capabilities": ["state_patch", "app_links"], "cursor": "AAAAAAAAAAAAAAAAAAAAAA",
        "senate": ["id": "s", "name": "alice", "link": NSNull(),
                   "person": ["id": "p", "name": "alice", "link": NSNull(),
                              "card": ["revision": "1", "updated_at": "2026-11-25T00:00:00Z", "value": ["other.app/v1": ["x": 1]]]]],
        "committees": [["id": "senate", "name": "senate", "archived": false, "link": "senate://open/v1/committee/senate"]],
    ]

    // MARK: Handshake

    @Test func nothingIsAnsweredBeforeTheHandshake() throws {
        let h = try Harness()
        h.say("storage.get", ["key": "k"], id: "1")
        h.session.status("ready")
        h.session.snapshot(Self.snapshot)
        h.session.environment(["visible": true])
        #expect(h.sent.isEmpty)
    }

    @Test func theHandshakeAgreesOnAVersionAndSaysWhatIsOffered() throws {
        let h = try Harness()
        var started = 0
        h.session.onReady = { started += 1 }
        h.ready()
        let (ok, value, _) = try #require(h.result("hello"))
        let offered = try #require(value as? [String: Any])
        #expect(ok && offered["version"] as? Int == 1)
        #expect(offered["capabilities"] as? [String] == ["open", "storage", "card.write"])
        #expect(h.types() == ["result", "status"] && (h.sent[1]["data"] as? [String: Any])?["state"] as? String == "data_unavailable")
        #expect(started == 1)
        #expect(h.sent.allSatisfy { $0["host_version"] as? Int == 1 })
    }

    @Test func anUnsupportedVersionIsRefusedOnce() throws {
        let h = try Harness()
        h.say("ready", ["supported_host_versions": [2]], id: "hello")
        #expect(h.code("hello") == "unsupported")
        #expect(!h.session.ready)
        h.say("storage.get", ["key": "k"], id: "after")
        #expect(h.result("after") == nil)
    }

    @Test func malformedMessagesAreIgnoredOrRefused() throws {
        let h = try Harness()
        h.ready()
        let before = h.sent.count
        for bad: Any in [
            "text", 5, ["host_version": 2, "type": "storage.get", "request_id": "x"],
            ["host_version": true, "type": "storage.get", "request_id": "x"],
            ["host_version": 1, "type": "storage.get", "request_id": 7, "data": ["key": "k"]],
            ["host_version": 1, "type": "storage.get", "request_id": String(repeating: "x", count: 65)],
            ["host_version": 1, "type": "storage.get", "request_id": "x", "extra": 1],
            ["host_version": 1, "type": String(repeating: "t", count: 65)],
        ] {
            h.session.receive(bad, activated: false)
        }
        #expect(h.sent.count == before)
        h.say("storage.get", "not an object", id: "shape")
        #expect(h.code("shape") == "invalid_request")
        h.say("teleport", [:], id: "unknown")
        #expect(h.code("unknown") == "unsupported")
        h.say("storage.set", ["key": "big", "value": String(repeating: "x", count: 70_000)], id: "huge")
        #expect(h.code("huge") == "invalid_request")
    }

    @Test func requestsAreRateLimited() throws {
        let h = try Harness()
        h.ready()
        for i in 0..<45 { h.say("storage.get", ["key": "k"], id: "r\(i)") }
        #expect(h.code("r39") == nil && h.code("r40") == "quota")
        h.now += .seconds(1)
        h.say("storage.get", ["key": "k"], id: "later")
        #expect(h.result("later")?.0 == true)
    }

    // MARK: Observe data

    @Test func theSnapshotComesBeforeEvents() throws {
        let h = try Harness()
        h.ready()
        #expect(!h.session.frame(["kind": "message.posted"]))
        #expect(h.session.snapshot(Self.snapshot))
        #expect(h.session.frame(["kind": "message.posted", "cursor": "x"]))
        h.session.status("ready")
        #expect(h.types().suffix(3) == ["observe.snapshot", "observe.event", "status"])
        // Anything but ready: the next data starts from a new snapshot.
        h.session.status("data_unavailable")
        #expect(!h.session.frame(["kind": "message.posted"]))
        // A new document: nothing until it says ready again.
        h.session.reset()
        let count = h.sent.count
        #expect(!h.session.snapshot(Self.snapshot))
        #expect(h.sent.count == count)
    }

    @Test func aSnapshotWithoutTheRequiredCapabilitiesIsNotDelivered() throws {
        let h = try Harness()
        h.ready()
        var thin = Self.snapshot
        thin["capabilities"] = ["app_links"]
        #expect(!h.session.snapshot(thin))
        #expect(!h.types().contains("observe.snapshot"))
        #expect((h.sent.last?["data"] as? [String: Any])?["state"] as? String == "version_mismatch")
    }

    @Test func theEnvironmentGoesThroughAsGiven() throws {
        let h = try Harness()
        h.ready()
        h.session.environment(["color_scheme": "dark", "reduced_motion": true, "locale": "zh-Hans", "time_zone": "Asia/Shanghai",
                               "visible": true, "viewport": ["width": 800, "height": 600]])
        let env = try #require(h.sent.last)
        #expect(env["type"] as? String == "environment")
        #expect((env["data"] as? [String: Any])?["locale"] as? String == "zh-Hans")
    }

    // MARK: Storage

    @Test func storageKeepsJSONWithinItsQuotas() throws {
        let folder = SceneFixture.scratch("storage")
        let h = try Harness(storage: SceneStorage(folder: folder, senate: "local"))
        h.ready()
        h.say("storage.set", ["key": "look", "value": ["hat": 3, "name": "x"]], id: "1")
        h.say("storage.get", ["key": "look"], id: "2")
        #expect(h.result("1")?.0 == true)
        #expect((h.result("2")?.1 as? [String: Any])?["hat"] as? Int == 3)
        h.say("storage.get", ["key": "missing"], id: "3")
        #expect(h.result("3")?.1 is NSNull)
        for (i, key) in ["", "a/b", "ключ", String(repeating: "k", count: 65), "a b"].enumerated() {
            h.say("storage.set", ["key": key, "value": 1], id: "bad\(i)")
            #expect(h.code("bad\(i)") == "invalid_request", "\(key)")
        }
        h.say("storage.set", ["key": "big", "value": String(repeating: "x", count: 16 * 1024)], id: "value")
        #expect(h.code("value") == "quota")
        for i in 0..<5 { h.say("storage.set", ["key": "fill\(i)", "value": String(repeating: "x", count: 15_000)], id: "fill\(i)") }
        #expect(h.result("fill3")?.0 == true && h.code("fill4") == "quota")
        h.say("storage.delete", ["key": "fill0"], id: "del")
        h.say("storage.set", ["key": "fill4", "value": String(repeating: "x", count: 15_000)], id: "again")
        #expect(h.result("again")?.0 == true)
        // Kept: another session of the same scene and Senate reads it back.
        #expect(SceneStorage(folder: folder, senate: "local").get("look") != nil)
        #expect(SceneStorage(folder: folder, senate: "another").get("look") == nil)
    }

    // MARK: Links

    @Test func openGoesToTheRouterWithTheActivationTheAppSaw() throws {
        let h = try Harness()
        h.ready()
        h.say("open", ["url": "senate://open/v1/inbox"], id: "1", activated: true)
        h.say("open", ["url": "senate://open/v1/inbox"], id: "2", activated: false)
        #expect(h.opened.map(\.1) == [true, false] && h.result("1")?.0 == true)
        h.openAnswer = false
        h.say("open", ["url": "javascript:alert(1)"], id: "3")
        #expect(h.code("3") == "invalid_request")
        h.say("open", ["url": String(repeating: "x", count: 2049)], id: "4")
        h.say("open", ["url": 5], id: "5")
        #expect(h.code("4") == "invalid_request" && h.code("5") == "invalid_request")
        for i in 0..<4 { h.say("open", ["url": "senate://open/v1/inbox"], id: "f\(i)") }
        #expect(h.code("f2") == "quota")
    }

    // MARK: Cards

    @Test func aCardWriteIsAskedAndMergedIntoTheCard() async throws {
        let h = try Harness()
        h.ready()
        h.say("card.write", ["schema": "fixture.badge/v1", "value": ["colour": 2]], id: "early")
        #expect(h.code("early") == "unavailable", "without the person's row there is nothing to merge into")
        h.session.snapshot(Self.snapshot)
        h.say("card.write", ["schema": "other.app/v1", "value": ["x": 2]], id: "undeclared")
        #expect(h.code("undeclared") == "invalid_request")
        h.say("card.write", ["schema": "fixture.badge/v1", "value": "text"], id: "notobject")
        #expect(h.code("notobject") == "invalid_request")
        h.say("card.write", ["schema": "fixture.badge/v1", "value": ["colour": 2]], id: "yes")
        await h.settle()
        #expect(h.result("yes")?.0 == true)
        let asked = try #require(h.confirms.first)
        #expect(asked.scene == "Fixture" && asked.schema == "fixture.badge/v1" && asked.value == #"{"colour":2}"#)
        #expect(h.written == [#"{"fixture.badge/v1":{"colour":2},"other.app/v1":{"x":1}}"#])
        // Too large for a card: refused before anyone is asked.
        h.say("card.write", ["schema": "fixture.badge/v1", "value": ["s": String(repeating: "x", count: 2100)]], id: "big")
        #expect(h.code("big") == "quota" && h.confirms.count == 1)
    }

    @Test func aDeclinedCardWriteIsQuietForAMinute() async throws {
        let h = try Harness()
        h.ready()
        h.session.snapshot(Self.snapshot)
        h.confirmAnswer = false
        h.say("card.write", ["schema": "fixture.badge/v1", "value": NSNull()], id: "no")
        await h.settle()
        #expect(h.code("no") == "declined" && h.written.isEmpty)
        h.say("card.write", ["schema": "fixture.badge/v1", "value": NSNull()], id: "again")
        #expect(h.code("again") == "declined" && h.confirms.count == 1)
        h.now += .seconds(61)
        h.confirmAnswer = true
        h.say("card.write", ["schema": "fixture.badge/v1", "value": NSNull()], id: "later")
        await h.settle()
        #expect(h.result("later")?.0 == true && h.written == [#"{"other.app/v1":{"x":1}}"#])
    }

    @Test func cardLimits() {
        #expect(SceneSession.cardDocument([:]) == "null")
        #expect(SceneSession.cardDocument(["a": ["b": 1]]) != nil)
        var deep: [String: Any] = ["v": 1]
        for _ in 0..<8 { deep = ["n": deep] }
        #expect(SceneSession.cardDocument(deep) == nil)
        #expect(SceneSession.cardDocument(["a": (0..<130).map { $0 }]) == nil)
        #expect(SceneSession.cardDocument(["a": String(repeating: "x", count: 2100)]) == nil)
    }

    // MARK: The fixture scene in the real web view

    @Test func theFixtureRunsInTheHostAndOpensThroughTheRouter() async throws {
        let library = SceneFixture.library()
        let core = FakeCore()
        let store = CoreStore(makeCore: { core }, defaults: UserDefaults(suiteName: "scene-host-\(UUID().uuidString)")!,
                              notifications: false, scenes: library)
        core.push(online([]))
        for _ in 0..<200 where !store.state.online { try await Task.sleep(for: .milliseconds(5)) }
        store.links.windowVisible = { true }
        store.links.activate = {}
        let scene = try library.install(try library.inspect(SceneFixture.folder))
        let page = store.scenePage(scene).page
        defer { store.dropScenePage(scene.id) }
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 400, height: 300), styleMask: [.borderless], backing: .buffered, defer: true)
        window.contentView = page.web
        for _ in 0..<300 where !page.ready { try await Task.sleep(for: .milliseconds(20)) }
        try #require(page.ready, "the fixture said ready")
        #expect(page.session.lastStatus == "data_unavailable")

        // Snapshot, then an event: the page shows both.
        #expect(page.session.snapshot(Self.snapshot))
        page.session.frame(["kind": "state.patch", "data": ["upsert": ["committees": [["id": "payments", "name": "payments", "archived": false]]]]])
        page.session.status("ready")
        var text = ""
        for _ in 0..<100 {
            text = (await page.evaluate("return window.fixture.text()") as? String) ?? ""
            if text.contains("payments") { break }
            try await Task.sleep(for: .milliseconds(20))
        }
        #expect(text == "ready|alice|senate|payments")
        let received = (await page.evaluate("return window.fixture.received()") as? [String]) ?? []
        #expect(received.first == "result" && received.contains("environment"))
        #expect(received.contains("observe.snapshot") && received.contains("observe.event"))

        // `open` reaches the app's router (no click seen: a system delivery).
        let opened = await page.evaluate("return await window.fixture.request('open', { url: 'senate://open/v1/inbox' })") as? [String: Any]
        #expect(opened?["ok"] as? Bool == true)
        await store.links.flush()
        #expect(store.links.routed.last == .shown(.section(.inbox)))

        // Its own files, and nothing else: not another scene's, not the disk, not the network.
        func fetch(_ url: String) async -> String {
            (await page.evaluate("try { const r = await fetch(u); return String(r.status) } catch (e) { return 'refused' }", ["u": url]) as? String) ?? "none"
        }
        #expect(await fetch("manifest.json") == "200")
        #expect(await fetch("senate-scene://\(String(repeating: "d", count: 32))/manifest.json") == "refused")
        #expect(await fetch("../../../../etc/hosts") == "refused")
        #expect(await fetch("file:///etc/hosts") == "refused")
        #expect(await fetch("https://example.com/") == "refused")
        // No markup from strings (Trusted Types with no policy).
        let html = await page.evaluate("try { document.body.innerHTML = '<b>x</b>'; return 'allowed' } catch (e) { return 'refused' }") as? String
        #expect(html == "refused")
        // No navigation away from its entry.
        _ = await page.evaluate("location.href = 'senate-scene://\(scene.id)/manifest.json'; return true")
        try await Task.sleep(for: .milliseconds(300))
        #expect(page.web.url == SceneScheme.url(scene: scene.id, path: "index.html"))
        window.contentView = nil
    }
}
