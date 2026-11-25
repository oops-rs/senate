import SwiftUI

/// An installed scene: the Senate's live activity as it draws it, in its own
/// isolated web view and fed only by its own Observe grant; or the same
/// activity as a plain list (VoiceOver's default). Until the person lets
/// this scene see this Senate, a card says what it asks for and asks.
struct ScenePage: View {
    /// Posted with `true` to show the list, `false` the scene (snapshots).
    static let showListNote = Notification.Name("SenateSceneShowList")

    @Bindable var store: CoreStore
    let scene: InstalledScene
    @State private var page: SceneWebPage?
    @State private var list: ObserveList?
    @State private var showList: Bool?
    @State private var needsAccess = false
    @State private var granting = false
    @State private var failure: String?
    @Environment(\.colorScheme) private var scheme
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.accessibilityVoiceOverEnabled) private var voiceOver

    var body: some View {
        let listing = showList ?? voiceOver
        ZStack(alignment: .topTrailing) {
            ZStack {
                if let page {
                    SceneWebView(page: page)
                        .ignoresSafeArea(edges: .top)
                        .accessibilityLabel(L("scene.view_label", scene.manifest.name))
                        .accessibilityHint(L("scene.view_hint"))
                } else {
                    Color.clear
                }
                if listing, let list {
                    ObserveActivityList(store: store, title: scene.manifest.name, list: list)
                        .background(Theme.surface)
                }
                if needsAccess { accessCard }
            }
            controls(list: listing)
                .padding(Theme.Space.m)
        }
        .onAppear { attach() }
        .onChange(of: scene) { attach() }
        .onDisappear { page?.setOptions(visible: false) }
        .onChange(of: scheme) { page?.setOptions(dark: scheme == .dark) }
        .onChange(of: reduceMotion) { page?.setOptions(reduceMotion: reduceMotion) }
        .onReceive(NotificationCenter.default.publisher(for: Self.showListNote)) { note in
            showList = note.object as? Bool
        }
    }

    private func attach() {
        let made = store.scenePage(scene)
        page?.setOptions(visible: false)
        page = made.page
        list = made.list
        SceneWebPage.shown = made.page
        let list = made.list
        made.page.onAccessNeeded = { needsAccess = $0 }
        made.page.onSnapshot = { list.snapshot($0) }
        made.page.onFrame = { list.frame($0) }
        needsAccess = made.page.accessNeeded
        failure = nil
        made.page.setOptions(reduceMotion: reduceMotion, dark: scheme == .dark, visible: true)
    }

    /// One click lets this scene see this Senate, as far as it asks.
    private var accessCard: some View {
        VStack(alignment: .leading, spacing: Theme.Space.s) {
            Text(verbatim: L("scene.access_title", scene.manifest.name)).font(.headline)
            Text(verbatim: L("scene.access_body")).font(.callout).foregroundStyle(.secondary)
            SceneAsks(manifest: scene.manifest)
            HStack {
                Spacer()
                Button(L("scene.access_button")) {
                    granting = true
                    failure = nil
                    Task {
                        do { try await page?.grantAccess() } catch { failure = L("scene.access_unavailable") }
                        granting = false
                    }
                }
                .buttonStyle(.borderedProminent)
                .disabled(granting)
            }
            if let failure { Text(verbatim: failure).font(.caption).foregroundStyle(.secondary) }
        }
        .padding(Theme.Space.l)
        .frame(maxWidth: 420)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
    }

    private func controls(list: Bool) -> some View {
        HStack(spacing: Theme.Space.s) {
            Picker(L("scene.view"), selection: Binding(get: { list }, set: { showList = $0 })) {
                Text(verbatim: L("scene.view_scene")).tag(false)
                Text(verbatim: L("scene.view_list")).tag(true)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .fixedSize()
            Button { store.select(.scenes) } label: { Image(systemName: "square.stack.3d.up") }
                .help(L("scenes.manage")).accessibilityLabel(L("scenes.manage"))
        }
        .buttonStyle(.borderless)
        .padding(.horizontal, Theme.Space.s)
        .padding(.vertical, Theme.Space.xs)
        .background(Theme.bright, in: Capsule())
        .overlay(Capsule().strokeBorder(Theme.outline))
    }
}

