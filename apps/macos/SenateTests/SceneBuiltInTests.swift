import Foundation
import Testing
@testable import Senate

/// Scenes the app carries (`Resources/Scenes/*.senatescene`): installed at
/// launch through the same checks as Add Scene, once; removed ones stay
/// removed and are offered; newer versions update only when they ask for
/// nothing more; a bad archive is skipped.
@MainActor
@Suite struct SceneBuiltInTests {
    let scenes = SceneFixture.scratch("library")
    let bundled = SceneFixture.scratch("bundled")

    /// Put `<key>.senatescene` among the app's built-in scenes.
    func ship(_ key: String = "fixture", manifest: String = SceneFixture.manifest()) throws {
        var zip = ZipBuilder()
        zip.add("index.html", "<!doctype html><p>hi</p>")
        zip.add("manifest.json", manifest)
        try zip.build().write(to: bundled.appendingPathComponent("\(key).senatescene"))
    }

    /// The app launching: a library over the same folders, then the built-in pass.
    func launch() -> SceneLibrary {
        let library = SceneLibrary(root: scenes, development: nil, builtIn: bundled)
        library.installBuiltIn()
        return library
    }

    @Test func firstLaunchInstallsOnceWithNoAccess() throws {
        try ship()
        let first = launch()
        let scene = try #require(first.scenes.first)
        #expect(first.scenes.count == 1)
        #expect(scene.builtIn == "fixture" && !scene.development && InstalledScene.validID(scene.id))
        #expect(scene.manifest.name == "Fixture")
        #expect(SceneAccess.load("local", scene: scene.id) == nil)
        #expect(first.offered.isEmpty)

        let again = launch()
        #expect(again.scenes.map(\.id) == [scene.id])
        #expect(again.scenes.first?.digest == scene.digest)
    }

    @Test func aRemovedBuiltInStaysRemovedAndIsOffered() throws {
        try ship()
        let library = launch()
        let scene = try #require(library.scenes.first)
        try library.remove(scene.id)
        #expect(library.scenes.isEmpty)
        #expect(library.offered.map(\.key) == ["fixture"])
        #expect(library.offered.first?.name == "Fixture")

        let relaunched = launch()
        #expect(relaunched.scenes.isEmpty)
        let offer = try #require(relaunched.offered.first)

        // Add: the same path as any scene, under a new identity, and no longer offered.
        let added = try relaunched.install(try relaunched.inspect(offer.archive, builtIn: offer.key))
        #expect(added.id != scene.id && added.builtIn == "fixture")
        #expect(relaunched.offered.isEmpty)
        #expect(launch().scenes.map(\.id) == [added.id])
    }

    @Test func aNewerVersionAskingNoMoreUpdatesInPlace() throws {
        try ship()
        let old = try #require(launch().scenes.first)
        try ship(manifest: SceneFixture.manifest(["version": "1.1.0"]))
        let library = launch()
        let updated = try #require(library.scenes.first)
        #expect(library.scenes.count == 1)
        #expect(updated.id == old.id && updated.builtIn == "fixture")
        #expect(updated.manifest.version == "1.1.0" && updated.digest != old.digest)
        #expect(library.waitingUpdates.isEmpty)
    }

    @Test func aNewerVersionAskingMoreWaitsForTheSheet() throws {
        try ship()
        let old = try #require(launch().scenes.first)
        let more: [String: Any?] = [
            "version": "2.0.0",
            "observe": ["major": 1, "required": ["state_patch"], "optional": ["app_links", "topology"]],
        ]
        try ship(manifest: SceneFixture.manifest(more))
        let library = launch()
        let kept = try #require(library.scenes.first)
        #expect(kept.manifest.version == "1.0.0" && kept.digest == old.digest)
        #expect(library.waitingUpdates[old.id] == bundled.appendingPathComponent("fixture.senatescene"))

        // The person's Update: the same update path, identity and built-in mark kept.
        let candidate = try library.inspect(try #require(library.waitingUpdates[old.id]), builtIn: kept.builtIn)
        try library.update(old.id, with: candidate)
        #expect(library.scene(old.id)?.manifest.version == "2.0.0")
        #expect(library.scene(old.id)?.builtIn == "fixture")
        #expect(library.waitingUpdates.isEmpty)
    }

    @Test func anOlderOrSameVersionChangesNothing() throws {
        try ship(manifest: SceneFixture.manifest(["version": "1.2.0"]))
        let installed = try #require(launch().scenes.first)
        try ship(manifest: SceneFixture.manifest(["version": "1.1.9", "name": "Older"]))
        let library = launch()
        #expect(library.scenes.first?.digest == installed.digest)
        #expect(library.waitingUpdates.isEmpty)
    }

    @Test func anInvalidArchiveIsSkipped() throws {
        try Data("not a zip".utf8).write(to: bundled.appendingPathComponent("broken.senatescene"))
        try ship("bad-manifest", manifest: SceneFixture.manifest(["entry": nil]))
        try ship()
        let library = launch()
        #expect(library.scenes.map(\.builtIn) == ["fixture"])
        #expect(library.offered.isEmpty && library.damaged.isEmpty)
        let leftovers = try FileManager.default.contentsOfDirectory(atPath: scenes.path).filter { $0.hasPrefix(".") }
        #expect(leftovers.isEmpty)
    }

    @Test func noBuiltInScenesChangesNothing() {
        let library = SceneLibrary(root: scenes, development: nil, builtIn: nil)
        library.installBuiltIn()
        #expect(library.scenes.isEmpty && library.offered.isEmpty)
        let missing = SceneLibrary(root: scenes, development: nil, builtIn: bundled.appendingPathComponent("none"))
        missing.installBuiltIn()
        #expect(missing.scenes.isEmpty)
    }

    @Test func versionsCompareBySemver() {
        #expect(SceneVersion.newer("1.10.0", than: "1.9.0"))
        #expect(SceneVersion.newer("1.0.0", than: "1.0.0-beta.2"))
        #expect(SceneVersion.newer("1.0.0-beta.10", than: "1.0.0-beta.2"))
        #expect(!SceneVersion.newer("1.0.0", than: "1.0.0"))
        #expect(!SceneVersion.newer("0.9.9", than: "1.0.0"))
    }
}
