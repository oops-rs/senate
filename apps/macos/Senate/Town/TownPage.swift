import SpriteKit
import SwiftUI

/// The page's model: the scene, the last reading of the store, and what
/// happened since the page opened (the accessible list's activity).
@Observable
final class TownModel {
    /// Posted (DEBUG snapshots) to act out the scripted busy events.
    static let playScript = Notification.Name("SenateTownPlayScript")
    /// Posted with `true` to show the list, `false` the map.
    static let showListNote = Notification.Name("SenateTownShowList")

    struct Entry: Identifiable, Hashable {
        let id: Int
        let at: Date
        let text: String
        let thread: TownThreadRef?
    }

    @ObservationIgnored let scene = TownScene(size: CGSize(width: 800, height: 600))
    /// The made-up town (`SENATE_APP_TOWN_DEMO`), instead of the store's.
    @ObservationIgnored let script = TownScript.requested
    private(set) var input: TownInput?
    private(set) var log: [Entry] = []
    /// `nil`: the list while VoiceOver runs, else the map.
    var showList: Bool?
    @ObservationIgnored private var counter = 0

    init() {
        scene.onVisit = { [weak self] action in self?.record(action) }
    }

    /// A new reading: the scene shows it and acts out what changed.
    func update(_ new: TownInput) {
        let events = TownDiff.events(from: input, to: new)
        let names = new.state.senators
        for event in events {
            switch event {
            case let .mood(id, mood):
                add(L("town.now", names[id]?.name ?? id, mood.label), thread: nil)
            case let .arrived(id):
                add(L("town.arrived", names[id]?.name ?? id), thread: nil)
            case let .left(id):
                add(L("town.left", input?.state.senators[id]?.name ?? id), thread: nil)
            case .message:
                break // logged when the scene acts it out, or below when it does not
            }
        }
        input = new
        scene.apply(new.state, events: events)
    }

    /// The scripted busy events (snapshots and trying the scene).
    func play(_ events: [TownEvent], state: TownState) {
        input = TownInput(state: state, threads: input?.threads ?? [])
        scene.apply(state, events: events)
    }

    private func record(_ action: TownAction) {
        guard case let .visit(from, to, thread, line, _) = action else { return }
        let names = input?.state.senators ?? [:]
        let target = to.flatMap { names[$0]?.name } ?? thread.committee
        add(L("town.said", names[from]?.name ?? from, target, line), thread: thread)
    }

    private func add(_ text: String, thread: TownThreadRef?) {
        counter += 1
        log.insert(Entry(id: counter, at: .now, text: text, thread: thread), at: 0)
        if log.count > 60 { log.removeLast(log.count - 60) }
    }
}

extension TownMood {
    var label: String {
        switch self {
        case .working: L("town.mood.working")
        case .idle: L("town.mood.idle")
        case .waiting: L("town.mood.waiting")
        case .paused: L("town.mood.paused")
        case .stopped: L("town.mood.stopped")
        }
    }
}

