import SwiftUI
import SenateCore

/// Your profile: who you are (your picture and name here, your Senate
/// handle, online or not), then for each Senate the daemon serves: what
/// it is, your agents as a table, the committees you chair or sit on, and
/// the Senates registered below it, when there are any. Sections under
/// headings, aligned columns, no cards.
struct SenatesView: View {
    @Bindable var store: CoreStore
    @State private var editing = false

    var body: some View {
        let state = store.state
        if state.senates.isEmpty {
            EmptyState(text: L("senates.empty"))
        } else {
            ScrollView {
                VStack(alignment: .leading, spacing: Theme.Space.xxl) {
                    header(state)
                    ForEach(state.senates) { senate in
                        SenateSections(store: store, senate: senate, titled: state.senates.count > 1)
                    }
                }
                .frame(maxWidth: Theme.readingWidth, alignment: .leading)
                .padding(.horizontal, Theme.Space.xl)
                .padding(.vertical, Theme.Space.xl)
                .frame(maxWidth: .infinity)
            }
            .toolbar {
                ToolbarItem(placement: .navigation) {
                    Text(verbatim: L("tab.profile"))
                        .font(.headline)
                        .padding(.horizontal, Theme.Space.xs)
                        .accessibilityAddTraits(.isHeader)
                }
                .sharedBackgroundVisibility(.hidden)
                ToolbarSpacer(.flexible)
                // The page's one action: its words and icon.
                ToolbarItem(placement: .primaryAction) {
                    Button { editing = true } label: {
                        Label(L("profile.edit"), systemImage: "pencil")
                    }
                    .help(L("profile.edit_title"))
                    .accessibilityIdentifier("profile-edit")
                }
            }
            .sheet(isPresented: $editing) {
                ProfileEditor(profile: store.profile, handle: state.person ?? "") { editing = false }
            }
        }
    }

    private func header(_ state: AppState) -> some View {
        let handle = state.person ?? "Senate"
        return HStack(alignment: .center, spacing: Theme.Space.l) {
            Avatar(name: store.profile.name(handle), online: nil, size: 72, picture: store.profile.picture)
            VStack(alignment: .leading, spacing: 4) {
                let name = store.profile.name(handle)
                Text(verbatim: name)
                    .font(.system(size: 26, weight: .semibold))
                    .foregroundStyle(Theme.onSurface)
                    .help(name == handle ? "senator::\(handle)" : "")
                    .accessibilityAddTraits(.isHeader)
                // The handle as names are shown everywhere: the name, the
                // full id in the tooltip and the context menu. Under a name
                // set here; otherwise the name above is the handle.
                if name != handle {
                    Text(verbatim: handle)
                        .font(Theme.Fonts.row)
                        .foregroundStyle(Theme.onSurfaceVariant)
                        .help("senator::\(handle)")
                        .contextMenu {
                            Button(L("mention.copy", "[senator::\(handle)]")) { Mentions.copy("senator::\(handle)") }
                        }
                }
                HStack(spacing: 6) {
                    StatusDot(online: state.online, size: 7)
                    Text(verbatim: state.online ? L("a11y.online") : (state.offlineReason ?? L("a11y.offline")))
                        .font(Theme.Fonts.row)
                        .foregroundStyle(Theme.onSurfaceVariant)
                }
            }
            Spacer(minLength: 0)
        }
    }
}

