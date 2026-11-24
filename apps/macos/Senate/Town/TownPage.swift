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
    /// The town as last read.
    private(set) var input: TownState?
    private(set) var log: [Entry] = []
    /// `nil`: the list while VoiceOver runs, else the map.
    var showList: Bool?
    @ObservationIgnored private var counter = 0

    init() {
        scene.onVisit = { [weak self] action in self?.record(action) }
    }

    /// A new reading: the scene shows it and acts out what changed.
    func update(_ new: TownState) {
        let events = TownDiff.events(from: input, to: new)
        let names = new.senators
        for event in events {
            switch event {
            case let .mood(id, mood):
                add(L("town.now", names[id]?.name ?? id, mood.label), thread: nil)
            case let .arrived(id):
                add(L("town.arrived", names[id]?.name ?? id), thread: nil)
            case let .left(id):
                add(L("town.left", input?.senators[id]?.name ?? id), thread: nil)
            case .message:
                break // logged when the scene acts it out, or below when it does not
            case let .answered(thread, _, _):
                add(L("town.settled", thread.title), thread: thread)
            }
        }
        input = new
        scene.apply(new, events: events)
    }

    /// What the committees' activity says happened (`TownActivityFeed`).
    func activity(_ events: [TownEvent]) {
        guard let input else { return }
        for case let .answered(thread, _, _) in events {
            add(L("town.settled", thread.title), thread: thread)
        }
        scene.apply(input, events: events)
    }

    /// The live feed, while the page is shown (not for the scripted town).
    @ObservationIgnored private var feed: TownActivityFeed?

    func startFeed(_ store: CoreStore) {
        guard script == nil else { return }
        if feed == nil { feed = TownActivityFeed(store: store) { [weak self] in self?.activity($0) } }
        feed?.start()
    }

    func stopFeed() {
        feed?.stop()
    }

    /// The scripted busy events (snapshots and trying the scene).
    func play(_ events: [TownEvent], state: TownState) {
        var state = state
        state.person = input?.person ?? state.person
        input = state
        scene.apply(state, events: events)
    }

    /// What the pointer is over, in words.
    func tooltip(_ hit: TownScene.Hit) -> String? {
        guard let state = input else { return nil }
        switch hit {
        case let .senator(id, _):
            guard let s = state.senators[id] else { return nil }
            var lines = ["\(s.chair ? L("town.chair", s.name) : s.name) · \(s.runtime.rawValue) · \(s.mood.label)"]
            if let doing = s.doing, !doing.isEmpty { lines.append(doing) }
            return lines.joined(separator: "\n")
        case let .thread(ref):
            let waiting = state.committees.flatMap(\.waiting).first { $0.key == ref.key }
            guard let waiting else { return ref.title }
            var text = L("town.tip.waiting", state.person ?? L("town.you"), waiting.title)
            if let asker = waiting.asker { text += "\n" + L("town.tip.asked_by", asker) }
            return text
        case let .committee(name):
            let count = state.committees.first { $0.name == name }?.senators.count ?? 0
            return "\(name)\n\(L("town.tip.senators", Int64(count)))"
        case .hall:
            return "\(L("town.hall"))\n\(L("town.tip.hall"))"
        }
    }

    private func record(_ action: TownAction) {
        guard case let .visit(from, to, thread, line, _) = action else { return }
        let names = input?.senators ?? [:]
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
    var body: some View {
        // The scripted town waits on whoever is signed in, like the real one.
        let input = model.script.map { scripted in
            var town = scripted
            town.person = store.state.person ?? scripted.person
            return town
        } ?? TownState(store: store)
        let list = model.showList ?? voiceOver
        ZStack(alignment: .topTrailing) {
            if list {
                TownActivityList(store: store, model: model, state: model.input ?? input, open: open)
            } else {
                map
            }
            controls(list: list)
                .padding(Theme.Space.m)
        }
        .onChange(of: input, initial: true) { model.update(input) }
        .onChange(of: scheme, initial: true) { model.scene.setPalette(scheme == .dark ? .dark : .light) }
        .onChange(of: reduceMotion, initial: true) { model.scene.reduceMotion = reduceMotion }
        .onAppear {
            model.scene.visible = true
            model.startFeed(store)
        }
        .onDisappear {
            model.scene.visible = false
            model.stopFeed()
        }
        .onReceive(NotificationCenter.default.publisher(for: TownModel.playScript)) { note in
            if note.object as? String == "large" {
                model.play(TownScript.largeEvents, state: TownScript.large)
            } else {
                model.play(TownScript.busy, state: TownScript.busyState)
            }
        }
        .onReceive(NotificationCenter.default.publisher(for: TownModel.showListNote)) { note in
            model.showList = note.object as? Bool
        }
    }

    private var map: some View {
        TownMap(model: model, open: open)
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
            } else if let home = model.input?.home(of: id) {
                store.place = .committee(home)
            }
        case let .thread(ref): store.openThread(committee: ref.committee, root: ref.root)
        case let .committee(name): store.place = .committee(name)
        case .hall: store.place = .secretary
        }
    }
}