/// What a scene asks for, in words: what it observes, whether it sees the
/// Senate's direct links, which cards it reads and writes.
struct SceneAsks: View {
    let manifest: SceneManifest

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.xs) {
            line(L("scene.asks.observe"), L("scene.asks.metadata"))
            line(L("scene.asks.links"), manifest.wantsTopology ? L("scene.asks.yes") : L("scene.asks.no"))
            line(L("scene.asks.required"), manifest.required.isEmpty ? L("scene.asks.none") : manifest.required.joined(separator: ", "))
            line(L("scene.asks.optional"), manifest.optional.isEmpty ? L("scene.asks.none") : manifest.optional.joined(separator: ", "))
            line(L("scene.asks.reads"), manifest.cardReads.isEmpty ? L("scene.asks.none") : manifest.cardReads.joined(separator: ", "))
            line(L("scene.asks.writes"), manifest.cardWrites.isEmpty ? L("scene.asks.none") : manifest.cardWrites.joined(separator: ", "))
        }
        .font(.callout)
    }

    private func line(_ label: String, _ value: String) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: Theme.Space.s) {
            Text(verbatim: label).foregroundStyle(.secondary).frame(width: 150, alignment: .leading)
            Text(verbatim: value).textSelection(.enabled).fixedSize(horizontal: false, vertical: true)
        }
    }
}

/// The Senate as a list: each committee and who is in it doing what, the
/// desk, and what has happened since the page opened. Every row opens the
/// link Observe gave it, through the app's one router.
struct ObserveActivityList: View {
    let store: CoreStore
    let title: String
    let list: ObserveList

    var body: some View {
        ReadingColumn(spacing: Theme.Space.xs) {
            PageHeading(text: title)
            Color.clear.frame(height: Theme.Space.s)
            SectionHeader(L("scene.list.activity"))
            if list.log.isEmpty {
                Text(verbatim: L("scene.list.no_activity"))
                    .font(Theme.Fonts.row)
                    .foregroundStyle(Theme.onSurfaceVariant)
                    .padding(.leading, Theme.Space.xs)
            }
            ForEach(list.log) { entry in
                row(entry.text, detail: entry.at.formatted(date: .omitted, time: .shortened), link: entry.link)
            }
            if list.docket > 0 {
                Color.clear.frame(height: Theme.Space.m)
                SectionHeader(L("scene.list.desk"))
                row(L("scene.list.docket", Int64(list.docket)), detail: nil, link: list.deskLink)
            }
            ForEach(list.committees) { committee in
                Color.clear.frame(height: Theme.Space.s)
                Button { open(committee.link) } label: { SectionHeader(committee.name) }
                    .buttonStyle(.plain)
                    .accessibilityAddTraits(.isHeader)
                ForEach(committee.waiting, id: \.self) { link in
                    row(L("scene.list.waiting"), detail: L("scene.state.waiting"), link: link)
                }
                ForEach(committee.senators) { senator in
                    row(senator.chair ? L("scene.list.chair", senator.name) : senator.name,
                        detail: [senator.runtime, ObserveList.label(senator.state)].filter { !$0.isEmpty }.joined(separator: " · "),
                        link: senator.link)
                }
            }
        }
    }

    private func open(_ link: String?) {
        guard let link, !link.isEmpty else { return }
        store.links.open(link, provenance: .gesture)
    }

    private func row(_ text: String, detail: String?, link: String?) -> some View {
        Button { open(link) } label: {
            HStack(alignment: .firstTextBaseline, spacing: Theme.Space.s) {
                Text(verbatim: text)
                    .font(Theme.Fonts.text)
                    .foregroundStyle(Theme.onSurface)
                    .lineLimit(2)
                Spacer(minLength: Theme.Space.s)
                if let detail {
                    Text(verbatim: detail)
                        .font(Theme.Fonts.meta)
                        .foregroundStyle(Theme.onSurfaceVariant)
                }
            }
            .padding(.vertical, 6)
            .padding(.horizontal, Theme.Space.s)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(link == nil)
    }
}
