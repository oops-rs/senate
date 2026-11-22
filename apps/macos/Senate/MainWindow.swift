import SwiftUI
import SenateCore

/// The window: a sidebar with the sections and the threads waiting on you,
/// and beside it the chosen section or thread. Nothing sits above the
/// content but what it needs at its top right.
struct MainWindow: View {
    @Bindable var store: CoreStore

    var body: some View {
        NavigationSplitView {
            Sidebar(store: store)
                .navigationSplitViewColumnWidth(min: 200, ideal: Theme.sidebarWidth, max: 320)
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
                .toolbar {
                    if store.state.online, store.setupError == nil, store.place == .section(.sessions) {
                        SessionsToolbar(store: store)
                    }
                }
        }
        .background { Shortcuts(store: store) }
        .onChange(of: store.state.menu.docket == nil) { _, gone in
            if gone && store.section == .docket { store.section = .inbox }
        }
    }

    /// The window's name, for Mission Control and VoiceOver; it is not drawn.
    private var title: String {
        if case .thread = store.place, let row = store.selectedRow {
            return Mentions.shared.plain(row.title, committee: row.committee)
        }
        return (store.section ?? .inbox).title
    }

    @ViewBuilder private var content: some View {
        if let problem = store.setupError {
            SetupProblemView(store: store, message: problem)
        } else if !store.state.online {
            OfflineView(store: store)
        } else {
            switch store.place {
            case .thread:
                if let row = store.selectedRow { ThreadPage(store: store, row: row) }
            case .section(.inbox): HomeView(store: store)
            case .section(.approvals): ApprovalsView(store: store)
            case .section(.sessions): SessionsView(store: store)
            case .section(.docket): DocketView(store: store)
            case .section(.senates): SenatesView(store: store)
            }
        }
    }
}

/// ⌘1…⌘5 open the sections in sidebar order; ⌘R reads everything again, or
/// tries the daemon now while it cannot be reached.
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

/// The sidebar: the app's name, the sections, the threads waiting on you
/// (searched and narrowed to an agent here), and who you are at its foot.
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
            AccountRow(store: store)
        }
        .toolbar {
            ToolbarItem {
                Button {
                    if searching { closeSearch() } else { openSearch() }
                } label: {
                    Label(L("inbox.search"), systemImage: "magnifyingglass")
                }
                .help(L("inbox.search"))
                .keyboardShortcut("f")
            }
        }
    }

    private var list: some View {
        List(selection: place) {
            Section {
                ForEach(store.sections, id: \.self) { section in
                    Label(section.title, systemImage: section.symbol)
                        .badge(badge(section))
                        .tag(CoreStore.Place.section(section))
                }
            }
            // Offline, nothing is known to wait: say nothing rather than
            // "nothing waits on you".
            if store.state.online {
              Section {
                let threads = store.inbox
                ForEach(threads) { thread in
                    ThreadRow(thread: thread)
                        .environment(\.mentionCommittee, thread.committee)
                        .tag(CoreStore.Place.thread(thread.key))
                        .contextMenu { ThreadMenu(store: store, thread: thread) }
                }
                if threads.isEmpty {
                    Text(verbatim: store.search.isEmpty ? L("inbox.empty") : L("sidebar.no_matches"))
                        .font(Theme.Fonts.row)
                        .foregroundStyle(Theme.onSurfaceVariant)
                        .selectionDisabled()
                }
            } header: {
                HStack {
                    Text(verbatim: L("sidebar.waiting"))
                    Spacer()
                    if store.inboxAgents.count > 1 {
                        AgentFilter(store: store)
                    }
                }
              }
            }
        }
        .listStyle(.sidebar)
        .scrollContentBackground(.hidden)
        .environment(\.defaultMinListRowHeight, Theme.rowHeight)
    }

    /// A count beside a section only where it is not shown elsewhere in the
    /// sidebar: the threads are listed below, and a terminal's prompt is
    /// already among the approvals.
    private func badge(_ section: CoreStore.Section) -> Int {
        switch section {
        case .approvals, .docket: Int(store.count(section))
        case .inbox, .sessions, .senates: 0
        }
    }

    private var searchField: some View {
        HStack(spacing: 6) {
            Image(systemName: "magnifyingglass")
                .foregroundStyle(Theme.onSurfaceVariant)
                .accessibilityHidden(true)
            TextField(L("inbox.search"), text: $store.search)
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

extension CoreStore.Section {
    var title: String {
        switch self {
        case .inbox: L("tab.home")
        case .approvals: L("tab.approvals")
        case .sessions: L("tab.sessions")
        case .docket: L("tab.docket")
        case .senates: L("tab.senates")
        }
    }

    var symbol: String {
        switch self {
        case .inbox: "house"
        case .approvals: "checkmark.shield"
        case .sessions: "terminal"
        case .docket: "list.bullet.rectangle"
        case .senates: "building.columns"
        }
    }
}

/// One waiting thread: its title on one line, heavier with a dot while it
/// has news you have not read. Where it is and who spoke last are in the
/// tooltip, and in the home view.
private struct ThreadRow: View {
    let thread: InboxThread

    var body: some View {
        HStack(spacing: 6) {
            MentionText(thread.title)
                .font(Theme.Fonts.row.weight(thread.unread ? .semibold : .regular))
                .lineLimit(1)
                .accessibilityValue(thread.unread ? L("a11y.unread") : "")
            Spacer(minLength: Theme.Space.xs)
            if thread.unread {
                Circle()
                    .fill(Theme.primary)
                    .frame(width: 7, height: 7)
                    .accessibilityHidden(true)
            }
        }
        .help(whereAndWho)
    }

    private var whereAndWho: String {
        let from = Mentions.shared.plain(thread.newsFrom ?? thread.from, committee: thread.committee)
        return "\(thread.committee) · \(from) · \(Ago.text(thread.since, now: .now))"
    }
}

/// Narrow the waiting threads to one agent's.
private struct AgentFilter: View {
    @Bindable var store: CoreStore

    var body: some View {
        Menu {
            Picker(L("inbox.agent"), selection: $store.agentFilter) {
                Text(verbatim: L("inbox.all_agents")).tag(String?.none)
                ForEach(store.inboxAgents) { agent in
                    Text(verbatim: agent.pending > 0 ? "\(agent.path) (\(agent.pending))" : agent.path)
                        .tag(Optional(agent.path))
                }
            }
            .pickerStyle(.inline)
        } label: {
            Image(systemName: store.agentFilter == nil
                ? "line.3.horizontal.decrease"
                : "line.3.horizontal.decrease.circle.fill")
                .foregroundStyle(store.agentFilter == nil ? Theme.onSurfaceVariant : Theme.primary)
                .frame(width: 24, height: 24)
                .contentShape(Rectangle())
        }
        .menuStyle(.button)
        .buttonStyle(.plain)
        .menuIndicator(.hidden)
        .fixedSize()
        .help(L("inbox.agent"))
        .accessibilityLabel(L("inbox.agent"))
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
