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
            content
                // Wide enough that nothing in it wraps into a tall column.
                .frame(minWidth: 440, maxWidth: .infinity, maxHeight: .infinity)
                .background(Theme.surface.ignoresSafeArea())
                .safeAreaInset(edge: .top, spacing: 0) {
                    if store.state.online, let error = store.shownError {
                        ErrorBanner(message: error) { store.actionError = nil }
                    }
                }
                .navigationTitle(title)
                .toolbar(removing: .title)
                .toolbarBackgroundVisibility(.hidden, for: .windowToolbar)
        }
        .background { Shortcuts(store: store) }
    }

    /// The window's name, for Mission Control and VoiceOver; it is not drawn.
    private var title: String {
        switch store.place {
        case .thread:
            if let row = store.selectedRow { return Mentions.shared.plain(row.title, committee: row.committee) }
        case .session:
            if let s = store.selectedSession { return s.displayName }
        case let .committee(name):
            return name.isEmpty ? L("sidebar.no_committee") : name
        case let .section(section):
            return section.title
        case .item:
            break
        }
        return Section.inbox.title
    }

    private typealias Section = CoreStore.Section

    @ViewBuilder private var content: some View {
        if let problem = store.setupError {
            SetupProblemView(store: store, message: problem)
        } else if !store.state.online {
            OfflineView(store: store)
        } else {
            switch store.place {
            case .thread:
                if let row = store.selectedRow {
                    ThreadPage(store: store, row: row)
                } else {
                    EmptyState(text: L("thread.gone"))
                }
            case .session:
                if let session = store.selectedSession {
                    SessionPage(store: store, session: session)
                } else {
                    EmptyState(text: L("session.gone"))
                }
            case let .committee(name): CommitteePage(store: store, name: name)
            case .item, .section(.inbox): InboxView(store: store)
            case .section(.approvals): ApprovalsView(store: store)
            case .section(.senates): SenatesView(store: store)
            }
        }
    }
}

/// ⌘1 Inbox, ⌘2 Approvals; ⌘⌥↑ and ⌘⌥↓ move between sessions; ⌘R reads
/// everything again, or tries the daemon now while it cannot be reached.
/// (⌘, is the store's: SwiftUI keeps that key for the Settings scene's own
/// menu item, which a menu-bar-only app has no menu bar to reach.)
/// Invisible buttons: the app has no menu bar of its own (it lives in the
/// status item).
private struct Shortcuts: View {
    let store: CoreStore

    var body: some View {
        ZStack {
            ForEach(Array(store.sections.enumerated()), id: \.element) { index, section in
                Button(section.title) { store.place = .section(section) }
                    .keyboardShortcut(KeyEquivalent(Character(String(index + 1))), modifiers: .command)
            }
            Button(L("sidebar.previous_session")) { store.moveSession(-1) }
                .keyboardShortcut(.upArrow, modifiers: [.command, .option])
            Button(L("sidebar.next_session")) { store.moveSession(1) }
                .keyboardShortcut(.downArrow, modifiers: [.command, .option])
            Button(L("toolbar.refresh")) {
                if store.state.online { store.refresh() } else { store.reconnect() }
            }
            .keyboardShortcut("r")
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
                Image(systemName: "building.columns.fill")
                    .foregroundStyle(Theme.primary)
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
                .keyboardShortcut("f")
            }
        }
    }

    private var list: some View {
        List(selection: place) {
            Section {
                ForEach(store.sections, id: \.self) { section in
                    Label(section.title, systemImage: section.symbol)
                        .badge(store.state.online ? store.count(section) : 0)
                        .tag(CoreStore.Place.section(section))
                }
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
            }
        }
        .listStyle(.sidebar)
        .scrollContentBackground(.hidden)
        .environment(\.defaultMinListRowHeight, Theme.rowHeight)
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

    private var place: Binding<CoreStore.Place?> {
        Binding(
            get: { store.place },
            set: { if let place = $0 { store.place = place } }
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
            StatusMark(status: session.status)
            Text(verbatim: session.displayName)
                .font(Theme.Fonts.row.weight(session.status == .unread || session.status == .needsYou ? .semibold : .regular))
                .foregroundStyle(session.active ? Theme.onSurface : Theme.onSurfaceVariant)
                .lineLimit(1)
            RuntimeMark(runtime: session.runtime)
            Spacer(minLength: Theme.Space.xs)
            Ago(date: session.lastActivity)
                .font(Theme.Fonts.meta)
                .foregroundStyle(Theme.onSurfaceVariant)
        }
        .help(tooltip)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(session.displayName)
        .accessibilityValue("\(session.statusLabel), \(session.runtime), \(session.lastActivity.formatted(.relative(presentation: .named)))")
    }

    private var tooltip: String {
        var lines = [session.title ?? session.displayName]
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
            Button(L("mention.copy", h.label)) { Mentions.copyText(h.label) }
        }
        Button(L("mention.copy", session.sessionId)) { Mentions.copyText(session.sessionId) }
    }
}

