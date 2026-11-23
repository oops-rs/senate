import SwiftUI
import SenateCore

/// Home: everything waiting on you, the most pressing first. What holds an
/// agent at a permission, then each committee's requests, escalations,
/// threads with news for you and items put to the Speaker, each with who
/// asks. Below, folded, what your agents are still working on: it waits on
/// them, not on you. A row opens what it names.
struct HomeView: View {
    let store: CoreStore
    @State private var agentsOpen = false

    var body: some View {
        let permissions = store.permissionsWaiting
        let groups = store.waitingByCommittee
        let working = store.agentThreads
        let name = store.state.person ?? ""
        let total = store.waitingCount
        if total == 0 && working.isEmpty {
            EmptyState(text: L("home.clear", name), detail: L("home.clear_detail"))
        } else {
            ReadingColumn(spacing: Theme.Space.xs) {
                Text(verbatim: total == 0 ? L("home.clear", name) : L("home.waiting", name, Int64(total)))
                    .font(Theme.Fonts.display)
                    .foregroundStyle(Theme.onSurface)
                    .padding(.top, Theme.Space.xxxl)
                    .padding(.bottom, Theme.Space.l)
                    .padding(.leading, Theme.Space.xs)
                    .accessibilityAddTraits(.isHeader)
                if !permissions.isEmpty {
                    SectionHeader(L("home.permissions"))
                    ForEach(permissions) { item in
                        WaitingRow(store: store, item: item, showsCommittee: true)
                    }
                }
                ForEach(groups) { group in
                    CommitteeHeader(store: store, committee: group.committee, count: group.items.count)
                    // Lazy: a committee with thousands draws what is on screen.
                    LazyVStack(alignment: .leading, spacing: Theme.Space.xs) {
                        ForEach(group.items) { item in
                            WaitingRow(store: store, item: item)
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
            store.show(.committees)
            store.selectedCommittee = committee
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
                    ChairName(name: chair.chairDisplayName, id: id, live: chair.chairLive, committee: committee)
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
