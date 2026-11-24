import SwiftUI
import SenateCore

/// The window: a sidebar with Inbox, Approvals and each committee's
/// sessions on this machine, and beside it what is chosen. Nothing sits
/// above the content but what it needs at its top right.
struct MainWindow: View {
    @Bindable var store: CoreStore

    var body: some View {
        NavigationSplitView {
            Sidebar(store: store)
                .navigationSplitViewColumnWidth(min: 220, ideal: Theme.sidebarWidth, max: 340)
        } detail: {
            // One stack for every page: a link inside a page pushes, the
            // sidebar starts it again, and the system draws Back.
            NavigationStack(path: $store.path) {
                framed(store.root)
                    .navigationDestination(for: CoreStore.Place.self) { framed($0) }
            }
            // Wide enough that nothing in it wraps into a tall column.
            .frame(minWidth: 440, maxWidth: .infinity, maxHeight: .infinity)
            .safeAreaInset(edge: .top, spacing: 0) {
                if store.state.online, let error = store.shownError {
                    ErrorBanner(message: error) { store.actionError = nil }
                }
            }
            .toolbarBackgroundVisibility(.hidden, for: .windowToolbar)
        }
        .background { Shortcuts(store: store) }
    }

    /// A page with what every page has: the window's ground, and its name
    /// for Mission Control and VoiceOver (not drawn).
    private func framed(_ place: CoreStore.Place) -> some View {
        page(place)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(Theme.surface.ignoresSafeArea())
            .navigationTitle(title(place))
            .toolbar(removing: .title)
    }

    /// The window's name, for Mission Control and VoiceOver; it is not drawn.
    private func title(_ place: CoreStore.Place) -> String {
        switch place {
        case let .thread(key):
            if let row = store.row(key) { return Mentions.shared.plain(row.title, committee: row.committee) }
        case let .session(id):
            if let s = store.session(id) { return s.displayName }
        case let .committee(name):
            return name.isEmpty ? L("sidebar.no_committee") : name
        case .section(.senates):
            return store.state.person ?? Section.senates.title
        case let .section(section):
            return section.title
        case .secretary:
            return L("secretary.title")
        case .town:
            return L("town.title")
        case let .threads(committee):
            return committee
        case let .past(id):
            return store.history.row(id)?.name ?? L("tab.history")
        case let .desk(item):
            return L("desk.item_heading", item.committee, String(item.seq))
        case .item:
            break
        }
        return Section.inbox.title
    }

    private typealias Section = CoreStore.Section

    /// A page of the window, for the stack's root or a page pushed on it.
    @ViewBuilder private func page(_ place: CoreStore.Place) -> some View {
        if let problem = store.setupError {
            SetupProblemView(store: store, message: problem)
        } else if !store.state.online {
            OfflineView(store: store)
        } else {
            switch place {
            case let .thread(key):
                if let row = store.row(key) {
                    ThreadPage(store: store, row: row)
                } else if let error = store.threadError {
                    EmptyState(text: L("inbox.thread_failed"), detail: error)
                } else {
                    ProgressView().controlSize(.small)
                }
            case let .session(id):
                if let session = store.session(id) {
                    SessionPage(store: store, session: session)
                } else {
                    EmptyState(text: L("session.gone"))
                }
            case let .committee(name): CommitteePage(store: store, name: name)
            case .secretary: SecretaryPage(store: store)
            case .town: TownPage(store: store)
            case let .threads(committee): CommitteeThreadsPage(store: store, committee: committee)
            case .section(.history): HistoryView(store: store)
            case let .past(id): PastSessionPage(store: store, sessionId: id)
            case .item, .section(.inbox): InboxView(store: store)
            case .section(.approvals): ApprovalsView(store: store)
            case let .desk(item): DeskEntryDetail(store: store, item: item).id(item.id)
            case .section(.senates): SenatesView(store: store)
            }
        }
    }
}

/// ⌘1 Inbox, ⌘2 Approvals, ⌘3 your profile; ⌘⌥↑ and ⌘⌥↓ move between sessions; ⌘R reads
/// everything again, or tries the daemon now while it cannot be reached.
/// (⌘, is the store's: SwiftUI keeps that key for the Settings scene's own
/// menu item, which a menu-bar-only app has no menu bar to reach.)
/// Invisible buttons: the app has no menu bar of its own (it lives in the
/// status item).
private struct Shortcuts: View {
    let store: CoreStore