/// One Senate: what it is, your agents, your committees, the Senates below.
private struct SenateSections: View {
    let store: CoreStore
    let senate: SenateInfo
    /// More than one Senate: each section heads with its name.
    let titled: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.xxl) {
            Section_(titled ? senate.title : L("profile.this_senate")) {
                Grid(alignment: .leadingFirstTextBaseline, horizontalSpacing: Theme.Space.xl, verticalSpacing: 10) {
                    fact(L("profile.senate_name")) {
                        HStack(spacing: 6) {
                            Text(verbatim: senate.senate ?? senate.title)
                            if senate.primary {
                                Text(verbatim: L("senates.primary")).foregroundStyle(Theme.onSurfaceVariant)
                            }
                        }
                    }
                    fact(L("senates.address")) { Text(verbatim: senate.serverUrl).textSelection(.enabled) }
                    fact(L("senates.you")) { MentionText(senate.me) }
                    fact(L("senates.speaker")) {
                        if let speaker = senate.speaker {
                            MentionText(speaker)
                        } else {
                            Text(verbatim: L("senates.no_speaker")).foregroundStyle(Theme.onSurfaceVariant)
                        }
                    }
                    if let uplink = senate.uplink {
                        fact(L("profile.registration")) { Text(verbatim: uplink) }
                    }
                }
            }
            let agents = store.state.agents.filter { $0.senate == senate.key && (store.showInactive || $0.active) }
            if !agents.isEmpty {
                Section_(L("profile.agents")) { agentsTable(agents) }
            }
            let mine = committees
            if !mine.isEmpty {
                Section_(L("profile.committees")) { committeesTable(mine) }
            }
            if !senate.children.isEmpty {
                Section_(L("senates.children")) {
                    VStack(alignment: .leading, spacing: Theme.Space.m) {
                        ForEach(senate.children, id: \.name) { ChildBlock(child: $0) }
                    }
                }
            }
        }
        .font(Theme.Fonts.text)
        .foregroundStyle(Theme.onSurface)
    }

    private func fact<V: View>(_ label: String, @ViewBuilder _ value: () -> V) -> some View {
        GridRow {
            Text(verbatim: label)
                .foregroundStyle(Theme.onSurfaceVariant)
                .gridColumnAlignment(.trailing)
            value()
        }
    }

    // MARK: Your agents

    private func agentsTable(_ agents: [AgentInfo]) -> some View {
        Grid(alignment: .leadingFirstTextBaseline, horizontalSpacing: Theme.Space.l, verticalSpacing: 8) {
            GridRow {
                ColumnHead(L("profile.col.name"))
                ColumnHead(L("profile.col.runtime"))
                ColumnHead(L("profile.col.committees"))
                ColumnHead(L("profile.col.sessions")).gridColumnAlignment(.trailing)
                ColumnHead(L("profile.col.waiting")).gridColumnAlignment(.trailing)
            }
            Divider().gridCellUnsizedAxes(.horizontal)
            ForEach(agents) { agent in
                GridRow {
                    HStack(spacing: 6) {
                        SenatorName(name: agent.displayName, id: agent.path)
                        if agent.presiding {
                            Text(verbatim: L("senates.presiding")).font(Theme.Fonts.meta).foregroundStyle(Theme.primary)
                        }
                    }
                    Text(verbatim: agent.runtime ?? "—").foregroundStyle(Theme.onSurfaceVariant)
                    Text(verbatim: committees(of: agent).joined(separator: ", ").ifEmpty("—"))
                        .foregroundStyle(Theme.onSurfaceVariant)
                        .lineLimit(1)
                    Text(verbatim: agent.sessions > 0 ? String(agent.sessions) : "—")
                        .monospacedDigit()
                        .foregroundStyle(Theme.onSurfaceVariant)
                    Text(verbatim: agent.pending > 0 ? String(agent.pending) : "—")
                        .monospacedDigit()
                        .foregroundStyle(agent.pending > 0 ? Theme.attention : Theme.onSurfaceVariant)
                }
                .opacity(agent.active ? 1 : 0.55)
            }
        }
    }

    private func committees(of agent: AgentInfo) -> [String] {
        Array(Set(senate.committees.filter { $0.agent == agent.path }.map(\.committee))).sorted()
    }

    // MARK: Your committees

    private struct Membership: Identifiable {
        let committee: String
        let chair: String?
        let agents: [String]
        let live: Bool
        var id: String { committee }
    }

    /// The committees where you, or an agent of yours, sit or chair.
    private var committees: [Membership] {
        let lines = senate.committees.filter { store.showInactive || $0.live }
        // An agent by the name it goes by, not its path.
        let names = Dictionary(
            store.state.agents.filter { $0.senate == senate.key }.map { ($0.path, $0.displayName) },
            uniquingKeysWith: { first, _ in first }
        )
        return Dictionary(grouping: lines, by: \.committee).map { committee, lines in
            Membership(
                committee: committee,
                chair: senate.chairs.first { $0.committee == committee }?.chair,
                agents: Array(Set(lines.map { names[$0.agent] ?? $0.agent })).sorted(),
                live: lines.contains { $0.live }
            )
        }
        .sorted { $0.committee < $1.committee }
    }

    private func committeesTable(_ rows: [Membership]) -> some View {
        Grid(alignment: .leadingFirstTextBaseline, horizontalSpacing: Theme.Space.l, verticalSpacing: 8) {
            GridRow {
                ColumnHead(L("profile.col.committee"))
                ColumnHead(L("profile.col.chair"))
                ColumnHead(L("profile.agents"))
            }
            Divider().gridCellUnsizedAxes(.horizontal)
            ForEach(rows) { row in
                GridRow {
                    HStack(spacing: 6) {
                        StatusDot(online: row.live, size: 6)
                        Button { store.place = .committee(row.committee) } label: {
                            Text(verbatim: row.committee).foregroundStyle(Theme.committee)
                        }
                        .buttonStyle(.plain)
                        .help(L("committees.open", row.committee))
                    }
                    Group {
                        if let chair = row.chair {
                            MentionText(chair).environment(\.mentionCommittee, row.committee)
                        } else {
                            Text(verbatim: "—").foregroundStyle(Theme.onSurfaceVariant)
                        }
                    }
                    Text(verbatim: row.agents.joined(separator: ", "))
                        .foregroundStyle(Theme.onSurfaceVariant)
                        .lineLimit(1)
                }
            }
        }
    }
}

