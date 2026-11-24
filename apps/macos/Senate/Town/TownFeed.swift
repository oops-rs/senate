import Foundation
import SenateCore

/// The town from what the store already has: committees and this
/// machine's sessions (the sidebar's tree), the roster's members (with
/// their runtime, session state and status title, on any machine), the
/// threads waiting on the person, and the Speaker's desk.
extension TownState {
    init(store: CoreStore) {
        let state = store.state
        let secretaryLine = store.secretarySession
        let secretaryId = secretaryLine.map { TownText.senatorId($0.mention) }
        let rosters = Dictionary(store.roster.map { ($0.committee, $0) }, uniquingKeysWith: { a, _ in a })
        var committees: [TownCommittee] = []
        for group in state.groups {
            guard let name = group.committee, group.active || store.showInactive else { continue }
            let roster = rosters[name]
            let titles = Dictionary(
                (roster?.members ?? []).compactMap { m in m.statusTitle.map { (TownText.senatorId(m.senatorId), $0) } },
                uniquingKeysWith: { a, _ in a }
            )
            var senators: [TownSenator] = group.sessions
                .filter { store.showInactive || $0.active }
                .map(TownSenator.init(line:))
                .filter { $0.id != secretaryId }
                .map { s in
                    var s = s
                    if let title = titles[s.id] { s.doing = title }
                    return s
                }
            // Members on other machines, as the roster tells their state.
            for member in roster?.members ?? [] {
                let id = TownText.senatorId(member.senatorId)
                // An older daemon says only whether it is active.
                let mood = member.sessionState == nil ? (member.active ? .idle : .stopped)
                    : TownMood(session: member.sessionState)
                guard member.active || mood != .stopped || store.showInactive,
                      id != secretaryId, !senators.contains(where: { $0.id == id }) else { continue }
                senators.append(TownSenator(
                    id: id, name: member.displayName,
                    runtime: member.runtime.map { TownRuntime($0) } ?? TownRuntime(guessing: id),
                    mood: mood, chair: roster?.chairId.map(TownText.senatorId) == id,
                    sessionId: member.sessionId, doing: member.statusTitle
                ))
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
        self.init(committees: committees, hall: hall, person: state.person)
    }
}

/// The committees' activity, live: what is said and settled anywhere in
/// the Senate, for the town to act out. It starts at each committee's
/// head (nothing old is replayed), keeps the last seq it saw of each, and
/// after a dropped stream picks up from there; after a reset it starts at
/// the head again.
@MainActor
final class TownActivityFeed {
    private weak var store: CoreStore?
    private let deliver: ([TownEvent]) -> Void
    private var watch: UInt64?
    private var cursors: [String: UInt64] = [:]
    private var retry: Task<Void, Never>?
    private var running = false

    /// The entries as they came, for a town that maps them itself (the web Town).
    private let entries: (([TownActivityEntry]) -> Void)?

    init(store: CoreStore, entries: (([TownActivityEntry]) -> Void)? = nil, deliver: @escaping ([TownEvent]) -> Void) {
        self.store = store
        self.entries = entries
        self.deliver = deliver
    }

    func start() {
        guard !running else { return }
        running = true
        open()
    }

    func stop() {
        running = false
        retry?.cancel()
        retry = nil
        if let watch, let core = store?.core { core.stopActivity(id: watch) }
        watch = nil
    }

    private func open() {
        guard running, let core = store?.core else { return again() }
        let listener = Listener(
            batch: { [weak self] batch in Task { @MainActor in self?.receive(batch) } },
            end: { [weak self] error in Task { @MainActor in self?.ended(error) } }
        )
        do {
            watch = try core.watchActivity(committees: nil, after: cursors, listener: listener)
        } catch {
            Log.app.error("town: the activity watch could not start: \(String(describing: error), privacy: .public)")
            again()
        }
    }

    private func receive(_ batch: ActivityBatch) {
        guard running else { return }
        if batch.reset {
            cursors = [:]
            return
        }
        cursors.merge(batch.cursors) { max($0, $1) }
        for entry in batch.events { cursors[entry.committee] = max(cursors[entry.committee] ?? 0, entry.seq) }
        let mapped = batch.events.map(TownActivityEntry.init)
        if !mapped.isEmpty { entries?(mapped) }
        let events = TownActivity.events(mapped)
        if !events.isEmpty { deliver(events) }
    }

    private func ended(_ error: String?) {
        watch = nil
        if let error { Log.app.info("town: the activity watch ended: \(error, privacy: .public)") }
        again()
    }

    /// Try again in a few seconds, while the page is open.
    private func again() {
        guard running else { return }
        retry?.cancel()
        retry = Task { [weak self] in
            try? await Task.sleep(for: .seconds(3))
            guard !Task.isCancelled else { return }
            self?.open()
        }
    }

    /// The core calls it on its own threads.
    private final class Listener: ActivityListener, @unchecked Sendable {
        let batch: @Sendable (ActivityBatch) -> Void
        let end: @Sendable (String?) -> Void

        init(batch: @escaping @Sendable (ActivityBatch) -> Void, end: @escaping @Sendable (String?) -> Void) {
            self.batch = batch
            self.end = end
        }

        func onBatch(batch: ActivityBatch) { self.batch(batch) }
        func onEnd(error: String?) { end(error) }
    }
}

extension TownActivityEntry {
    init(_ entry: ActivityEntry) {
        self.init(
            committee: entry.committee, seq: entry.seq, kind: entry.kind, from: entry.from, to: entry.to,
            onBehalfOf: entry.onBehalfOf, thread: entry.thread, line: entry.line, state: entry.state
        )
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

extension TownMood {
    /// A roster member's session state: `working`, `idle`, `paused`,
    /// `stopped`, `away` (not connected) or `unknown` (connected, activity
    /// not known: shown idle).
    init(session: String?) {
        switch session {
        case "working": self = .working
        case "paused": self = .paused
        case "stopped", "away": self = .stopped
        case "waiting", "needs_you": self = .waiting
        default: self = .idle
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