    var body: some View {
        ZStack {
            // ⌘1 Inbox, ⌘2 Approvals, ⌘3 your profile, ⌘R: the View menu's
            // while Senate is in the Dock; these buttons' in the menu bar only.
            if !store.showInDock {
                ForEach(Array(CoreStore.Section.allCases.enumerated()), id: \.element) { index, section in
                    Button(section.title) { store.select(.section(section)) }
                        .keyboardShortcut(KeyEquivalent(Character(String(index + 1))), modifiers: .command)
                }
                Button(L("nav.back")) { store.back() }
                    .keyboardShortcut("[")
                    .disabled(!store.canGoBack)
                Button(L("toolbar.refresh")) {
                    if store.state.online { store.refresh() } else { store.reconnect() }
                }
                .keyboardShortcut("r")
            }
            Button(L("sidebar.previous_session")) { store.moveSession(-1) }
                .keyboardShortcut(.upArrow, modifiers: [.command, .option])
            Button(L("sidebar.next_session")) { store.moveSession(1) }
                .keyboardShortcut(.downArrow, modifiers: [.command, .option])
        }
        .opacity(0)
        .frame(width: 0, height: 0)
        .accessibilityHidden(true)
    }
}

/// The sidebar: the app's name, Inbox and Approvals, then one folding
/// group per committee with this machine's sessions on it (searched here),
/// and who you are at its foot.
private struct Sidebar: View {
    @Bindable var store: CoreStore
    @State private var searching = false
    @FocusState private var searchFocused: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: Theme.Space.s) {
                // The chamber mark, no tile: its seats in the title's colour
                // (rows fading), the Speaker's seat in the icon's amber; as
                // tall as the title's capitals, so the two read as one.
                ZStack {
                    Image("SenateMark")
                        .renderingMode(.template)
                        .resizable()
                        .foregroundStyle(Theme.onSurface)
                    Image("SenateMarkSpeaker")
                        .resizable()
                }
                .aspectRatio(contentMode: .fit)
                .frame(height: 13)
                .accessibilityHidden(true)
                Text(verbatim: "Senate")
                    .font(.system(size: 18, weight: .medium))
                    .foregroundStyle(Theme.onSurface)
            }
            .padding(.horizontal, Theme.Space.l)
            .padding(.top, Theme.Space.xs)
            .padding(.bottom, Theme.Space.m)
            if searching {
                searchField
            }
            list
        }
        .background(Theme.sidebar)
        .safeAreaInset(edge: .bottom, spacing: 0) {
            // On the sidebar's tone, so a long list scrolls under it unseen.
            AccountRow(store: store)
                .background(Theme.sidebar)
        }
        .toolbar {
            ToolbarItem {
                Button {
                    if searching { closeSearch() } else { openSearch() }
                } label: {
                    Label(L("sidebar.search"), systemImage: "magnifyingglass")
                }
                .help(L("sidebar.search"))
                // The View menu's Find while in the Dock.
                .keyboardShortcut(store.showInDock ? nil : KeyboardShortcut("f"))
            }
        }
        .onChange(of: store.searchRequests) { openSearch() }
    }

    private var list: some View {
        List(selection: place) {
            Section {
                // Pinned: the machine's default secretary, always here.
                SecretaryRow(store: store)
                    .tag(CoreStore.Place.secretary)
                ForEach(store.sections, id: \.self) { section in
                    let count = store.state.online ? store.count(section) : 0
                    FixedRow(title: section.title, count: count > 0 ? count : nil) {
                        FixedSymbol(name: section.symbol)
                    }
                    .tag(CoreStore.Place.section(section))
                }
                FixedRow(title: L("town.title")) { FixedSymbol(name: "house") }
                    .tag(CoreStore.Place.town)
            }
            // Offline, nothing is known: say nothing rather than "no sessions".
            if store.state.online {
                let groups = store.shownGroups
                Section {
                    ForEach(groups) { group in
                        CommitteeGroup(store: store, group: group, searching: !store.query.isEmpty)
                    }
                    if groups.isEmpty {
                        Text(verbatim: store.query.isEmpty ? L("sidebar.no_sessions") : L("sidebar.no_matches"))
                            .font(Theme.Fonts.row)
                            .foregroundStyle(Theme.onSurfaceVariant)
                            .selectionDisabled()
                    }
                } header: {
                    HStack {
                        Text(verbatim: L("sidebar.committees"))
                        Spacer()
                        SidebarFilter(store: store)
                    }
                }
                // Archived committees, when asked for: under their own
                // heading, dimmed, not opened, each with Unarchive.
                if store.showArchived && !store.archived.isEmpty {
                    Section(L("sidebar.archived_heading")) {
                        ForEach(store.archived, id: \.self) { name in
                            ArchivedRow(store: store, committee: name)
                                .selectionDisabled()
                        }
                    }
                }
            }
        }
        .listStyle(.sidebar)
        .scrollContentBackground(.hidden)
        .environment(\.defaultMinListRowHeight, Theme.rowHeight)
        // The archived committees: read when asked for, and on reconnecting.
        .task(id: store.showArchived && store.state.online) { await store.loadArchived() }
        // The committees' threads, followed while online.
        .task(id: "\(store.state.online) \(store.showInactive) \(store.state.groups.compactMap(\.committee))") {
            if store.state.online {
                await store.threads.start(committees: store.state.groups.compactMap(\.committee), archived: store.showInactive)
            } else {
                store.threads.stop()
            }
        }
    }

    private var searchField: some View {
        HStack(spacing: 6) {
            Image(systemName: "magnifyingglass")
                .foregroundStyle(Theme.onSurfaceVariant)
                .accessibilityHidden(true)
            TextField(L("sidebar.search"), text: $store.search)
                .textFieldStyle(.plain)
                .font(Theme.Fonts.row)
                .focused($searchFocused)
                .onExitCommand { closeSearch() }
            Button {
                closeSearch()
            } label: {
                Image(systemName: "xmark.circle.fill")
                    .foregroundStyle(Theme.onSurfaceVariant)
            }
            .buttonStyle(.plain)
            .accessibilityLabel(L("sidebar.close_search"))
        }
        .padding(.horizontal, 10)
        .frame(height: 30)
        .background(Theme.containerHigh, in: Capsule())
        .padding(.horizontal, Theme.Space.m)
        .padding(.bottom, Theme.Space.s)
    }

    private func openSearch() {
        searching = true
        searchFocused = true
    }

    private func closeSearch() {
        store.search = ""
        searching = false
    }

    /// The sidebar shows the stack's root (what was chosen in it; a page
    /// pushed above does not move it, which would start the stack again);
    /// choosing in it starts the window's navigation from there.
    private var place: Binding<CoreStore.Place?> {
        Binding(
            get: { store.root },
            set: { if let place = $0 { store.select(place) } }
        )
    }
}