extension CoreStore.Section {
    var title: String {
        switch self {
        case .inbox: L("tab.inbox")
        case .approvals: L("tab.approvals")
        case .senates: L("tab.senates")
        }
    }

    var symbol: String {
        switch self {
        case .inbox: "tray"
        case .approvals: "checkmark.shield"
        case .senates: "building.columns"
        }
    }
}

/// The sidebar's filter: the agent whose inbox is shown, and *Show inactive*.
private struct SidebarFilter: View {
    @Bindable var store: CoreStore

    var body: some View {
        Menu {
            if store.inboxAgents.count > 1 {
                Picker(L("inbox.agent"), selection: $store.agentFilter) {
                    Text(verbatim: L("inbox.all_agents")).tag(String?.none)
                    ForEach(store.inboxAgents) { agent in
                        Text(verbatim: agent.pending > 0 ? "\(agent.displayName) (\(agent.pending))" : agent.displayName)
                            .help(agent.path)
                            .tag(Optional(agent.path))
                    }
                }
                .pickerStyle(.inline)
                Divider()
            }
            Toggle(L("roster.show_inactive"), isOn: $store.showInactive)
        } label: {
            Image(systemName: store.agentFilter == nil && !store.showInactive
                ? "line.3.horizontal.decrease"
                : "line.3.horizontal.decrease.circle.fill")
                .foregroundStyle(store.agentFilter == nil && !store.showInactive ? Theme.onSurfaceVariant : Theme.primary)
                .frame(width: 24, height: 24)
                .contentShape(Rectangle())
        }
        .menuStyle(.button)
        .buttonStyle(.plain)
        .menuIndicator(.hidden)
        .fixedSize()
        .help(L("sidebar.filter"))
        .accessibilityLabel(L("sidebar.filter"))
    }
}

/// Who this machine is online as and where, or that it is offline; its
/// menu opens Settings and About.
private struct AccountRow: View {
    let store: CoreStore

    var body: some View {
        let state = store.state
        let name = state.person ?? "Senate"
        Menu {
            Button(L("tab.senates")) { store.place = .section(.senates) }
                .disabled(!state.online)
            Button(L("sessions.watch")) { store.openWatch() }
                .disabled(!state.online || state.sessions.allSatisfy { !$0.running })
            Divider()
            SettingsLink { Text(verbatim: L("menu.settings")) }
            Button(L("menu.about")) { About.show() }
            Divider()
            Button(L("toolbar.refresh")) { store.refresh() }
                .disabled(!state.online)
        } label: {
            HStack(spacing: 10) {
                Text(verbatim: String(name.prefix(1)).uppercased())
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(Theme.onPrimaryContainer)
                    .frame(width: 28, height: 28)
                    .background(Theme.primaryContainer, in: Circle())
                    .overlay(alignment: .bottomTrailing) {
                        StatusDot(online: state.online, size: 9)
                            .overlay { Circle().stroke(Theme.sidebar, lineWidth: 2) }
                            .offset(x: 2, y: 2)
                    }
                VStack(alignment: .leading, spacing: 1) {
                    Text(verbatim: name)
                        .font(Theme.Fonts.label)
                        .foregroundStyle(Theme.onSurface)
                        .lineLimit(1)
                    Text(verbatim: second(state))
                        .font(Theme.Fonts.meta)
                        .foregroundStyle(Theme.onSurfaceVariant)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                Spacer(minLength: 0)
            }
            .contentShape(Rectangle())
        }
        .menuStyle(.button)
        .buttonStyle(.plain)
        .menuIndicator(.hidden)
        .padding(.horizontal, Theme.Space.l)
        .padding(.vertical, Theme.Space.m)
        .help(state.online ? (state.senates.first?.serverUrl ?? "") : (state.offlineReason ?? ""))
        .accessibilityLabel(L("sidebar.account"))
        .accessibilityValue("\(name), \(second(state))")
    }

    /// Under the name: the Senate's address while online.
    private func second(_ state: AppState) -> String {
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
