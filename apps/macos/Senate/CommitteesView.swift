import SwiftUI
import SenateCore

/// Every committee of the Senate, one card each: its chair (by name, live
/// or not), how many members are active, and what waits on you there. A
/// card opens the committee: its roster and its threads. Committees with
/// no one active and nothing for you are hidden until "Show inactive".
/// Who counts as active, and every name, come from the core's roster.
struct CommitteesView: View {
    @Bindable var store: CoreStore

    var body: some View {
        if let name = store.selectedCommittee, let committee = store.roster.first(where: { $0.committee == name }) {
            CommitteePage(store: store, committee: committee)
        } else {
            list
        }
    }

    @ViewBuilder private var list: some View {
        let all = store.roster
        let shown = store.showInactive ? all : all.filter { store.isActive($0) }
        if all.isEmpty {
            EmptyState(text: L("committees.empty"))
        } else {
            ReadingColumn {
                HStack(alignment: .top) {
                    PageHeading(text: L("tab.committees"))
                    Spacer(minLength: Theme.Space.m)
                    InactiveToggle(store: store, hidden: all.count - shown.count)
                        .padding(.top, Theme.Space.xl)
                }
                .padding(.bottom, Theme.Space.s)
                ForEach(shown) { committee in
                    CommitteeCard(store: store, committee: committee)
                }
            }
        }
    }
}

/// One committee in the list: name, chair, members active, what waits on
/// you there. The whole card opens it.
private struct CommitteeCard: View {
    let store: CoreStore
    let committee: RosterCommittee
    @State private var hovered = false

    var body: some View {
        let waiting = store.waitingByCommittee.first { $0.committee == committee.committee }?.items.count ?? 0
        Button {
            store.selectedCommittee = committee.committee
        } label: {
            VStack(alignment: .leading, spacing: Theme.Space.s) {
                HStack(spacing: Theme.Space.s) {
                    Text(verbatim: committee.committee)
                        .font(Theme.Fonts.heading)
                        .foregroundStyle(Theme.onSurface)
                    if waiting > 0 {
                        Chip(text: L("committees.waiting", Int64(waiting)), tone: .attention)
                    }
                    Spacer(minLength: Theme.Space.s)
                    Image(systemName: "chevron.right")
                        .font(.system(size: 12, weight: .semibold))
                        .foregroundStyle(Theme.onSurfaceVariant)
                        .accessibilityHidden(true)
                }
                HStack(spacing: Theme.Space.m) {
                    ChairLine(committee: committee)
                    Text(verbatim: L("committees.active", Int64(committee.activeMembers)))
                        .foregroundStyle(Theme.onSurfaceVariant)
                    if committee.pending > 0 {
                        Text(verbatim: L("committees.open_threads", Int64(committee.pending)))
                            .foregroundStyle(Theme.onSurfaceVariant)
                    }
                }
                .font(Theme.Fonts.row)
                .lineLimit(1)
            }
            .padding(Theme.Space.l)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(hovered ? Theme.containerHigh : Theme.container,
                        in: RoundedRectangle(cornerRadius: Theme.Radius.card))
            .contentShape(RoundedRectangle(cornerRadius: Theme.Radius.card))
        }
        .buttonStyle(.plain)
        .onHover { hovered = $0 }
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(.isButton)
    }
}

/// `chair ● ledger`, or that the committee has none.
private struct ChairLine: View {
    let committee: RosterCommittee

    var body: some View {
        HStack(spacing: 6) {
            Text(verbatim: L("committees.chair"))
                .foregroundStyle(Theme.onSurfaceVariant)
            if let id = committee.chairId {
                ChairName(name: committee.chairDisplayName, id: id, live: committee.chairLive,
                          committee: committee.committee)
            } else {
                Text(verbatim: L("senates.no_chair")).foregroundStyle(Theme.onSurfaceVariant)
            }
        }
    }
}

/// One committee: its chair, what waits on you there, the threads your
/// agents are working on there, and its members.
private struct CommitteePage: View {
    @Bindable var store: CoreStore
    let committee: RosterCommittee

    var body: some View {
        let name = committee.committee
        let waiting = store.waitingByCommittee.first { $0.committee == name }?.items ?? []
        let working = store.agentThreads.filter { $0.committee == name }
        let members = committee.members
            .filter { store.showInactive || $0.active }
            .sorted { a, b in
                // The chair first, then by name.
                let chairA = a.senatorId == committee.chairId, chairB = b.senatorId == committee.chairId
                return chairA != chairB ? chairA : a.displayName < b.displayName
            }
        ReadingColumn(spacing: Theme.Space.xs) {
            Button {
                store.selectedCommittee = nil
            } label: {
                Label(L("committees.all"), systemImage: "chevron.left")
                    .font(Theme.Fonts.row)
            }
            .buttonStyle(.plain)
            .foregroundStyle(Theme.onSurfaceVariant)
            .padding(.top, Theme.Space.l)
            .padding(.leading, Theme.Space.xs)
            PageHeading(text: name)
            HStack(spacing: Theme.Space.m) {
                ChairLine(committee: committee)
                Text(verbatim: L("committees.active", Int64(committee.activeMembers)))
                    .foregroundStyle(Theme.onSurfaceVariant)
            }
            .font(Theme.Fonts.row)
            .padding(.leading, Theme.Space.xs)
            .padding(.bottom, Theme.Space.m)
            if !waiting.isEmpty {
                SectionHeader(L("sidebar.waiting"))
                ForEach(waiting) { item in
                    WaitingRow(store: store, item: item)
                }
            }
            if !working.isEmpty {
                SectionHeader(L("committees.threads"))
                ForEach(working) { thread in
                    WaitingRow(store: store, item: Waiting(kind: .thread(thread)))
                }
            }
            HStack(alignment: .bottom) {
                SectionHeader(L("committees.members"))
                Spacer(minLength: Theme.Space.m)
                InactiveToggle(store: store, hidden: committee.members.count - members.count)
            }
            VStack(alignment: .leading, spacing: Theme.Space.s) {
                ForEach(members) { member in
                    HStack(spacing: 6) {
                        StatusDot(online: member.active, size: 6)
                        SenatorName(name: member.displayName, id: member.senatorId)
                        if member.senatorId == committee.chairId {
                            Chip(text: L("committees.chair"), tone: .accent)
                        }
                        if !member.active {
                            Chip(text: L("roster.inactive"))
                        }
                    }
                }
                if members.isEmpty {
                    Text(verbatim: L("committees.no_members"))
                        .foregroundStyle(Theme.onSurfaceVariant)
                }
            }
            .font(Theme.Fonts.text)
            .card()
        }
        .environment(\.mentionCommittee, name)
    }
}