/// One committee: its name (which opens its page) with what waits on you
/// there, folding its sessions; folded, it shows its most urgent one's
/// status.
private struct CommitteeGroup: View {
    @Bindable var store: CoreStore
    let group: SessionGroup
    /// While searching every group is open.
    let searching: Bool

    var body: some View {
        DisclosureGroup(isExpanded: expanded) {
            ForEach(group.sessions) { session in
                SessionRow(session: session)
                    .tag(CoreStore.Place.session(session.sessionId))
                    .contextMenu { SessionMenu(store: store, session: session) }
            }
            // Its threads: what its senators are saying to one another,
            // newest first; "Show all" past the first few.
            if let committee = group.committee {
                let threads = store.threads.threads(committee)
                ForEach(threads.prefix(ThreadsModel.shown), id: \.root) { thread in
                    SidebarThreadRow(store: store, thread: thread)
                        .tag(CoreStore.Place.thread(ThreadsModel.key(thread)))
                }
                if threads.count > ThreadsModel.shown || store.threads.more.contains(committee) {
                    FixedRow(title: L("sidebar.threads_all")) { FixedSymbol(name: "ellipsis") }
                        .foregroundStyle(Theme.onSurfaceVariant)
                        .tag(CoreStore.Place.threads(committee))
                }
            }
        } label: {
            HStack(spacing: 6) {
                Text(verbatim: group.title)
                    .font(Theme.Fonts.row.weight(.semibold))
                    .foregroundStyle(group.committee == nil ? Theme.onSurfaceVariant : Theme.onSurface)
                    .lineLimit(1)
                Spacer(minLength: Theme.Space.xs)
                if !expanded.wrappedValue, let status = group.status {
                    StatusMark(status: status)
                }
                if group.waiting > 0 {
                    Text(verbatim: "\(group.waiting)")
                        .font(Theme.Fonts.meta.weight(.semibold))
                        .monospacedDigit()
                        .foregroundStyle(Theme.attention)
                        .accessibilityLabel(L("sidebar.waiting_count", Int64(group.waiting)))
                }
            }
            .help(group.committee.map { L("committees.open", $0) } ?? "")
        }
        .tag(CoreStore.Place.committee(group.committee ?? ""))
    }

