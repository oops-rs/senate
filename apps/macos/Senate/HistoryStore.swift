import Foundation
import SenateCore

/// History: the machine's ended and stopped sessions, newest first, read a
/// page at a time; a search across past and live conversations; and what
/// can be done with one (read it, restore it, forget it).
@MainActor
@Observable
final class HistoryModel {
    /// What has been read so far, newest first.
    private(set) var rows: [HistoryRow] = []
    private(set) var next: String?
    private(set) var loading = false
    private(set) var loadedOnce = false
    private(set) var error: String?
    /// Only this committee (`""`: sessions on none); nil: all.
    var committee: String? { didSet { if committee != oldValue { Task { await reload() } } } }
    /// Only this runtime; nil: all.
    var runtime: String?

    /// The search, and what it found.
    var query = "" { didSet { if query != oldValue { scheduleSearch() } } }
    private(set) var hits: [SearchHit] = []
    private(set) var searching = false
    /// The daemon is still indexing: hits are partial; `indexed` of `total`.
    private(set) var indexing: (indexed: UInt64?, total: UInt64?)?
    private var searchNext: String?
    @ObservationIgnored private var searchTask: Task<Void, Never>?

    @ObservationIgnored weak var store: CoreStore?
    static let page: UInt32 = 40

    /// Rows as shown: the runtime filter applied.
    var shown: [HistoryRow] {
        guard let runtime else { return rows }
        return rows.filter { $0.runtime == runtime }
    }

    func row(_ sessionId: String) -> HistoryRow? { rows.first { $0.sessionId == sessionId } }

    // MARK: Reading

    func reload() async {
        rows = []
        next = nil
        loadedOnce = false
        await loadMore()
    }

    func loadMore() async {
        guard let core = store?.core, !loading, !loadedOnce || next != nil else { return }
        loading = true
        defer { loading = false }
        do {
            let page = try await core.history(committee: committee, before: loadedOnce ? next : nil, limit: Self.page)
            rows.append(contentsOf: page.rows.filter { row in !rows.contains { $0.sessionId == row.sessionId } })
            next = page.next
            loadedOnce = true
            error = nil
        } catch {
            self.error = describe(error)
        }
    }

    /// Read on until this session is among the rows (it was found by a
    /// search and is further back than what has been read).
    func find(_ sessionId: String, pages limit: Int = 50) async {
        var pages = 0
        while row(sessionId) == nil, next != nil || !loadedOnce, pages < limit {
            await loadMore()
            pages += 1
        }
    }

    // MARK: Search

    private func scheduleSearch() {
        searchTask?.cancel()
        let q = query.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !q.isEmpty else {
            hits = []
            searching = false
            indexing = nil
            return
        }
        searchTask = Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(250))
            guard !Task.isCancelled else { return }
            await self?.search(q)
        }
    }

    private func search(_ q: String) async {
        guard let core = store?.core else { return }
        searching = true
        defer { searching = false }
        // While the daemon indexes, the hits are partial: ask again.
        for _ in 0..<120 {
            guard !Task.isCancelled, q == query.trimmingCharacters(in: .whitespacesAndNewlines) else { return }
            do {
                let page = try await core.searchSessions(query: q, committee: committee, cursor: nil, limit: 50)
                hits = page.hits
                searchNext = page.next
                indexing = page.indexing ? (page.indexed, page.total) : nil
                error = nil
                // Names for what was found further back than read so far.
                for hit in page.hits where !hit.live && row(hit.sessionId) == nil {
                    await find(hit.sessionId)
                }
                guard page.indexing else { return }
            } catch {
                self.error = describe(error)
                return
            }
            try? await Task.sleep(for: .seconds(1.5))
        }
    }

    func searchMore() async {
        guard let core = store?.core, let cursor = searchNext, !searching else { return }
        let q = query.trimmingCharacters(in: .whitespacesAndNewlines)
        searching = true
        defer { searching = false }
        if let page = try? await core.searchSessions(query: q, committee: committee, cursor: cursor, limit: 50) {
            hits.append(contentsOf: page.hits)
            searchNext = page.next
        }
    }

    var canSearchMore: Bool { searchNext != nil }

    // MARK: Acting on one

    /// Forget its record: it leaves History; the agent's own transcript
    /// stays where its runtime keeps it.
    func forget(_ row: HistoryRow) async -> Bool {
        guard let store else { return false }
        let ok = await store.run("forget:\(row.sessionId)") { core in
            try await core.forgetSession(sessionId: row.sessionId)
        }
        if ok { rows.removeAll { $0.sessionId == row.sessionId } }
        return ok
    }

    /// Restore it (the session's own restore): once it runs again it leaves
    /// History and opens as a live session.
    func restore(_ row: HistoryRow) async {
        guard let store else { return }
        let ok = await store.run("session:\(row.label)") { core in
            try await core.restoreSession(label: row.label)
        }
        guard ok else { return }
        rows.removeAll { $0.sessionId == row.sessionId }
        for _ in 0..<150 where store.session(row.sessionId) == nil {
            try? await Task.sleep(for: .milliseconds(100))
        }
        if store.session(row.sessionId) != nil { store.place = .session(row.sessionId) }
    }
}

/// A match to show once its session is open: which item, and the words.
struct Reveal: Equatable {
    let sessionId: String
    let seq: UInt64
    let query: String
}