/// The map: the scene, its gestures (drag, pinch, scroll, ⌘-scroll, a
/// click, a double click on a house) and the tooltip under the pointer.
/// Its own view, so a moving pointer redraws only the tooltip.
private struct TownMap: View {
    let model: TownModel
    let open: (TownScene.Hit) -> Void
    @State private var dragged: CGSize = .zero
    @State private var magnified: CGFloat = 1
    @State private var tip: (text: String, at: CGPoint)?

    var body: some View {
        let scene = model.scene
        SpriteView(scene: scene, preferredFramesPerSecond: 60, options: [.ignoresSiblingOrder])
            .accessibilityHidden(true)
            .ignoresSafeArea(edges: .top)
            .overlay {
                // The gestures on a clear layer over the view, the same on iOS.
                Color.clear
                    .contentShape(Rectangle())
                    .onTapGesture(count: 2, coordinateSpace: .local) { point in scene.focus(at: point) }
                    .onTapGesture(coordinateSpace: .local) { point in
                        if let hit = scene.hit(at: point) { open(hit) }
                    }
                    .onContinuousHover(coordinateSpace: .local) { phase in
                        switch phase {
                        case let .active(point):
                            tip = scene.hit(at: point).flatMap { model.tooltip($0) }.map { ($0, point) }
                        case .ended:
                            tip = nil
                        }
                    }
                    .gesture(
                        DragGesture(minimumDistance: 2)
                            .onChanged { value in
                                tip = nil
                                scene.setInteracting(true)
                                scene.pan(by: CGSize(
                                    width: value.translation.width - dragged.width,
                                    height: value.translation.height - dragged.height
                                ))
                                dragged = value.translation
                            }
                            .onEnded { _ in
                                dragged = .zero
                                scene.setInteracting(false)
                            }
                    )
                    .simultaneousGesture(
                        MagnifyGesture()
                            .onChanged { value in
                                scene.setInteracting(true)
                                scene.zoom(by: value.magnification / magnified, at: value.startLocation)
                                magnified = value.magnification
                            }
                            .onEnded { _ in
                                magnified = 1
                                scene.setInteracting(false)
                            }
                    )
                    .accessibilityElement()
                    .accessibilityLabel(L("town.map_label"))
                    .accessibilityHint(L("town.map_hint"))
            }
            #if os(macOS)
            .background {
                TownScrollCatcher(
                    pan: { delta in
                        tip = nil
                        scene.pan(by: delta)
                    },
                    zoom: { factor, point in scene.zoom(by: factor, at: point) },
                    interacting: { scene.setInteracting($0) }
                )
            }
            #endif
            .overlay(alignment: .topLeading) {
                if let tip {
                    TownTooltip(text: tip.text)
                        .offset(x: tip.at.x + 14, y: tip.at.y + 18)
                        .allowsHitTesting(false)
                }
            }
    }
}

/// A small card under the pointer.
private struct TownTooltip: View {
    let text: String

    var body: some View {
        Text(verbatim: text)
            .font(Theme.Fonts.meta)
            .foregroundStyle(Theme.onSurface)
            .lineLimit(4)
            .frame(maxWidth: 280, alignment: .leading)
            .fixedSize(horizontal: false, vertical: true)
            .padding(.horizontal, Theme.Space.s)
            .padding(.vertical, 6)
            .background(Theme.bright, in: RoundedRectangle(cornerRadius: 8))
            .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Theme.outline))
            .shadow(color: .black.opacity(0.08), radius: 4, y: 2)
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