    private var expanded: Binding<Bool> {
        Binding(
            get: { searching || !store.folded.contains(group.id) },
            set: { open in
                if open { store.folded.remove(group.id) } else { store.folded.insert(group.id) }
            }
        )
    }
}

/// One session: its status, its name, its runtime, and how long since it
/// last did something. Its title and directory are in the tooltip.
private struct SessionRow: View {
    let session: SessionLine

    var body: some View {
        HStack(spacing: 7) {
            SessionMark(runtime: session.runtime, status: session.status)
            Text(verbatim: session.displayName)
                .font(Theme.Fonts.row.weight(session.status == .unread || session.status == .needsYou ? .semibold : .regular))
                .foregroundStyle(session.active ? Theme.onSurface : Theme.onSurfaceVariant)
                .lineLimit(1)
            Spacer(minLength: Theme.Space.xs)
            Ago(date: session.lastActivity)
                .font(Theme.Fonts.meta)
                .foregroundStyle(Theme.onSurfaceVariant)
        }
        .help(tooltip)
        // One element, read as text as the sidebar's other rows are: its
        // name, then its state, runtime and how long since it did anything.
        .accessibilityElement(children: .ignore)
        .accessibilityAddTraits(.isStaticText)
        .accessibilityLabel(session.displayName)
        .accessibilityValue("\(session.statusLabel), \(session.runtime), \(session.lastActivity.formatted(.relative(presentation: .named)))")
    }

    private var tooltip: String {
        var lines = [session.title.map { "\($0) · \(session.runtime)" } ?? "\(session.displayName) · \(session.runtime)"]
        var place = session.dir
        if let branch = session.branch { place += " · \(branch)" }
        lines.append(place)
        lines.append(session.statusLabel)
        return lines.joined(separator: "\n")
    }
}

/// A session's right-click menu in the sidebar: what its page's buttons do.
private struct SessionMenu: View {
    let store: CoreStore
    let session: SessionLine

    var body: some View {
        if let h = session.hosted {
            if h.running {
                Button(L("sessions.attach")) { store.attach(h) }
            }
            SessionActions(store: store, hosted: h, style: .menu)
            Divider()
        }
        CopySenator(label: session.hosted?.label, mention: session.mention)
    }
}

extension CoreStore.Section {
    var title: String {
        switch self {
        case .inbox: L("tab.inbox")
        case .approvals: L("tab.approvals")
        case .senates: L("tab.profile")
        case .history: L("tab.history")
        }
    }

    var symbol: String {
        switch self {
        case .inbox: "tray"
        case .approvals: "checkmark.shield"
        case .senates: "person.crop.circle"
        case .history: "clock.arrow.circlepath"
        }
    }
}

/// The sidebar's filter options, on its Committees header: *Show
/// inactive* (stopped sessions, agents and committees no longer active,
/// here and on the pages that list them). Filled while a filter is on.
private struct SidebarFilter: View {
    @Bindable var store: CoreStore

    var body: some View {
        Menu {
            Toggle(L("roster.show_inactive"), isOn: $store.showInactive)
            Toggle(L("roster.show_archived"), isOn: $store.showArchived)
        } label: {
            let on = store.showInactive || store.showArchived
            Image(systemName: on
                ? "line.3.horizontal.decrease.circle.fill"
                : "line.3.horizontal.decrease.circle")
                .foregroundStyle(on ? Theme.primary : Theme.onSurfaceVariant)
                .frame(width: 22, height: 18)
                .contentShape(Rectangle())
        }
        .menuStyle(.button)
        .buttonStyle(.plain)
        .menuIndicator(.hidden)
        .fixedSize()
        .help(L("sidebar.filter"))
        .accessibilityLabel(L("sidebar.filter"))
        .accessibilityValue([
            store.showInactive ? L("roster.show_inactive") : nil,
            store.showArchived ? L("roster.show_archived") : nil,
        ].compactMap { $0 }.joined(separator: ", "))
    }
}

