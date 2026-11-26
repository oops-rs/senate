import Foundation
import Observation
import SenateCore

/// The open session's conversation: the items read so far (oldest first,
/// one per seq), whether more lie before them, and whether the runtime is
/// at work. Read as a tail, paged back on demand, and kept current by the
/// core's watch while the page is open.
@Observable
final class SessionTimeline {
    let sessionId: String
    private(set) var items: [TimelineEntry] = [] {
        didSet { rows = ConversationRows.rows(items) }
    }
    /// The session's committee: its mentions are named for it.
    var committee: String?
    /// What the page draws: the items, reasoning folded into the rows it
    /// led to.
    private(set) var rows: [ConversationRow] = []
    var available = true
    /// Why there is nothing to read, when there is not.
    var unavailable: String?
    var hasOlder = false
    var working = false
    /// The first read has not come back yet.
    var loading = true
    var loadingOlder = false
    var error: String?

    init(sessionId: String) {
        self.sessionId = sessionId
    }

    var lastSeq: UInt64? { items.last?.seq }
    var firstSeq: UInt64? { items.first?.seq }

    func replace(with page: TimelinePage) {
        items = page.items
        available = page.available
        unavailable = page.unavailable
        hasOlder = page.hasOlder
        working = page.working
        loading = false
        error = nil
    }

    func prepend(_ page: TimelinePage) {
        let known = Set(items.map(\.seq))
        items = page.items.filter { !known.contains($0.seq) } + items
        hasOlder = page.hasOlder
    }

    /// Merge what a watch said: an item with a known seq is replaced in
    /// place (a tool call completing), a new one is appended in order.
    func merge(_ delta: TimelineDelta) {
        working = delta.working
        for item in delta.items {
            if let i = items.lastIndex(where: { $0.seq == item.seq }) {
                items[i] = item
            } else if let last = items.last, item.seq < last.seq {
                let at = items.firstIndex { $0.seq > item.seq } ?? items.endIndex
                items.insert(item, at: at)
            } else {
                items.append(item)
            }
        }
    }
}

/// What the core's timeline watch says, in order, on the main actor.
enum TimelineEvent: Sendable {
    case delta(TimelineDelta)
    case end(String?)
}

/// The core's timeline listener: it only yields into a stream the store
/// reads on the main actor.
nonisolated final class TimelineEvents: TimelineListener {
    private let continuation: AsyncStream<TimelineEvent>.Continuation

    init(_ continuation: AsyncStream<TimelineEvent>.Continuation) {
        self.continuation = continuation
    }

    func onDelta(delta: TimelineDelta) { continuation.yield(.delta(delta)) }
    func onEnd(error: String?) {
        continuation.yield(.end(error))
        continuation.finish()
    }
}

extension CoreStore {
    /// How many items a page reads.
    static let timelinePage: UInt32 = 60

    /// The session id whose conversation `place` shows, when any.
    private func timelineSessionId(for place: Place) -> String? {
        switch place {
        case let .session(id), let .past(id): id
        case .secretary:
            secretary?.state == .running && !secretaryStarting ? secretarySession?.sessionId : nil
        default: nil
        }
    }

    /// Read and follow the open session's conversation; let go of one no
    /// longer open. Called whenever the place changes.
    func syncTimeline() {
        let open = timelineSessionId(for: place)
        guard timeline?.sessionId != open else { return }
        if let old = timeline {
            core?.stopTimeline(sessionId: old.sessionId)
            // Still on the window's stack: kept, to come back to as it was
            // (older pages read included), no longer followed meanwhile.
            parked[old.sessionId] = old
        }
        // Only what the stack can go back to.
        let onStack = Set(([root] + path).compactMap(timelineSessionId(for:)))
        parked = parked.filter { onStack.contains($0.key) }
        guard let open else {
            timeline = nil
            return
        }
        if let back = parked.removeValue(forKey: open) {
            timeline = back
            // Followed again from where it was left.
            if back.available { follow(back) } else { Task { await readTail(back) } }
            return
        }
        let t = SessionTimeline(sessionId: open)
        t.committee = session(open)?.committee ?? history.row(open)?.committee
        timeline = t
        Task { await readTail(t) }
    }

    /// Read the session's tail again and follow it from there.
    func readTail(_ t: SessionTimeline) async {
        guard let core else { return }
        do {
            let page = try await core.timeline(sessionId: t.sessionId, before: nil, limit: Self.timelinePage)
            guard timeline === t else { return }
            t.replace(with: page)
            if page.available { follow(t) }
        } catch {
            guard timeline === t else { return }
            t.loading = false
            t.error = describe(error)
        }
    }

    /// The page before the first item read.
    func loadOlder() async {
        guard let core, let t = timeline, t.hasOlder, !t.loadingOlder, let first = t.firstSeq else { return }
        t.loadingOlder = true
        defer { t.loadingOlder = false }
        do {
            let page = try await core.timeline(sessionId: t.sessionId, before: first, limit: Self.timelinePage)
            guard timeline === t else { return }
            t.prepend(page)
        } catch {
            t.error = describe(error)
        }
    }

