import Foundation
import Testing
@testable import Senate
import SenateCore

/// The scene shell's guards, for any scene: where the bearer may go, how a
/// hostile `Retry-After` is read, which grants are used, what a scene may
/// store (security reviews 2 and 3: M2, N3, L3, L4, L5, in their generic form).
@MainActor
@Suite struct SceneShellTests {
    let core = FakeCore()
    let defaults: UserDefaults

    init() {
        let suite = "senate-scene-tests-\(UUID().uuidString)"
        defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
    }

    func store(_ library: SceneLibrary = SceneFixture.library()) async throws -> CoreStore {
        let core = self.core
        let store = CoreStore(makeCore: { core }, defaults: defaults, notifications: false, scenes: library)
        core.push(online([]))
        for _ in 0..<200 where !store.state.online { try await Task.sleep(for: .milliseconds(5)) }
        return store
    }

    /// The fixture installed in a fresh library (a new identity each time).
    func installed(_ library: SceneLibrary, manifest: String? = nil) throws -> InstalledScene {
        let folder = try SceneFixture.copy()
        if let manifest { try Data(manifest.utf8).write(to: folder.appendingPathComponent("manifest.json")) }
        return try library.install(try library.inspect(folder))
    }

    // M2: the bearer goes only to a numeric loopback address with a port, nothing else in it.
    @Test func theBearerGoesOnlyToALoopbackEndpoint() {
        for good in ["http://127.0.0.1:7199", "http://127.0.0.1:7199/", "http://[::1]:7199"] {
            #expect(ObserveClient.vouched(good) != nil, "\(good)")
        }
        for bad in ["http://localhost:7199", "https://127.0.0.1:7199", "http://127.0.0.1", "http://10.0.0.2:7199",
                    "http://evil.example:7199", "http://127.0.0.1:7199/v1", "http://u:p@127.0.0.1:7199",
                    "http://127.0.0.1:7199/?x=1", "http://127.0.0.1.evil.example:7199", "file:///tmp/x"] {
            #expect(ObserveClient.vouched(bad) == nil, "\(bad)")
        }
    }

    // N3 (review 3): a hostile Retry-After never reaches a sleep that traps.
    @Test func retryAfterIsParsedDefensively() {
        #expect(ObserveClient.retryAfterSeconds("5") == 5)
        #expect(ObserveClient.retryAfterSeconds(" 0 ") == 1)
        #expect(ObserveClient.retryAfterSeconds("99999") == 300)
        for bad in ["nan", "NaN", "inf", "-inf", "-5", "1e9", "4.5", "Wed, 21 Oct 2015 07:28:00 GMT", "", "9999999999999999999", "٣"] {
            #expect(ObserveClient.retryAfterSeconds(bad) == nil, "\(bad)")
        }
        #expect(ObserveClient.retryAfterSeconds(nil) == nil)
    }