/// Narrow the inbox to one agent's threads (what was put to you stays).
struct AgentFilter: View {
    @Bindable var store: CoreStore

    var body: some View {
        Menu {
            Picker(L("inbox.agent"), selection: $store.agentFilter) {
                Text(verbatim: L("inbox.all_agents")).tag(String?.none)
                ForEach(store.inboxAgents) { agent in
                    Text(verbatim: agent.pending > 0 ? "\(agent.displayName) (\(agent.pending))" : agent.displayName)
                        .help(agent.path)
                        .tag(Optional(agent.path))
                }
            }
            .pickerStyle(.inline)
        } label: {
            Label(L("inbox.agent"), systemImage: store.agentFilter == nil
                ? "line.3.horizontal.decrease.circle"
                : "line.3.horizontal.decrease.circle.fill")
        }
        .menuIndicator(.hidden)
        .help(L("inbox.agent"))
    }
}

/// Who you are, at the sidebar's foot: one row (your initial with the
/// online dot, your name, the Senate's address), shown selected while your
/// profile is open. Choosing it opens the account popover: who you are,
/// then Profile, Watch all, Settings…, Refresh and About Senate.
private struct AccountRow: View {
    let store: CoreStore
    @State private var hovered = false
    @State private var showing = false

    var body: some View {
        let state = store.state
        let name = store.profile.name(state.person)
        let selected = store.place == .section(.senates)
        Button {
            showing.toggle()
        } label: {
            // Who you are: your picture (with the online dot) and name; the
            // Senate's address is on the profile page.
            HStack(spacing: 10) {
                Avatar(name: name, online: state.online, size: 28, picture: store.profile.picture)
                Text(verbatim: name)
                    .font(Theme.Fonts.label)
                    .foregroundStyle(Theme.onSurface)
                    .lineLimit(1)
                    .truncationMode(.tail)
                Spacer(minLength: 0)
            }
            .padding(.horizontal, Theme.Space.s)
            .padding(.vertical, 6)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(selected || showing ? Theme.selection : hovered ? Theme.containerHigh : .clear,
                        in: RoundedRectangle(cornerRadius: 8))
            .contentShape(RoundedRectangle(cornerRadius: 8))
        }
        .buttonStyle(.plain)
        .onHover { hovered = $0 }
        .help(state.online ? (state.senates.first?.serverUrl ?? "") : (state.offlineReason ?? ""))
        .accessibilityLabel(L("sidebar.account"))
        .accessibilityValue("\(name), \(state.online ? L("a11y.online") : L("a11y.offline"))")
        .accessibilityHint(L("sidebar.account_hint"))
        .accessibilityAddTraits(selected ? .isSelected : [])
        .popover(isPresented: $showing, arrowEdge: .top) {
            AccountPopover(store: store) { showing = false }
        }
        #if DEBUG
        .onReceive(NotificationCenter.default.publisher(for: Snapshot.openAccount)) { _ in showing = true }
        .onReceive(NotificationCenter.default.publisher(for: Snapshot.closeAccount)) { _ in showing = false }
        #endif
        .padding(.horizontal, Theme.Space.s)
        .padding(.vertical, Theme.Space.s)
    }
}

/// The account popover: who you are and where, then what you can do from
/// here, as a short list. Arrow keys move, Return chooses; choosing one, a
/// click outside or Escape closes it.
private struct AccountPopover: View {
    let store: CoreStore
    let close: () -> Void
    @FocusState private var focused: Int?

    private struct Action {
        let title: String
        let symbol: String
        let shortcut: String?
        let enabled: Bool
        let run: () -> Void
    }

    private var actions: [Action] {
        let state = store.state
        return [
            Action(title: L("account.profile"), symbol: "person.crop.circle", shortcut: "⌘3", enabled: state.online) {
                store.select(.section(.senates))
            },
            Action(title: L("sessions.watch"), symbol: "rectangle.split.3x1", shortcut: nil,
                   enabled: state.online && state.sessions.contains { $0.running }) {
                store.openWatch()
            },
            Action(title: L("menu.settings"), symbol: "gearshape", shortcut: "⌘,", enabled: true) {
                store.openSettings?()
            },
            Action(title: L("toolbar.refresh"), symbol: "arrow.clockwise", shortcut: "⌘R", enabled: state.online) {
                store.refresh()
            },
            Action(title: L("menu.about"), symbol: "info.circle", shortcut: nil, enabled: true) {
                About.show()
            },
        ]
    }