/// Town: the Senate as a little neighbourhood. Each committee is a house
/// and its senators little people in it; a message is one walking over to
/// another with a speech bubble; what waits on you is a visitor at the door
/// and a raised flag. A click opens what was clicked. The same activity is
/// also a plain list, for VoiceOver.
struct TownPage: View {
    @Bindable var store: CoreStore
    @State private var model = TownModel()
    @Environment(\.colorScheme) private var scheme
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.accessibilityVoiceOverEnabled) private var voiceOver
    @State private var dragged: CGSize = .zero
    @State private var magnified: CGFloat = 1

    var body: some View {
        let input = model.script ?? TownInput(store: store)
        let list = model.showList ?? voiceOver
        ZStack(alignment: .topTrailing) {
            if list {
                TownActivityList(store: store, model: model, state: model.input?.state ?? input.state, open: open)
            } else {
                map
            }
            controls(list: list)
                .padding(Theme.Space.m)
        }
        .onChange(of: input, initial: true) { model.update(input) }
        .onChange(of: scheme, initial: true) { model.scene.setPalette(scheme == .dark ? .dark : .light) }
        .onChange(of: reduceMotion, initial: true) { model.scene.reduceMotion = reduceMotion }
        .onAppear { model.scene.visible = true }
        .onDisappear { model.scene.visible = false }
        .onReceive(NotificationCenter.default.publisher(for: TownModel.playScript)) { note in
            if note.object as? String == "large" {
                model.play(TownScript.largeEvents, state: TownScript.large.state)
            } else {
                model.play(TownScript.busy, state: TownScript.busyState)
            }
        }
        .onReceive(NotificationCenter.default.publisher(for: TownModel.showListNote)) { note in
            model.showList = note.object as? Bool
        }
    }

    private var map: some View {
        SpriteView(scene: model.scene, preferredFramesPerSecond: 60, options: [.ignoresSiblingOrder])
            .accessibilityHidden(true)
            .ignoresSafeArea(edges: .top)
            .overlay {
                // The gestures on a clear layer over the view, the same on iOS.
                Color.clear
                    .contentShape(Rectangle())
                    .onTapGesture(coordinateSpace: .local) { point in
                        if let hit = model.scene.hit(at: point) { open(hit) }
                    }
                    .gesture(
                        DragGesture(minimumDistance: 2)
                            .onChanged { value in
                                model.scene.setInteracting(true)
                                model.scene.pan(by: CGSize(
                                    width: value.translation.width - dragged.width,
                                    height: value.translation.height - dragged.height
                                ))
                                dragged = value.translation
                            }
                            .onEnded { _ in
                                dragged = .zero
                                model.scene.setInteracting(false)
                            }
                    )
                    .simultaneousGesture(
                        MagnifyGesture()
                            .onChanged { value in
                                model.scene.setInteracting(true)
                                model.scene.zoom(by: value.magnification / magnified, at: value.startLocation)
                                magnified = value.magnification
                            }
                            .onEnded { _ in
                                magnified = 1
                                model.scene.setInteracting(false)
                            }
                    )
                    .accessibilityElement()
                    .accessibilityLabel(L("town.map_label"))
                    .accessibilityHint(L("town.map_hint"))
            }
    }

    private func controls(list: Bool) -> some View {
        HStack(spacing: Theme.Space.s) {
            if !list {
                Button { model.scene.zoom(by: 1 / 1.25) } label: { Image(systemName: "minus.magnifyingglass") }
                    .help(L("town.zoom_out")).accessibilityLabel(L("town.zoom_out"))
                Button { model.scene.zoom(by: 1.25) } label: { Image(systemName: "plus.magnifyingglass") }
                    .help(L("town.zoom_in")).accessibilityLabel(L("town.zoom_in"))
                Button { model.scene.fit() } label: { Image(systemName: "arrow.up.left.and.arrow.down.right") }
                    .help(L("town.fit")).accessibilityLabel(L("town.fit"))
            }
            Picker(L("town.view"), selection: Binding(get: { list }, set: { model.showList = $0 })) {
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

    /// Open what was clicked: a senator's session, a thread, a committee,
    /// the Secretary at the hall.
    private func open(_ hit: TownScene.Hit) {
        guard model.script == nil else { return }
        switch hit {
        case let .senator(id, sessionId):
            if let sessionId {
                store.place = store.secretarySession?.sessionId == sessionId ? .secretary : .session(sessionId)
            } else if let home = model.input?.state.home(of: id) {
                store.place = .committee(home)
            }
        case let .thread(ref): store.openThread(committee: ref.committee, root: ref.root)
        case let .committee(name): store.place = .committee(name)
        case .hall: store.place = .secretary
        }
    }
}

/// The town as a list: each house and who is in it doing what, the hall,
/// and what has happened since the page opened. Every row opens the same
/// thing a click on the map does.
private struct TownActivityList: View {
    let store: CoreStore
    let model: TownModel
    let state: TownState
    let open: (TownScene.Hit) -> Void

    var body: some View {
        ReadingColumn(spacing: Theme.Space.xs) {
            PageHeading(text: L("town.title"))
            Color.clear.frame(height: Theme.Space.s)
            SectionHeader(L("town.activity"))
            if model.log.isEmpty {
                Text(verbatim: L("town.no_activity"))
                    .font(Theme.Fonts.row)
                    .foregroundStyle(Theme.onSurfaceVariant)
                    .padding(.leading, Theme.Space.xs)
            }
            ForEach(model.log) { entry in
                row(entry.text, detail: entry.at.formatted(date: .omitted, time: .shortened)) {
                    if let thread = entry.thread { open(.thread(thread)) }
                }
            }
            Color.clear.frame(height: Theme.Space.m)
            SectionHeader(L("town.hall"))
            if let secretary = state.hall.secretary {
                row(L("town.now", secretary.name, secretary.mood.label), detail: L("town.secretary")) { open(.hall) }
            }
            if state.hall.docket > 0 {
                row(L("town.docket", Int64(state.hall.docket)), detail: nil) { store.place = .section(.approvals) }
            }
            ForEach(state.committees) { committee in
                Color.clear.frame(height: Theme.Space.s)
                Button { open(.committee(committee.name)) } label: { SectionHeader(committee.name) }
                    .buttonStyle(.plain)
                    .accessibilityAddTraits(.isHeader)
                ForEach(committee.waiting, id: \.self) { thread in
                    row(thread.title, detail: L("town.mood.waiting")) { open(.thread(thread)) }
                }
                ForEach(committee.senators) { senator in
                    row(senator.chair ? L("town.chair", senator.name) : senator.name,
                        detail: "\(senator.runtime.rawValue) · \(senator.mood.label)") {
                        open(.senator(id: senator.id, sessionId: senator.sessionId))
                    }
                }
            }
        }
    }

    private func row(_ text: String, detail: String?, action: @escaping () -> Void) -> some View {
        Button(action: action) {
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
    }
}