    // L3: a grant planted in the Keychain is not used unless the daemon issued it to this scene.
    @Test func aPlantedGrantIsIgnored() throws {
        let library = SceneFixture.library()
        let scene = try installed(library), other = try installed(library)
        let live = ObserveGrantInfo(id: "g1", name: scene.grantName, expiresAt: "2099-01-01T00:00:00Z")
        let mine = SceneAccess.Grant(token: "obs1_x", id: "g1", installation: "this-mac", expiresAt: "2099-01-01T00:00:00Z",
                                     name: scene.grantName, topology: false)
        #expect(SceneAccess.vouched(mine, grants: [live], installation: "this-mac", scene: scene))
        var planted = mine
        planted.id = "unknown"
        #expect(!SceneAccess.vouched(planted, grants: [live], installation: "this-mac", scene: scene))
        #expect(!SceneAccess.vouched(mine, grants: [live], installation: "another-mac", scene: scene))
        // A grant the daemon issued under another name, or to another scene, is not this scene's.
        #expect(!SceneAccess.vouched(mine, grants: [ObserveGrantInfo(id: "g1", name: "Town", expiresAt: "2099-01-01T00:00:00Z")],
                                     installation: "this-mac", scene: scene))
        #expect(!SceneAccess.vouched(mine, grants: [live], installation: "this-mac", scene: other))
        var old = mine
        old.expiresAt = "2000-01-01T00:00:00Z"
        #expect(!SceneAccess.vouched(old, grants: [live], installation: "this-mac", scene: scene))
        var junk = mine
        junk.expiresAt = "not a date"
        #expect(junk.expired)
    }

    // L4: what a scene stores is plain JSON; NaN, infinities and dates are refused, not serialised.
    @Test func aSceneStoresOnlyPlainJSON() {
        #expect(SceneSession.plainJSON(["calm": true, "n": 1.5, "s": "x", "a": [1, NSNull()]] as [String: Any]))
        #expect(!SceneSession.plainJSON(Double.nan))
        #expect(!SceneSession.plainJSON(Double.infinity))
        #expect(!SceneSession.plainJSON(Date()))
        #expect(!SceneSession.plainJSON(["x": [Double.nan]] as [String: Any]))
        var deep: Any = 1
        for _ in 0..<40 { deep = [deep] }
        #expect(!SceneSession.plainJSON(deep))
    }

    // MARK: Grants, per scene

    // L5: allowing a scene issues one grant of exactly its scope; allowing it
    // again revokes its earlier ones and no one else's.
    @Test func oneLiveGrantPerScene() async throws {
        let library = SceneFixture.library()
        let store = try await store(library)
        let scene = try installed(library)
        let senate = store.state.senates.first?.key ?? "local"
        core.observeGrantList = [
            ObserveGrantInfo(id: "old-1", name: scene.grantName, expiresAt: "2099-01-01T00:00:00Z"),
            ObserveGrantInfo(id: "someone", name: "my scene", expiresAt: "2099-01-01T00:00:00Z"),
            ObserveGrantInfo(id: "town", name: "Town", expiresAt: "2099-01-01T00:00:00Z"),
        ]
        let first = try await SceneAccess.allow(scene, senate: senate, core: core)
        #expect(core.calls.contains("observe grant \(scene.grantName)"), "metadata, no topology: the fixture does not ask")
        #expect(core.calls.contains("observe revoke old-1"))
        #expect(!core.calls.contains("observe revoke someone") && !core.calls.contains("observe revoke town"))
        #expect(SceneAccess.load(senate, scene: scene.id) == first)
        let second = try await SceneAccess.allow(scene, senate: senate, core: core)
        #expect(core.calls.contains("observe revoke \(first.id)"))
        #expect(SceneAccess.issued(to: scene, in: core.observeGrantList).map(\.id) == [second.id])
        await SceneAccess.removeAll(scene: scene.id, core: core)
        #expect(core.calls.contains("observe revoke \(second.id)"))
        #expect(SceneAccess.load(senate, scene: scene.id) == nil)
    }

    @Test func topologyOnlyWhenAsked() async throws {
        let library = SceneFixture.library()
        let store = try await store(library)
        let linked = try installed(library, manifest: SceneFixture.manifest(["observe": ["major": 1, "required": [], "optional": ["topology"]]]))
        let grant = try await SceneAccess.allow(linked, senate: store.state.senates.first?.key ?? "local", core: core)
        #expect(grant.topology)
        #expect(core.calls.contains("observe grant \(linked.grantName) topology"))
        await SceneAccess.removeAll(scene: linked.id, core: core)
    }

    @Test func removingASceneRevokesItsGrantAndDeletesItsStorage() async throws {
        let library = SceneFixture.library()
        let store = try await store(library)
        let scene = try installed(library)
        let senate = store.state.senates.first?.key ?? "local"
        _ = try await SceneAccess.allow(scene, senate: senate, core: core)
        let storage = SceneStorage(folder: library.storageFolder(scene.id), senate: senate)
        #expect(storage.set("k", json: "1"))
        #expect(FileManager.default.fileExists(atPath: library.storageFolder(scene.id).path))
        await SceneAccess.removeAll(scene: scene.id, core: core)
        try library.remove(scene.id)
        #expect(SceneAccess.issued(to: scene, in: core.observeGrantList).isEmpty)
        #expect(SceneAccess.load(senate, scene: scene.id) == nil)
        #expect(!FileManager.default.fileExists(atPath: library.storageFolder(scene.id).path))
        #expect(library.scene(scene.id) == nil)
        // Installed again, it is a new scene: nothing of the old one comes back.
        let again = try installed(library)
        #expect(again.id != scene.id)
        #expect(SceneStorage(folder: library.storageFolder(again.id), senate: senate).get("k") == nil)
    }

    // MARK: Two scenes at once

    @Test func twoScenesAreIsolated() async throws {
        let library = SceneFixture.library()
        let store = try await store(library)
        let a = try installed(library), b = try installed(library)
        #expect(a.id != b.id && a.manifest == b.manifest, "the same bundle twice: two identities")
        let senate = store.state.senates.first?.key ?? "local"
        // Grants: separate, and revoking one leaves the other.
        let ga = try await SceneAccess.allow(a, senate: senate, core: core)
        let gb = try await SceneAccess.allow(b, senate: senate, core: core)
        #expect(ga.id != gb.id && ga.token != gb.token)
        await SceneAccess.revoke(a, senate: senate, core: core)
        #expect(SceneAccess.load(senate, scene: a.id) == nil && SceneAccess.load(senate, scene: b.id) == gb)
        #expect(!SceneAccess.vouched(gb, grants: core.observeGrantList, installation: "this-mac", scene: a))
        // Storage: one file each, and per Senate.
        let sa = SceneStorage(folder: library.storageFolder(a.id), senate: senate)
        let sb = SceneStorage(folder: library.storageFolder(b.id), senate: senate)
        let sa2 = SceneStorage(folder: library.storageFolder(a.id), senate: "another")
        #expect(sa.set("k", json: #""a""#))
        #expect(sb.get("k") == nil && sa2.get("k") == nil)
        #expect(SceneStorage(folder: library.storageFolder(a.id), senate: senate).get("k") == #""a""#)
        // Messages: each page has its own session and origin; one's traffic never reaches the other.
        let pa = store.scenePage(a).page, pb = store.scenePage(b).page
        #expect(pa.session !== pb.session)
        #expect(SceneScheme.resolve(SceneScheme.url(scene: b.id, path: "index.html"), scene: a.id, root: a.root) == nil)
        await SceneAccess.removeAll(scene: b.id, core: core)
        store.dropScenePage(a.id)
        store.dropScenePage(b.id)
    }

    // MARK: Installing, updating

    @Test func updateKeepsIdentityAndShowsWhatChanged() async throws {
        let library = SceneFixture.library()
        let scene = try installed(library)
        let storage = SceneStorage(folder: library.storageFolder(scene.id), senate: "local")
        #expect(storage.set("kept", json: "true"))
        let next = try SceneFixture.copy()
        let wider = SceneFixture.manifest(["version": "1.1.0", "observe": ["major": 1, "required": ["state_patch"], "optional": ["topology", "desk"]],
                                           "cards": ["reads": ["fixture.badge/v1"], "writes": []]])
        try Data(wider.utf8).write(to: next.appendingPathComponent("manifest.json"))
        let candidate = try library.inspect(next)
        let change = SceneAsksChange(from: scene.manifest, to: candidate.manifest)
        #expect(change.addedCapabilities == ["topology", "desk"] && change.removedCapabilities == ["app_links"])
        #expect(change.addedReads == ["fixture.badge/v1"] && change.removedWrites == ["fixture.badge/v1"] && change.topologyChanged)
        try library.update(scene.id, with: candidate)
        let updated = try #require(library.scene(scene.id))
        #expect(updated.manifest.version == "1.1.0" && updated.digest != scene.digest)
        #expect(SceneStorage(folder: library.storageFolder(scene.id), senate: "local").get("kept") == "true")
        #expect(library.scenes.count == 1)
    }

    @Test func aChangedInstalledFileMakesTheSceneUnreadable() throws {
        let library = SceneFixture.library()
        let scene = try installed(library)
        try Data("<!doctype html><script src=x.js></script>".utf8).write(to: scene.root.appendingPathComponent("index.html"))
        library.reload()
        #expect(library.scene(scene.id) == nil && library.damaged == [scene.id])
        try library.remove(scene.id)
        #expect(library.damaged.isEmpty)
    }

    @Test func aDevelopmentSceneIsLoadedInPlaceAndNotRemovable() throws {
        let folder = try SceneFixture.copy()
        let library = SceneLibrary(root: SceneFixture.scratch("library"), development: folder)
        let scene = try #require(library.scenes.first)
        #expect(scene.development && scene.root == folder && InstalledScene.validID(scene.id))
        try library.remove(scene.id)
        #expect(FileManager.default.fileExists(atPath: folder.path))
        #expect(SceneLibrary(root: SceneFixture.scratch("library"), development: folder).scenes.first?.id == scene.id)
    }
}