    var body: some View {
        let state = store.state
        let name = store.profile.name(state.person)
        let actions = actions
        VStack(alignment: .leading, spacing: Theme.Space.s) {
            HStack(spacing: Theme.Space.m) {
                Avatar(name: name, online: state.online, size: 36, picture: store.profile.picture)
                VStack(alignment: .leading, spacing: 2) {
                    Text(verbatim: name)
                        .font(Theme.Fonts.heading)
                        .foregroundStyle(Theme.onSurface)
                    Text(verbatim: state.online
                        ? L("profile.online_at", Profile.place(state))
                        : (state.offlineReason ?? L("a11y.offline")))
                        .font(Theme.Fonts.meta)
                        .foregroundStyle(Theme.onSurfaceVariant)
                        .lineLimit(2)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .padding(.horizontal, Theme.Space.s)
            .padding(.top, Theme.Space.xs)
            .accessibilityElement(children: .combine)
            Divider()
            VStack(spacing: 2) {
                ForEach(Array(actions.enumerated()), id: \.offset) { i, action in
                    Button {
                        guard action.enabled else { return }
                        close()
                        action.run()
                    } label: {
                        HStack(spacing: Theme.Space.s) {
                            Image(systemName: action.symbol)
                                .frame(width: 18)
                                .foregroundStyle(Theme.onSurfaceVariant)
                                .accessibilityHidden(true)
                            Text(verbatim: action.title)
                                .foregroundStyle(action.enabled ? Theme.onSurface : Theme.onSurfaceVariant)
                            Spacer(minLength: Theme.Space.m)
                            if let shortcut = action.shortcut {
                                Text(verbatim: shortcut)
                                    .foregroundStyle(Theme.onSurfaceVariant)
                                    .accessibilityHidden(true)
                            }
                        }
                        .font(Theme.Fonts.row)
                        .padding(.horizontal, Theme.Space.s)
                        .frame(height: 28)
                        .background(focused == i ? Theme.containerHigh : .clear, in: RoundedRectangle(cornerRadius: 6))
                        .contentShape(RoundedRectangle(cornerRadius: 6))
                    }
                    .buttonStyle(.plain)
                    .disabled(!action.enabled)
                    .focusable()
                    .focused($focused, equals: i)
                    .onHover { if $0 { focused = i } }
                    .accessibilityAddTraits(.isButton)
                }
            }
            .accessibilityElement(children: .contain)
            .accessibilityLabel(L("sidebar.account_menu"))
            .onMoveCommand { direction in
                let enabled = actions.indices.filter { actions[$0].enabled }
                guard !enabled.isEmpty else { return }
                let at = enabled.firstIndex(of: focused ?? -1)
                switch direction {
                case .down: focused = enabled[at.map { min($0 + 1, enabled.count - 1) } ?? 0]
                case .up: focused = enabled[at.map { max($0 - 1, 0) } ?? enabled.count - 1]
                default: break
                }
            }
            .onKeyPress(.return) {
                guard let i = focused, actions[i].enabled else { return .ignored }
                close()
                actions[i].run()
                return .handled
            }
        }
        .padding(Theme.Space.s)
        .frame(width: 260)
        .onAppear { focused = actions.firstIndex { $0.enabled } }
    }
}

/// The person's initial on the accent, with a dot for online or not.
struct Avatar: View {
    let name: String
    /// The online dot; nil: none.
    let online: Bool?
    var size: CGFloat = 28
    /// The picture set in the app (LocalProfile); nil: the initial.
    var picture: NSImage?

    var body: some View {
        Group {
            if let picture {
                Image(nsImage: picture)
                    .resizable()
                    .interpolation(.high)
                    .clipShape(Circle())
            } else {
                Text(verbatim: String(name.prefix(1)).uppercased())
                    .font(.system(size: size * 0.46, weight: .semibold))
                    .foregroundStyle(Theme.onPrimaryContainer)
                    .frame(width: size, height: size)
                    .background(Theme.primaryContainer, in: Circle())
            }
        }
        .frame(width: size, height: size)
        .overlay(alignment: .bottomTrailing) {
            if let online {
                StatusDot(online: online, size: size * 0.32)
                    .overlay { Circle().stroke(Theme.sidebar, lineWidth: 2) }
                    .offset(x: 2, y: 2)
            }
        }
        .accessibilityHidden(true)
    }
}

enum Profile {
    /// Where you are: the Senate's address while online.
    static func place(_ state: AppState) -> String {
        guard state.online else { return L("a11y.offline") }
        guard let address = state.senates.first?.serverUrl else { return L("a11y.online") }
        guard let url = URL(string: address), let host = url.host() else { return address }
        return url.port.map { "\(host):\($0)" } ?? host
    }
}

/// What went wrong with the last action, until dismissed.
private struct ErrorBanner: View {
    let message: String
    let dismiss: () -> Void

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: Theme.Space.s) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(Theme.attention)
                .accessibilityHidden(true)
            MentionText(message)
                .font(Theme.Fonts.text)
                .foregroundStyle(Theme.onSurface)
                .lineLimit(4)
                .textSelection(.enabled)
            Spacer(minLength: Theme.Space.s)
            Button(action: dismiss) {
                Image(systemName: "xmark")
                    .frame(width: 24, height: 24)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .foregroundStyle(Theme.onSurfaceVariant)
            .accessibilityLabel(L("a11y.dismiss"))
        }
        .padding(.horizontal, Theme.Space.l)
        .padding(.vertical, Theme.Space.s)
        .background(Theme.attentionContainer, in: RoundedRectangle(cornerRadius: Theme.Radius.card))
        .frame(maxWidth: Theme.composerWidth)
        .padding(.horizontal, Theme.Space.xl)
        .padding(.bottom, Theme.Space.s)
        .frame(maxWidth: .infinity)
    }
}

