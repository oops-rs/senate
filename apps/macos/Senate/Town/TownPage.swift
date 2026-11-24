import SwiftUI

/// Town: the Senate's live activity as a little town (the scene from the
/// top-level `town/` package in a web view, fed only by the Senate's public
/// Observe API), or the same as a plain list for VoiceOver. Until the person
/// lets Town see this Senate, a card asks; it also offers the demo, which is
/// what a Senate without Observe (an older daemon) can show.
struct TownPage: View {
    /// Posted with `true` to show the list, `false` the map (snapshots).
    static let showListNote = Notification.Name("SenateTownShowList")

    @Bindable var store: CoreStore
    @State private var page: TownWebPage?
    @State private var list = TownList()
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
                    TownWebView(page: page)
                        .ignoresSafeArea(edges: .top)
                        .accessibilityLabel(L("town.map_label"))
                        .accessibilityHint(L("town.map_hint"))
                } else {
                    Color.clear
                }
                if listing {
                    TownActivityList(store: store, list: list)
                        .background(Theme.surface)
                }
                if needsAccess { accessCard }
            }
            controls(list: listing)
                .padding(Theme.Space.m)
        }
        .onAppear {
            let page = self.page ?? TownWebPage(store: store)
            self.page = page
            TownWebPage.shown = page
            page.onAccessNeeded = { needsAccess = $0 }
            page.onSnapshot = { list.snapshot($0) }
            page.onFrame = { list.frame($0) }
            needsAccess = page.accessNeeded
            page.setOptions(reduceMotion: reduceMotion, dark: scheme == .dark, visible: true)
        }
        .onDisappear { page?.setOptions(visible: false) }
        .onChange(of: scheme) { page?.setOptions(dark: scheme == .dark) }
        .onChange(of: reduceMotion) { page?.setOptions(reduceMotion: reduceMotion) }
        .onReceive(NotificationCenter.default.publisher(for: Self.showListNote)) { note in
            showList = note.object as? Bool
        }
    }

    /// One click lets Town see this Senate (metadata only); or watch the demo.
    private var accessCard: some View {
        VStack(spacing: Theme.Space.s) {
            Text(L("town.access_title")).font(.headline)
            Text(L("town.access_body")).font(.callout).foregroundStyle(.secondary).multilineTextAlignment(.center)
            HStack(spacing: Theme.Space.s) {
                Button(L("town.access_demo")) { page?.watchDemo() }
                Button(L("town.access_button")) {
                    granting = true
                    failure = nil
                    Task {
                        do { try await page?.grantAccess() } catch { failure = L("town.access_unavailable") }
                        granting = false
                    }
                }
                .buttonStyle(.borderedProminent)
                .disabled(granting)
            }
            if let failure { Text(failure).font(.caption).foregroundStyle(.secondary).multilineTextAlignment(.center) }
        }
        .padding(Theme.Space.l)
        .frame(maxWidth: 380)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
    }

    private func controls(list: Bool) -> some View {
        HStack(spacing: Theme.Space.s) {
            if !list {
                Button { Task { _ = await page?.evaluate("window.town.zoom(0.8); return true") } } label: { Image(systemName: "minus.magnifyingglass") }
                    .help(L("town.zoom_out")).accessibilityLabel(L("town.zoom_out"))
                Button { Task { _ = await page?.evaluate("window.town.zoom(1.25); return true") } } label: { Image(systemName: "plus.magnifyingglass") }
                    .help(L("town.zoom_in")).accessibilityLabel(L("town.zoom_in"))
                Button { Task { _ = await page?.evaluate("window.town.fit(); return true") } } label: { Image(systemName: "arrow.up.left.and.arrow.down.right") }
                    .help(L("town.fit")).accessibilityLabel(L("town.fit"))
            }
            Picker(L("town.view"), selection: Binding(get: { list }, set: { showList = $0 })) {
                Text(verbatim: L("town.map")).tag(false)
                Text(verbatim: L("town.list")).tag(true)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .fixedSize()
        }
        .buttonStyle(.borderless)
        .padding(.horizontal, Theme.Space.s)
        .padding(.vertical, Theme.Space.xs)
        .background(Theme.bright, in: Capsule())
        .overlay(Capsule().strokeBorder(Theme.outline))
    }
}

/// The town as a list: each house and who is in it doing what, the desk,
/// and what has happened since the page opened. Every row opens the link
/// Observe gave it, through the app's one router.
private struct TownActivityList: View {
    let store: CoreStore
    let list: TownList

    var body: some View {
        ReadingColumn(spacing: Theme.Space.xs) {
            PageHeading(text: L("town.title"))
            Color.clear.frame(height: Theme.Space.s)
            SectionHeader(L("town.activity"))
            if list.log.isEmpty {
                Text(verbatim: L("town.no_activity"))
                    .font(Theme.Fonts.row)
                    .foregroundStyle(Theme.onSurfaceVariant)
                    .padding(.leading, Theme.Space.xs)
            }
            ForEach(list.log) { entry in
                row(entry.text, detail: entry.at.formatted(date: .omitted, time: .shortened), link: entry.link)
            }
            if list.docket > 0 {
                Color.clear.frame(height: Theme.Space.m)
                SectionHeader(L("town.hall"))
                row(L("town.docket", Int64(list.docket)), detail: nil, link: list.deskLink)
            }
            ForEach(list.committees) { committee in
                Color.clear.frame(height: Theme.Space.s)
                Button { open(committee.link) } label: { SectionHeader(committee.name) }
                    .buttonStyle(.plain)
                    .accessibilityAddTraits(.isHeader)
                ForEach(committee.waiting, id: \.self) { link in
                    row(L("town.list.waiting"), detail: L("town.mood.waiting"), link: link)
                }
                ForEach(committee.senators) { senator in
                    row(senator.chair ? L("town.chair", senator.name) : senator.name,
                        detail: [senator.runtime, TownList.label(senator.state)].filter { !$0.isEmpty }.joined(separator: " · "),
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
