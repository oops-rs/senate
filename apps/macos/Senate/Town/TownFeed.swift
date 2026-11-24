import Foundation
import SenateCore

/// The town from what the store already has: committees and this machine's
/// sessions (the sidebar's tree), the roster's members on other machines,
/// the threads waiting on the person, the threads between agents, and the
/// Speaker's desk. No new daemon call.
extension TownInput {
    init(store: CoreStore) {
        let state = store.state
        let secretaryLine = store.secretarySession
        let secretaryId = secretaryLine.map { TownText.senatorId($0.mention) }
        let rosters = Dictionary(store.roster.map { ($0.committee, $0) }, uniquingKeysWith: { a, _ in a })
        var committees: [TownCommittee] = []
        for group in state.groups {
            guard let name = group.committee, group.active || store.showInactive else { continue }
            var senators: [TownSenator] = group.sessions
                .filter { store.showInactive || $0.active }
                .map(TownSenator.init(line:))
                .filter { $0.id != secretaryId }
            // Members on other machines: in town, their status unknown here.
            if let roster = rosters[name] {
                for member in roster.members where member.active {
                    let id = TownText.senatorId(member.senatorId)
                    guard id != secretaryId, !senators.contains(where: { $0.id == id }) else { continue }
                    senators.append(TownSenator(
                        id: id, name: member.displayName, runtime: TownRuntime(guessing: id), mood: .idle,
                        chair: roster.chairId.map(TownText.senatorId) == id
                    ))
                }
            }
            senators.sort { ($0.chair ? 0 : 1, $0.name, $0.id) < ($1.chair ? 0 : 1, $1.name, $1.id) }
            let waiting = state.inbox
                .filter { $0.forPerson && $0.committee == name }
                .sorted { $0.since < $1.since }
                .map {
                    TownThreadRef(committee: $0.committee, root: $0.root, title: $0.title,
                                  asker: Mentions.shared.plain($0.asker, committee: $0.committee))
                }
            // The Secretary's own committee, with only the Secretary: the hall is its house.
            if senators.isEmpty && waiting.isEmpty && name == secretaryLine?.committee { continue }
            committees.append(TownCommittee(name: name, senators: senators, waiting: waiting))
        }
        var hall = TownHall(docket: store.docketEntries.count)
        hall.secretary = secretaryLine.map(TownSenator.init(line:))
        self.state = TownState(committees: committees, hall: hall, person: state.person)
        threads = store.agentThreads.map(TownThread.init(row:))
    }
}

extension TownSenator {
    init(line: SessionLine) {
        self.init(
            id: TownText.senatorId(line.mention), name: line.displayName, runtime: TownRuntime(line.runtime),
            mood: TownMood(line.status), chair: line.chair, sessionId: line.sessionId,
            doing: line.title
        )
    }
}

extension TownMood {
    init(_ status: SessionStatus) {
        switch status {
        case .needsYou: self = .waiting
        case .working: self = .working
        case .unread, .idle: self = .idle
        case .paused: self = .paused
        case .stopped: self = .stopped
        }
    }
}

extension TownRuntime {
    /// A senator id names its runtime first: `claude-…`, `codex-…`.
    init(guessing id: String) {
        let head = id.split(separator: "-").first.map(String.init) ?? id
        self.init(head)
    }
}

extension TownThread {
    /// Who spoke last, as the row tells: the news's author when there is
    /// news, else who opened it; spoken to whoever is on the other side.
    init(row: InboxThread) {
        let opener = TownText.senatorId(row.from)
        let speaker = row.newsFrom.map(TownText.senatorId) ?? opener
        let listener: String? = if speaker == opener {
            row.owner.map(TownText.senatorId) ?? row.to.first.map(TownText.senatorId)
        } else {
            opener
        }
        self.init(
            ref: TownThreadRef(committee: row.committee, root: row.root, title: row.title),
            updated: row.updated, speaker: speaker, listener: listener,
            line: row.newsLine ?? row.title, answered: row.answered
        )
    }
}