/// The pinned Secretary row: its glyph carries its state as a session's
/// does (working, needs you, paused, stopped), "starting" while a first
/// message starts it.
private struct SecretaryRow: View {
    let store: CoreStore

    var body: some View {
        let line = store.secretarySession
        let state = store.secretary?.state
        FixedRow(
            title: L("sidebar.secretary"),
            count: line.flatMap { $0.unread > 0 ? Int($0.unread) : nil },
            busy: store.secretaryStarting,
            value: store.secretaryStarting ? L("secretary.starting") : line?.status.label ?? state?.label
        ) {
            if let line {
                SessionMark(runtime: line.runtime, status: line.status)
            } else {
                FixedSymbol(name: "person.crop.circle")
            }
        }
    }
}

/// A fixed row at the sidebar's top (Secretary, Inbox, Approvals): its
/// icon centred in the same 20-point column as the session rows' runtime
/// glyph, so every label starts on one line; its count right-aligned.
struct FixedRow<Icon: View>: View {
    let title: String
    var count: Int?
    var busy = false
    /// What VoiceOver reads after the name; the count when nil.
    var value: String?
    @ViewBuilder let icon: Icon

    var body: some View {
        HStack(spacing: 7) {
            icon.frame(width: 20, height: 20)
            Text(verbatim: title)
                .font(Theme.Fonts.row)
                .foregroundStyle(Theme.onSurface)
                .lineLimit(1)
            Spacer(minLength: Theme.Space.xs)
            if busy {
                ProgressView().controlSize(.mini)
            } else if let count {
                Text(verbatim: String(count))
                    .font(Theme.Fonts.meta)
                    .monospacedDigit()
                    .foregroundStyle(Theme.onSurfaceVariant)
            }
        }
        .accessibilityElement(children: .ignore)
        .accessibilityAddTraits(.isStaticText)
        .accessibilityLabel(title)
        .accessibilityValue(value ?? count.map(String.init) ?? "")
    }
}

/// The fixed rows' symbols: one size, weight and rendering, in the label's
/// quiet colour rather than the sidebar's accent.
struct FixedSymbol: View {
    let name: String

    var body: some View {
        Image(systemName: name)
            .symbolRenderingMode(.monochrome)
            .font(.system(size: 14, weight: .regular))
            .imageScale(.medium)
            .foregroundStyle(Theme.onSurfaceVariant)
            .accessibilityHidden(true)
    }
}

/// An archived committee in the sidebar (with "Show archived" on): its
/// name, dimmed, and Unarchive. It has no page to open.
private struct ArchivedRow: View {
    let store: CoreStore
    let committee: String

