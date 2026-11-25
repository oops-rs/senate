import AppKit
import SwiftUI
import UniformTypeIdentifiers

/// The Scenes place: what a scene is, the scenes installed, and adding,
/// updating and removing them. Adding shows what a scene asks for before
/// anything of it runs; a scene sees nothing of a Senate until the person
/// allows it on its own page.
struct ScenesView: View {
    /// Posted with a folder or archive URL to inspect it as if chosen, or
    /// with nothing to close the sheet (snapshots).
    static let inspectNote = Notification.Name("SenateScenesInspect")

    @Bindable var store: CoreStore
    @State private var adding: SceneLibrary.Candidate?
    /// An update in waiting: the installed scene and the new bundle.
    @State private var updating: (scene: InstalledScene, candidate: SceneLibrary.Candidate)?
    @State private var removing: InstalledScene?
    @State private var problem: String?
    @State private var busy = false

    private var library: SceneLibrary { store.scenes }
    private var senateKey: String { store.state.senates.first?.key ?? "local" }

    var body: some View {
        ReadingColumn(spacing: Theme.Space.s) {
            PageHeading(text: L("scenes.title"))
            Text(verbatim: L("scenes.about")).font(.callout).foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            Text(verbatim: L("scenes.how")).font(.callout).foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            HStack {
                Button(L("scenes.add")) { choose { inspect($0, then: { adding = $0 }) } }
                    .buttonStyle(.borderedProminent)
                    .disabled(busy)
                Spacer()
            }
            if let problem {
                Text(verbatim: problem).font(.callout).foregroundStyle(.red).fixedSize(horizontal: false, vertical: true)
            }
            Color.clear.frame(height: Theme.Space.s)
            SectionHeader(L("scenes.installed"))
            if library.scenes.isEmpty {
                Text(verbatim: L("scenes.none")).font(Theme.Fonts.row).foregroundStyle(Theme.onSurfaceVariant)
            }
            ForEach(library.scenes) { scene in row(scene) }
            ForEach(library.damaged, id: \.self) { id in
                HStack {
                    Text(verbatim: L("scenes.damaged", String(id.prefix(12)))).foregroundStyle(.secondary)
                    Spacer()
                    Button(L("scenes.remove")) { Task { await remove(id: id) } }
                }
            }
        }
        .sheet(item: $adding) { candidate in addSheet(candidate) }
        .sheet(isPresented: Binding(get: { updating != nil }, set: { if !$0 { cancelUpdate() } })) {
            if let updating { updateSheet(updating.scene, updating.candidate) }
        }
        .confirmationDialog(L("scenes.remove_title", removing?.manifest.name ?? ""),
                            isPresented: Binding(get: { removing != nil }, set: { if !$0 { removing = nil } })) {
            Button(L("scenes.remove"), role: .destructive) {
                if let scene = removing { Task { await remove(id: scene.id) } }
                removing = nil
            }
        } message: {
            Text(verbatim: L("scenes.remove_body"))
        }
        .onReceive(NotificationCenter.default.publisher(for: Self.inspectNote)) { note in
            if let url = note.object as? URL {
                inspect(url, then: { adding = $0 })
            } else if let candidate = adding {
                library.discard(candidate)
                adding = nil
            }
        }
        .onAppear { library.reload() }
    }

    private func row(_ scene: InstalledScene) -> some View {
        let allowed = SceneAccess.load(senateKey, scene: scene.id) != nil
        return VStack(alignment: .leading, spacing: Theme.Space.xs) {
            HStack(alignment: .firstTextBaseline) {
                Button { store.select(.scene(scene.id)) } label: {
                    Text(verbatim: scene.manifest.name).font(.headline)
                }
                .buttonStyle(.plain)
                Text(verbatim: scene.manifest.version).foregroundStyle(.secondary)
                if scene.development { Text(verbatim: L("scenes.development")).font(.caption).foregroundStyle(.orange) }
                Spacer()
                Text(verbatim: allowed ? L("scenes.allowed") : L("scenes.not_allowed")).font(.caption).foregroundStyle(.secondary)
            }
            Text(verbatim: L("scenes.identity", String(scene.id.prefix(12)), String(scene.digest.prefix(12))))
                .font(Theme.Fonts.meta).foregroundStyle(Theme.onSurfaceVariant).textSelection(.enabled)
            SceneAsks(manifest: scene.manifest)
            HStack {
                if allowed {
                    Button(L("scenes.revoke")) {
                        Task {
                            store.dropScenePage(scene.id)
                            await SceneAccess.revoke(scene, senate: senateKey, core: store.core)
                            library.reload()
                        }
                    }
                }
                if !scene.development {
                    Button(L("scenes.update")) { choose { inspect($0, then: { updating = (scene, $0) }) } }
                    Button(L("scenes.remove"), role: .destructive) { removing = scene }
                }
                Spacer()
            }
            .disabled(busy)
            Divider()
        }
        .padding(.vertical, Theme.Space.xs)
    }

    // MARK: Choosing and checking

    private func choose(_ then: @escaping (URL) -> Void) {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = true
        panel.allowsMultipleSelection = false
        panel.resolvesAliases = false
        panel.allowedContentTypes = [.folder, .zip] + (UTType(filenameExtension: "senatescene").map { [$0] } ?? [])
        panel.message = L("scenes.choose")
        panel.prompt = L("scenes.choose_button")
        guard panel.runModal() == .OK, let url = panel.url else { return }
        then(url)
    }

