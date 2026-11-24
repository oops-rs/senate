import Foundation
import SenateCore

/// The committees' open threads, for the sidebar: the conversations
/// between senators (and with chairs, the Speaker, other committees), not
/// only what waits on the person. Read a page per committee, kept live
/// from the committees' activity, newest first. Which threads have news
/// since the person last opened them is remembered on this Mac.
@MainActor
@Observable
final class ThreadsModel {
    /// Each committee's threads as read, newest activity first.
    private(set) var rows: [String: [CommitteeThread]] = [:]
    /// Committees with more threads than read.
    private(set) var more: Set<String> = []
    /// The last seq the person saw of each thread (`committee:root`).
    private(set) var seen: [String: UInt64] = [:]

    @ObservationIgnored weak var store: CoreStore?
    @ObservationIgnored private var watch: UInt64?
    @ObservationIgnored private var cursors: [String: UInt64] = [:]
    @ObservationIgnored private var pending: Set<String> = []
    @ObservationIgnored private var reload: Task<Void, Never>?
    @ObservationIgnored private var archived = false
    @ObservationIgnored private let defaults: UserDefaults
    private static let seenKey = "seenThreads"
    /// Shown under a committee before "Show all".
    static let shown = 5
    static let page: UInt32 = 6

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        seen = (defaults.dictionary(forKey: Self.seenKey) as? [String: NSNumber])?
            .mapValues(\.uint64Value) ?? [:]
    }

    static func key(_ t: CommitteeThread) -> String { "\(t.committee):\(t.root)" }

    /// A committee's threads as the sidebar shows them, newest first. A
    /// thread between two committees is in both their logs, so each lists
    /// its side of it.
    func threads(_ committee: String) -> [CommitteeThread] {
        (rows[committee] ?? []).sorted { $0.lastSeq > $1.lastSeq }
    }

    /// Something was said since the person last opened it.
    func unread(_ t: CommitteeThread) -> Bool {
        guard let last = seen[Self.key(t)] else { return false }
        return t.lastSeq > last
    }

    /// Opened: what it says now is seen.
    func markSeen(_ key: String) {
        let latest = rows.values.lazy.flatMap { $0 }.first { Self.key($0) == key }?.lastSeq
        guard let latest, (seen[key] ?? 0) < latest else { return }
        seen[key] = latest
        save()
    }

    // MARK: Reading

    /// Read every committee's first page and follow their activity.
    func start(committees: [String], archived: Bool) async {
        guard let core = store?.core else { return }
        if archived != self.archived {
            self.archived = archived
            rows = [:]
        }
        for c in committees where rows[c] == nil { await read(c) }
        guard watch == nil else { return }
        watch = try? core.watchActivity(committees: nil, after: cursors, listener: ActivityEvents { [weak self] batch in
            Task { @MainActor in self?.heard(batch) }
        })
    }

    func stop() {
        if let watch { store?.core?.stopActivity(id: watch) }
        watch = nil
        reload?.cancel()
    }

    private func read(_ committee: String) async {
        guard let core = store?.core else { return }
        guard let page = try? await core.committeeThreads(
            committee: committee, includeArchived: archived, limit: Self.page, cursor: nil
        ) else { return }
        rows[committee] = page.rows
        if page.nextCursor != nil { more.insert(committee) } else { more.remove(committee) }
        // First seen: a baseline, so only what is said from now is news.
        var changed = false
        for t in page.rows where seen[Self.key(t)] == nil {
            seen[Self.key(t)] = t.lastSeq
            changed = true
        }
        if changed { save() }
    }

    /// The committees' activity: read again the committees it touched
    /// (all of them after a reset), a moment later, once.
    private func heard(_ batch: ActivityBatch) {
        for (c, seq) in batch.cursors { cursors[c] = max(cursors[c] ?? 0, seq) }
        for e in batch.events { cursors[e.committee] = max(cursors[e.committee] ?? 0, e.seq) }
        if batch.reset {
            cursors = [:]
            pending.formUnion(rows.keys)
        }
        pending.formUnion(batch.events.map(\.committee).filter { rows[$0] != nil })
        guard !pending.isEmpty, reload == nil else { return }
        reload = Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(300))
            guard let self, !Task.isCancelled else { return }
            let touched = self.pending
            self.pending = []
            self.reload = nil
            for c in touched { await self.read(c) }
        }
    }

    /// The rest of a committee's threads ("Show all").
    func all(_ committee: String) async -> [CommitteeThread] {
        guard let core = store?.core else { return [] }
        var out: [CommitteeThread] = []
        var cursor: String?
        repeat {
            guard let page = try? await core.committeeThreads(
                committee: committee, includeArchived: archived, limit: 100, cursor: cursor
            ) else { break }
            out += page.rows
            cursor = page.nextCursor
        } while cursor != nil && out.count < 1000
        return out
    }

    private func save() {
        defaults.set(seen.mapValues { NSNumber(value: $0) }, forKey: Self.seenKey)
    }
}

/// The core's activity callbacks, handed to the main actor.
private final class ActivityEvents: ActivityListener, @unchecked Sendable {
    let batch: (ActivityBatch) -> Void
    init(_ batch: @escaping (ActivityBatch) -> Void) { self.batch = batch }
    func onBatch(batch: ActivityBatch) { self.batch(batch) }
    func onEnd(error: String?) {}
}