    var body: some View {
        let busy = store.isBusy("unarchive:\(committee)")
        HStack(spacing: 6) {
            Image(systemName: "archivebox")
                .font(.system(size: 12))
                .foregroundStyle(Theme.onSurfaceVariant)
                .accessibilityHidden(true)
            Text(verbatim: committee)
                .font(Theme.Fonts.row.weight(.semibold))
                .foregroundStyle(Theme.onSurfaceVariant)
                .lineLimit(1)
            Spacer(minLength: Theme.Space.xs)
            if busy {
                ProgressView().controlSize(.mini)
            } else {
                Button { Task { await store.unarchive(committee) } } label: {
                    Image(systemName: "arrow.uturn.backward.circle")
                        .foregroundStyle(Theme.onSurfaceVariant)
                }
                .buttonStyle(.plain)
                .help(L("committees.unarchive_help", committee))
                .accessibilityLabel(L("committees.unarchive"))
                .disabled(!store.state.online)
            }
        }
        .opacity(0.6)
        .contextMenu {
            Button(L("committees.unarchive")) { Task { await store.unarchive(committee) } }
                .disabled(busy || !store.state.online)
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel(committee)
        .accessibilityValue(L("sidebar.archived"))
        .accessibilityIdentifier("archived-\(committee)")
    }
}

/// A committee's thread in the sidebar: how it stands, its title, who is
/// talking to whom, and a dot when something was said since it was opened.
private struct SidebarThreadRow: View {
    let store: CoreStore
    let thread: CommitteeThread

    var body: some View {
        let t = thread
        let unread = store.threads.unread(t)
        HStack(spacing: 7) {
            ThreadStateMark(state: t.state, waiting: t.waitsOn != nil || t.escalatedTo != nil)
                .frame(width: 20, height: 20)
            Text(verbatim: Mentions.shared.plain(t.title, committee: t.committee))
                .font(Theme.Fonts.row.weight(unread ? .semibold : .regular))
                .foregroundStyle(t.state == "archived" ? Theme.onSurfaceVariant : Theme.onSurface)
                .lineLimit(1)
            Spacer(minLength: Theme.Space.xs)
            // Who to whom keeps its room; the title is cut first.
            Text(verbatim: ThreadParties.line(t))
                .font(Theme.Fonts.meta)
                .foregroundStyle(Theme.onSurfaceVariant)
                .lineLimit(1)
                .truncationMode(.tail)
                .frame(maxWidth: 120, alignment: .trailing)
                .fixedSize(horizontal: true, vertical: false)
                .layoutPriority(1)
            if unread {
                Circle().fill(Theme.primary).frame(width: 7, height: 7)
                    .accessibilityHidden(true)
            }
        }
        .help(Mentions.shared.plain(t.title, committee: t.committee))
        .accessibilityElement(children: .ignore)
        .accessibilityAddTraits(.isStaticText)
        .accessibilityLabel(Mentions.shared.plain(t.title, committee: t.committee))
        .accessibilityValue([ThreadParties.line(t), ThreadStateMark.label(t.state, waiting: t.waitsOn != nil),
                             unread ? L("a11y.unread") : nil].compactMap { $0 }.joined(separator: ", "))
    }
}

/// "codex → lead": who opened a thread and whom it is to, by name.
enum ThreadParties {
    static func line(_ t: CommitteeThread) -> String {
        let name = { (s: String) in
            Mentions.shared.plain(s.hasPrefix("[") ? s : "[\(s)]", committee: t.committee)
        }
        let to = t.addressees.prefix(2).map(name).joined(separator: ", ")
            + (t.addressees.count > 2 ? " +\(t.addressees.count - 2)" : "")
        return to.isEmpty ? name(t.opener) : "\(name(t.opener)) → \(to)"
    }
}

/// A thread's state as one small mark: waiting on a person (attention),
/// in progress, open, archived.
struct ThreadStateMark: View {
    let state: String
    let waiting: Bool

    var body: some View {
        Image(systemName: waiting ? "hand.raised.fill" : state == "processing" ? "ellipsis.bubble" :
                state == "archived" ? "archivebox" : "bubble.left.and.bubble.right")
            .font(.system(size: 12))
            .foregroundStyle(waiting ? Theme.attention : state == "processing" ? Theme.primary : Theme.onSurfaceVariant)
            .accessibilityHidden(true)
    }

    static func label(_ state: String, waiting: Bool) -> String {
        waiting ? L("thread.state_waiting") : L("thread.state_\(state)")
    }
}
