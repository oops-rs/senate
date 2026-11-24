import SwiftUI
import SenateCore

/// Inbox: the threads waiting on you for a reply, by committee, each with
/// who asks. What waits for a yes or a no (permissions, approvals, what is
/// put to you through the Speaker's desk) is in Approvals. Below, folded,
/// what your agents are working on: it waits on them, not on you. A row
/// opens the thread.
struct InboxView: View {
    let store: CoreStore
    @State private var agentsOpen = false

    var body: some View {
        content
            .toolbar {
                ToolbarSpacer(.flexible)
                if store.inboxAgents.count > 1 {
                    ToolbarItem(placement: .primaryAction) { AgentFilter(store: store) }
                }
                ToolbarItem(placement: .primaryAction) { RefreshButton(store: store) }
            }
    }

    @ViewBuilder private var content: some View {
        let shown = Set(store.inbox.map(\.key))
        let threads = store.inboxThreads.filter { shown.contains($0.key) }
        let groups = Dictionary(grouping: threads, by: \.committee)
            .map { CommitteeWaiting(committee: $0.key, items: $0.value.sorted { $0.since < $1.since }.map { Waiting(kind: .thread($0)) }) }
            .sorted { ($0.items.first?.since ?? .now, $0.committee) < ($1.items.first?.since ?? .now, $1.committee) }
        let working = store.agentThreads.filter { shown.contains($0.key) }
        let name = store.state.person ?? ""
        let decide = store.approvalsCount
        if threads.isEmpty && working.isEmpty {
            EmptyState(
                text: store.query.isEmpty ? L("home.clear", name) : L("sidebar.no_matches"),
                detail: decide > 0 ? L("inbox.see_approvals", Int64(decide)) : L("home.clear_detail")
            )
        } else {
            ReadingColumn(spacing: Theme.Space.xs) {
                HStack(alignment: .firstTextBaseline) {
                    Text(verbatim: threads.isEmpty ? L("home.clear", name) : L("inbox.waiting", name, Int64(threads.count)))
                        .font(Theme.Fonts.display)
                        .foregroundStyle(Theme.onSurface)
                        .accessibilityAddTraits(.isHeader)
                    Spacer(minLength: Theme.Space.m)
                }
                .padding(.top, Theme.Space.xxxl)
                .padding(.leading, Theme.Space.xs)
                if decide > 0 {
                    Button {
                        store.place = .section(.approvals)
                    } label: {
                        Label(L("inbox.see_approvals", Int64(decide)), systemImage: "checkmark.shield")
                            .font(Theme.Fonts.row)
                            .foregroundStyle(Theme.attention)
                    }
                    .buttonStyle(.plain)
                    .padding(.leading, Theme.Space.xs)
                }
                Color.clear.frame(height: Theme.Space.s)
                ForEach(groups) { group in
                    CommitteeHeader(store: store, committee: group.committee, count: group.items.count)
                    // Lazy: a committee with thousands draws what is on screen.
                    LazyVStack(alignment: .leading, spacing: Theme.Space.xs) {
                        ForEach(group.items) { item in
                            WaitingRow(store: store, item: item)
                                .contextMenu {
                                    if case let .thread(t) = item.kind { ThreadMenu(store: store, thread: t) }
                                }
                        }
                    }
                    .accessibilityElement(children: .contain)
                    .accessibilityLabel(group.committee)
                }
                if !working.isEmpty {
                    DisclosureGroup(isExpanded: $agentsOpen) {
                        LazyVStack(alignment: .leading, spacing: Theme.Space.xs) {
                            ForEach(working) { thread in
                                WaitingRow(store: store, item: Waiting(kind: .thread(thread)), showsCommittee: true)
                            }
                        }
                        .padding(.top, Theme.Space.xs)
                    } label: {
                        Text(verbatim: L("home.agents_working", Int64(working.count)))
                            .font(Theme.Fonts.row)
                            .foregroundStyle(Theme.onSurfaceVariant)
                            .accessibilityAddTraits(.isHeader)
                    }
                    .padding(.top, Theme.Space.l)
                    .padding(.leading, Theme.Space.xs)
                }
            }
        }
    }
}

/// A committee's name over what waits on you there, with its chair; it
/// opens the committee.
struct CommitteeHeader: View {
    let store: CoreStore
    let committee: String
    let count: Int