    private func follow(_ t: SessionTimeline) {
        guard let core else { return }
        let (stream, continuation) = AsyncStream.makeStream(of: TimelineEvent.self)
        do {
            try core.watchTimeline(sessionId: t.sessionId, after: t.lastSeq ?? 0, listener: TimelineEvents(continuation))
        } catch {
            t.error = describe(error)
            return
        }
        Task { [weak self] in
            for await event in stream {
                guard let self, self.timeline === t else { return }
                switch event {
                case let .delta(delta):
                    if delta.reset {
                        await self.readTail(t)
                        return
                    }
                    let start = ContinuousClock.now
                    t.merge(delta)
                    Perf.Probe.updates += 1
                    Perf.Probe.updateTime += ContinuousClock.now - start
                case let .end(error):
                    // The daemon let go (it restarted, or the session ended):
                    // read again in a moment and follow anew.
                    if let error { Log.app.notice("timeline \(t.sessionId, privacy: .public): \(error, privacy: .public)") }
                    try? await Task.sleep(for: .seconds(2))
                    guard self.timeline === t else { return }
                    await self.readTail(t)
                }
            }
        }
    }

    // MARK: The sidebar's tree

    /// The committees the sidebar lists: active ones (or all, with *Show
    /// inactive*), each with its sessions shown under the same rule and
    /// narrowed by the search.
    var shownGroups: [SessionGroup] {
        let q = query
        return state.groups.compactMap { group in
            var g = group
            g.sessions = group.sessions.filter { s in
                (showInactive || s.active)
                    && (q.isEmpty || [s.displayName, s.title ?? "", s.runtime, s.dir, s.branch ?? "", group.title]
                        .contains { $0.localizedStandardContains(q) })
            }
            let matches = q.isEmpty || group.title.localizedStandardContains(q)
            if !q.isEmpty { return g.sessions.isEmpty && !matches ? nil : g }
            // A committee with nothing here and nothing waiting is not shown
            // unless inactive ones are.
            if !showInactive && !group.active && g.sessions.isEmpty { return nil }
            if g.sessions.isEmpty && group.committee == nil { return nil }
            return g
        }
    }

    /// Every session the sidebar shows, top to bottom (⌘⌥↑ and ⌘⌥↓).
    var shownSessions: [SessionLine] {
        shownGroups.flatMap(\.sessions)
    }

    /// The session above (`-1`) or below (`+1`) the open one in the sidebar;
    /// from anywhere else, the first or the last.
    func moveSession(_ step: Int) {
        let all = shownSessions
        guard !all.isEmpty else { return }
        if case let .session(id) = place, let i = all.firstIndex(where: { $0.sessionId == id }) {
            let j = min(max(i + step, 0), all.count - 1)
            place = .session(all[j].sessionId)
        } else {
            place = .session((step > 0 ? all.first : all.last)!.sessionId)
        }
        if let key = all.first(where: { if case let .session(id) = place { $0.sessionId == id } else { false } })?.committee {
            folded.remove(key)
        }
    }

    /// The hosted session behind a prompt, as the sidebar knows it.
    func line(for hosted: HostedSession) -> SessionLine? {
        session(hosted.sessionId)
    }
}

extension SessionGroup: @retroactive Identifiable {
    /// The committee, or "" for the sessions of no committee.
    public var id: String { committee ?? "" }
}

extension SessionLine: @retroactive Identifiable {
    public var id: String { sessionId }
}

extension TimelineEntry: @retroactive Identifiable {
    public var id: UInt64 { seq }
}

extension CoreStore {
    /// Open the session a search found, scrolled to the item that matched
    /// with the words highlighted: a live session on its page, an ended one
    /// in History. Older pages are read until the item is there.
    func open(_ hit: SearchHit, query: String) {
        reveal = Reveal(sessionId: hit.sessionId, seq: hit.seq, query: query)
        place = hit.live && session(hit.sessionId) != nil ? .session(hit.sessionId) : .past(hit.sessionId)
        Task { await revealWhenRead() }
    }

    private func revealWhenRead() async {
        guard let r = reveal else { return }
        defer { if reveal == r { reveal = nil } }
        if history.row(r.sessionId) == nil, case .past = place { await history.find(r.sessionId) }
        // Its timeline, read.
        for _ in 0..<200 {
            if let t = timeline, t.sessionId == r.sessionId, !t.loading { break }
            try? await Task.sleep(for: .milliseconds(25))
        }
        guard let t = timeline, t.sessionId == r.sessionId, reveal == r else { return }
        // Older pages until the item is among them.
        var pages = 0
        while let first = t.firstSeq, first > r.seq, t.hasOlder, pages < 200, reveal == r {
            await loadOlder()
            pages += 1
        }
        // Its row: its own, a command's inside a group, or (for reasoning,
        // which has none) the row it folded into.
        let row = ConversationRows.rowId(for: r.seq, in: t.rows) ?? r.seq
        await page.reveal(row: row, query: r.query)
    }
}
