#if DEBUG
import AppKit

/// Development only: with `SENATE_APP_SNAPSHOT=<dir>` and
/// `SENATE_APP_SNAPSHOT_SCENES=1`, picture the Scenes place and a scene, and
/// quit. Run it with `SENATE_APP_SCENES_DIR` set to an empty scratch folder
/// (the scenes it installs go there, never into Application Support).
///
/// - `scenes-empty`: the Scenes place with nothing installed.
/// - with `SENATE_APP_SNAPSHOT_SCENE=<bundle folder or .senatescene>`:
///   `scenes-add` (the add sheet for it), then it is installed and
///   `scene-access` shows its page asking for access.
/// - with `SENATE_APP_SNAPSHOT_SCENE_GRANT=1` as well (only against the
///   fake daemon, which grants without a person): `scene-running` (the scene
///   fed by Observe) and `scene-list` (the accessible list).
enum SceneSnapshot {
    static let env = "SENATE_APP_SNAPSHOT_SCENES"

    static func run(_ store: CoreStore, shot: (String) async -> Void) async {
        let env = ProcessInfo.processInfo.environment
        store.select(.scenes)
        await shot("scenes-empty")
        guard let path = env["SENATE_APP_SNAPSHOT_SCENE"], !path.isEmpty else { return }
        let url = URL(fileURLWithPath: path)
        NotificationCenter.default.post(name: ScenesView.inspectNote, object: url)
        await shot("scenes-add")
        NotificationCenter.default.post(name: ScenesView.inspectNote, object: nil)
        try? await Task.sleep(for: .milliseconds(600))
        guard let candidate = try? store.scenes.inspect(url), let scene = try? store.scenes.install(candidate) else {
            print("snapshot: the scene could not be installed")
            return
        }
        store.select(.scene(scene.id))
        for _ in 0..<100 where SceneWebPage.shown?.ready != true { try? await Task.sleep(for: .milliseconds(100)) }
        guard let page = SceneWebPage.shown else { return print("snapshot: no scene page") }
        for _ in 0..<50 where !page.accessNeeded { try? await Task.sleep(for: .milliseconds(100)) }
        await shot("scene-access")
        guard env["SENATE_APP_SNAPSHOT_SCENE_GRANT"] == "1" else { return }
        do { try await page.grantAccess() } catch { print("snapshot: no grant: \(error)") }
        for _ in 0..<100 where page.session.lastStatus != "ready" { try? await Task.sleep(for: .milliseconds(100)) }
        print("snapshot: scene status \(page.session.lastStatus ?? "none")")
        await shot("scene-running")
        NotificationCenter.default.post(name: ScenePage.showListNote, object: true)
        await shot("scene-list")
        NotificationCenter.default.post(name: ScenePage.showListNote, object: nil)
    }
}
#endif