    var body: some View {
        let chair = store.roster.first { $0.committee == committee }
        Button {
            store.place = .committee(committee)
        } label: {
            HStack(spacing: 6) {
                Text(verbatim: committee)
                    .font(Theme.Fonts.row.weight(.semibold))
                    .foregroundStyle(Theme.onSurface)
                Text(verbatim: "\(count)")
                    .font(Theme.Fonts.row)
                    .foregroundStyle(Theme.onSurfaceVariant)
                if let chair, let id = chair.chairId {
                    Text(verbatim: "·").foregroundStyle(Theme.onSurfaceVariant)
                    ChairName(name: chair.chairDisplayName, id: id, live: chair.chairLive, committee: committee,
                              copyable: false)
                        .font(Theme.Fonts.row)
                }
                Spacer(minLength: 0)
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .padding(.top, Theme.Space.l)
        .padding(.leading, Theme.Space.xs)
        .help(L("committees.open", committee))
        .accessibilityAddTraits(.isHeader)
    }
}

/// One waiting item: what it is, what it asks or what was last said, where
/// and for how long. The whole row opens it.
struct WaitingRow: View {
    let store: CoreStore
    let item: Waiting
    /// Say which committee it is on (where rows are not grouped by one).
    var showsCommittee = false
    @State private var hovered = false

    /// What VoiceOver says after the title.
    private var spoken: String {
        var parts = [Mentions.shared.plain(item.detail, committee: item.committee)]
        if let committee = item.committee { parts.append(committee) }
        parts.append(item.since.formatted(.relative(presentation: .named)))
        if item.unread { parts.append(L("a11y.unread")) }
        return parts.joined(separator: ", ")
    }

    var body: some View {
        Button {
            store.open(item)
        } label: {
            HStack(alignment: .top, spacing: Theme.Space.m) {
                Image(systemName: item.symbol)
                    .font(.system(size: 14))
                    .foregroundStyle(item.isPermission ? Theme.attention : Theme.onSurfaceVariant)
                    .frame(width: 20, height: 20)
                    .accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 2) {
                    HStack(alignment: .firstTextBaseline, spacing: 6) {
                        MentionText(item.title)
                            .font(Theme.Fonts.text.weight(item.unread ? .semibold : .medium))
                            .foregroundStyle(Theme.onSurface)
                            .lineLimit(1)
                        if item.unread {
                            Circle()
                                .fill(Theme.primary)
                                .frame(width: 7, height: 7)
                                .accessibilityLabel(L("a11y.unread"))
                        }
                    }
                    MentionText(item.detail)
                        .font(item.isPermission ? Theme.Fonts.code : Theme.Fonts.row)
                        .foregroundStyle(Theme.onSurfaceVariant)
                        .lineLimit(2)
                        .multilineTextAlignment(.leading)
                }
                Spacer(minLength: Theme.Space.m)
                HStack(spacing: 6) {
                    if let kind = item.kindLabel {
                        Text(verbatim: kind)
                    }
                    if showsCommittee, let committee = item.committee {
                        Text(verbatim: committee)
                    }
                    Ago(date: item.since)
                }
                .font(Theme.Fonts.meta)
                .foregroundStyle(Theme.onSurfaceVariant)
                .lineLimit(1)
            }
            .padding(.horizontal, Theme.Space.m)
            .padding(.vertical, 10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(
                hovered ? Theme.container : .clear,
                in: RoundedRectangle(cornerRadius: Theme.Radius.card)
            )
            .contentShape(RoundedRectangle(cornerRadius: Theme.Radius.card))
        }
        .buttonStyle(.plain)
        .onHover { hovered = $0 }
        .environment(\.mentionCommittee, item.committee)
        // One element, named as it reads: mentions as names, not as written.
        .accessibilityElement(children: .ignore)
        .accessibilityAddTraits(.isButton)
        .accessibilityLabel(Mentions.shared.plain(item.title, committee: item.committee))
        .accessibilityValue(spoken)
    }
}

/// Read everything again (⌘R in the View menu).
struct RefreshButton: View {
    let store: CoreStore

    var body: some View {
        Button {
            if store.state.online { store.refresh() } else { store.reconnect() }
        } label: {
            Label(L("toolbar.refresh"), systemImage: "arrow.clockwise")
        }
        .help(L("toolbar.refresh"))
    }
}