/// A section of the profile: a heading, then its content.
private struct Section_<Content: View>: View {
    let title: String
    @ViewBuilder let content: Content

    init(_ title: String, @ViewBuilder content: () -> Content) {
        self.title = title
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.m) {
            Text(verbatim: title)
                .font(Theme.Fonts.heading)
                .foregroundStyle(Theme.onSurface)
                .accessibilityAddTraits(.isHeader)
            content
        }
    }
}

/// A table's column heading.
private struct ColumnHead: View {
    let title: String
    init(_ title: String) { self.title = title }

    var body: some View {
        Text(verbatim: title)
            .font(Theme.Fonts.meta)
            .foregroundStyle(Theme.onSurfaceVariant)
            .textCase(nil)
    }
}

/// A Senate registered below: its Speaker, when it was last seen, and the
/// seats it declares.
private struct ChildBlock: View {
    let child: ChildLine

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.xs) {
            HStack(spacing: 6) {
                StatusDot(online: child.online, size: 6)
                Text(verbatim: child.name).fontWeight(.medium)
                Text(verbatim: L("senates.child_speaker")).foregroundStyle(Theme.onSurfaceVariant)
                MentionText(child.speaker)
                if let seen = child.lastSeen {
                    TimedMention(date: seen) { L("senates.seen", $0) }
                        .foregroundStyle(Theme.onSurfaceVariant)
                }
            }
            ForEach(child.seats, id: \.senator) { seat in
                HStack(spacing: 6) {
                    Text(verbatim: seat.name)
                        .fontWeight(.medium)
                        .foregroundStyle(Theme.agent)
                        .help("[senator::\(seat.senator)]")
                    Text(verbatim: "\(seat.committee) · \(seat.presenceLabel)")
                        .foregroundStyle(Theme.onSurfaceVariant)
                    if let duty = seat.duty {
                        Text(verbatim: duty).foregroundStyle(Theme.onSurfaceVariant).lineLimit(1)
                    }
                }
                .padding(.leading, 14)
            }
        }
    }
}

private extension String {
    func ifEmpty(_ other: String) -> String { isEmpty ? other : self }
}