    private func inspect(_ url: URL, then: @escaping (SceneLibrary.Candidate) -> Void) {
        problem = nil
        busy = true
        defer { busy = false }
        do {
            then(try library.inspect(url))
        } catch let error as SceneBundle.Problem {
            problem = L("scenes.refused", error.description)
        } catch {
            problem = L("scenes.refused", error.localizedDescription)
        }
    }

    // MARK: Sheets

    private func addSheet(_ candidate: SceneLibrary.Candidate) -> some View {
        let manifest = candidate.manifest
        let sameName = library.scenes.contains { $0.manifest.name == manifest.name }
        return VStack(alignment: .leading, spacing: Theme.Space.s) {
            Text(verbatim: L("scenes.add_title", manifest.name)).font(.title3.weight(.semibold))
            Text(verbatim: L("scenes.add_meta", manifest.version, ByteCountFormatter.string(fromByteCount: Int64(candidate.bundle.size), countStyle: .file),
                             Int64(candidate.bundle.files.count), String(candidate.bundle.digest.prefix(12))))
                .font(.callout).foregroundStyle(.secondary)
            Text(verbatim: L("scenes.add_body")).font(.callout).fixedSize(horizontal: false, vertical: true)
            SceneAsks(manifest: manifest)
            if sameName {
                Text(verbatim: L("scenes.same_name")).font(.callout).foregroundStyle(.orange).fixedSize(horizontal: false, vertical: true)
            }
            HStack {
                Spacer()
                Button(L("scenes.cancel"), role: .cancel) {
                    library.discard(candidate)
                    adding = nil
                }
                .keyboardShortcut(.cancelAction)
                Button(L("scenes.add_confirm")) {
                    adding = nil
                    do {
                        let scene = try library.install(candidate)
                        store.select(.scene(scene.id))
                    } catch {
                        problem = L("scenes.refused", (error as? SceneBundle.Problem)?.description ?? error.localizedDescription)
                    }
                }
                .buttonStyle(.borderedProminent)
            }
        }
        .padding(Theme.Space.l)
        .frame(width: 480)
    }

    private func updateSheet(_ scene: InstalledScene, _ candidate: SceneLibrary.Candidate) -> some View {
        let change = SceneAsksChange(from: scene.manifest, to: candidate.manifest)
        return VStack(alignment: .leading, spacing: Theme.Space.s) {
            Text(verbatim: L("scenes.update_title", scene.manifest.name)).font(.title3.weight(.semibold))
            Text(verbatim: L("scenes.update_versions", scene.manifest.version, candidate.manifest.version, candidate.manifest.name))
                .font(.callout).foregroundStyle(.secondary)
            if change.isEmpty {
                Text(verbatim: L("scenes.update_same")).font(.callout)
            } else {
                VStack(alignment: .leading, spacing: Theme.Space.xs) {
                    changeLine(L("scenes.update_more"), change.addedCapabilities)
                    changeLine(L("scenes.update_less"), change.removedCapabilities)
                    changeLine(L("scenes.update_reads_more"), change.addedReads)
                    changeLine(L("scenes.update_reads_less"), change.removedReads)
                    changeLine(L("scenes.update_writes_more"), change.addedWrites)
                    changeLine(L("scenes.update_writes_less"), change.removedWrites)
                    if change.topologyChanged {
                        Text(verbatim: candidate.manifest.wantsTopology ? L("scenes.update_links_on") : L("scenes.update_links_off"))
                        Text(verbatim: L("scenes.update_regrant")).foregroundStyle(.secondary)
                    }
                }
                .font(.callout)
            }
            HStack {
                Spacer()
                Button(L("scenes.cancel"), role: .cancel) { cancelUpdate() }
                    .keyboardShortcut(.cancelAction)
                Button(L("scenes.update_confirm")) {
                    updating = nil
                    Task { await update(scene, candidate, regrant: change.topologyChanged) }
                }
                .buttonStyle(.borderedProminent)
            }
        }
        .padding(Theme.Space.l)
        .frame(width: 480)
    }

    @ViewBuilder private func changeLine(_ label: String, _ items: [String]) -> some View {
        if !items.isEmpty {
            HStack(alignment: .firstTextBaseline) {
                Text(verbatim: label).foregroundStyle(.secondary).frame(width: 170, alignment: .leading)
                Text(verbatim: items.joined(separator: ", "))
            }
        }
    }

    private func cancelUpdate() {
        if let updating { library.discard(updating.candidate) }
        updating = nil
    }

    // MARK: Acting

    private func update(_ scene: InstalledScene, _ candidate: SceneLibrary.Candidate, regrant: Bool) async {
        store.dropScenePage(scene.id)
        // A different reach (the Senate's links on or off): the old grant goes; the person allows again.
        if regrant { await SceneAccess.removeAll(scene: scene.id, core: store.core) }
        do {
            try library.update(scene.id, with: candidate)
        } catch {
            problem = L("scenes.refused", (error as? SceneBundle.Problem)?.description ?? error.localizedDescription)
        }
    }

    private func remove(id: String) async {
        busy = true
        defer { busy = false }
        store.dropScenePage(id)
        if case let .scene(shown) = store.place, shown == id { store.select(.scenes) }
        await SceneAccess.removeAll(scene: id, core: store.core)
        do { try library.remove(id) } catch { problem = L("scenes.refused", error.localizedDescription) }
    }
}
