import SwiftUI
import SenateCore

/// `chair ● ledger`, or that the committee has none.
private struct ChairLine: View {
    let committee: RosterCommittee
    var copyable = true

    var body: some View {
        HStack(spacing: 6) {
            Text(verbatim: L("committees.chair"))
                .foregroundStyle(Theme.onSurfaceVariant)
            if let id = committee.chairId {
                ChairName(name: committee.chairDisplayName, id: id, live: committee.chairLive,
                          committee: committee.committee, copyable: copyable)
            } else {
                Text(verbatim: L("senates.no_chair")).foregroundStyle(Theme.onSurfaceVariant)
            }
        }
    }
}

/// One committee: its chair, what waits on you there, its sessions on this
/// machine, the threads your agents are working on there, and its members.
/// The page of no committee lists the sessions on none.
struct CommitteePage: View {
    @Bindable var store: CoreStore
    let name: String

    var body: some View {
        let roster = store.roster.first { $0.committee == name }
        let group = store.state.groups.first { ($0.committee ?? "") == name }
        let sessions = (group?.sessions ?? []).filter { store.showInactive || $0.active }
        let waiting = store.waitingByCommittee.first { $0.committee == name }?.items ?? []
        let working = store.agentThreads.filter { $0.committee == name }
        ReadingColumn(spacing: Theme.Space.xs) {
            PageHeading(text: name.isEmpty ? L("sidebar.no_committee") : name)
            if let roster {
                HStack(spacing: Theme.Space.m) {
                    ChairLine(committee: roster)
                    Text(verbatim: L("committees.active", Int64(roster.activeMembers)))
                        .foregroundStyle(Theme.onSurfaceVariant)
                }
                .font(Theme.Fonts.row)
                .padding(.leading, Theme.Space.xs)
            }
            Color.clear.frame(height: Theme.Space.m)
            if !waiting.isEmpty {
                SectionHeader(L("sidebar.waiting"))
                ForEach(waiting) { item in
                    WaitingRow(store: store, item: item)
                }
            }
            if !sessions.isEmpty {
                HStack(alignment: .bottom) {
                    SectionHeader(L("committees.sessions"))
                    Spacer(minLength: Theme.Space.m)
                    InactiveNote(store: store, hidden: (group?.sessions.count ?? 0) - sessions.count)
                }
                VStack(alignment: .leading, spacing: 2) {
                    ForEach(sessions) { session in
                        SessionLink(store: store, session: session)
                    }
                }
            }
            if !working.isEmpty {
                SectionHeader(L("committees.threads"))
                ForEach(working) { thread in
                    WaitingRow(store: store, item: Waiting(kind: .thread(thread)))
                }
            }
            if let roster {
                let members = roster.members
                    .filter { store.showInactive || $0.active }
                    .sorted { a, b in
                        // The chair first, then by name.
                        let chairA = a.senatorId == roster.chairId, chairB = b.senatorId == roster.chairId
                        return chairA != chairB ? chairA : a.displayName < b.displayName
                    }
                HStack(alignment: .bottom) {
                    SectionHeader(L("committees.members"))
                    Spacer(minLength: Theme.Space.m)
                    InactiveNote(store: store, hidden: roster.members.count - members.count)
                }
                VStack(alignment: .leading, spacing: Theme.Space.s) {
                    ForEach(members) { member in
                        HStack(spacing: 6) {
                            StatusDot(online: member.active, size: 6)
                            SenatorName(name: member.displayName, id: member.senatorId)
                            if member.senatorId == roster.chairId {
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
        }
        .environment(\.mentionCommittee, name.isEmpty ? nil : name)
    }
}

/// A session on the committee's page: its status, name, title and age;
/// it opens the session.
private struct SessionLink: View {
    let store: CoreStore
    let session: SessionLine
    @State private var hovered = false

    var body: some View {
        Button {
            store.place = .session(session.sessionId)
        } label: {
            HStack(spacing: Theme.Space.s) {
                SessionMark(runtime: session.runtime, status: session.status)
                Text(verbatim: session.displayName)
                    .font(Theme.Fonts.text.weight(.medium))
                    .foregroundStyle(Theme.onSurface)
                if let title = session.title {
                    Text(verbatim: title)
                        .font(Theme.Fonts.row)
                        .foregroundStyle(Theme.onSurfaceVariant)
                        .lineLimit(1)
                }
                Spacer(minLength: Theme.Space.s)
                Text(verbatim: session.statusLabel)
                    .font(Theme.Fonts.meta)
                    .foregroundStyle(Theme.onSurfaceVariant)
                Ago(date: session.lastActivity)
                    .font(Theme.Fonts.meta)
                    .foregroundStyle(Theme.onSurfaceVariant)
            }
            .padding(.horizontal, Theme.Space.m)
            .padding(.vertical, 9)
            .background(hovered ? Theme.container : .clear, in: RoundedRectangle(cornerRadius: Theme.Radius.card))
            .contentShape(RoundedRectangle(cornerRadius: Theme.Radius.card))
        }
        .buttonStyle(.plain)
        .onHover { hovered = $0 }
        .accessibilityElement(children: .ignore)
        .accessibilityAddTraits(.isButton)
        .accessibilityLabel(session.displayName)
        .accessibilityValue("\(session.statusLabel), \(session.title ?? "")")
    }
}
