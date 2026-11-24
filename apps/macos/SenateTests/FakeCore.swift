import Foundation
@testable import Senate
import SenateCore

/// A core that talks to no daemon: it records what the store asks of it
/// and lets a test push states the way the Rust core would.
nonisolated final class FakeCore: AppCoreProtocol, @unchecked Sendable {
    // The store calls on the main actor; the tests read after awaiting.
    private let lock = NSLock()
    private var _calls: [String] = []
    private var listener: CoreListener?
    var failNext: CoreError?
    var threadView: ThreadView?
    /// Held until released: the action in flight.
    var gate: CheckedContinuation<Void, Never>?
    var holdActions = false

    var calls: [String] {
        lock.withLock { _calls }
    }

    private func record(_ call: String) throws {
        lock.withLock { _calls.append(call) }
        if let error = failNext {
            failNext = nil
            throw error
        }
    }

    /// As the core does after every change.
    func push(_ state: AppState) {
        listener?.onState(state: state)
    }

    func pushNote(_ note: PersonNote) {
        listener?.onForPerson(note: note)
    }

    private func hold() async {
        guard holdActions else { return }
        await withCheckedContinuation { gate = $0 }
    }

    func release() {
        gate?.resume()
        gate = nil
    }

    // MARK: AppCoreProtocol

    func deskItem(committee: String, seq: UInt64) async throws -> DeskItemView {
        try record("desk-item \(committee) \(seq)")
        let item = ThreadEvent(seq: seq, kind: "request", kindLabel: "Request", by: "tester",
                               to: ["speaker"], body: "Full request", at: Date(), mine: false, status: nil)
        let thread = ThreadView(committee: committee, root: seq, title: "Full request",
                                state: nil, stateLabel: nil, kindLabel: "Request", events: [item])
        return DeskItemView(eventId: "source-item", item: item, thread: thread, origin: nil,
                            destination: "payments::senator::ledger", destinationLabel: "ledger in payments", blocked: nil)
    }
    func newDeskActionId() -> String { "desk-test-action" }
    func decideDeskItem(committee: String, seq: UInt64, itemId: String, id: String, reason: String?) async throws -> DeskOutcomeView {
        try record("desk-decide \(committee) \(seq) \(id) \(reason ?? "forward")")
        return DeskOutcomeView(delivered: reason == nil, destination: "payments::senator::ledger",
                               destinationLabel: "ledger in payments", committee: "payments", seq: 75)
    }

    func approve(id: String, decision: ApprovalDecision) async throws {
        try record("approve \(id) \(decision)")
    }
    func approveSession(label: String, decision: ApprovalDecision) async throws {
        try record("approve-session \(label) \(decision)")
    }
    func resumeSession(label: String) async throws {
        try record("resume \(label)")
    }
    func attachTerminal(label: String) throws -> TerminalCommand {
        try record("attach \(label)")
        return TerminalCommand(argv: ["senate", "attach", label], cwd: nil, shellLine: "", appleScript: "")
    }
    func attachTerminalForSession(sessionId: String) -> TerminalCommand {
        try? record("attach-session \(sessionId)")
        return TerminalCommand(argv: ["senate", "attach", "--session=\(sessionId)"],
                               cwd: nil, shellLine: "", appleScript: "")
    }
    func close(committee: String, thread: UInt64) async throws {
        try record("close \(committee) \(thread)")
    }
    func codexTerminal(threadId: String?) throws -> TerminalCommand {
        try record("codex \(threadId ?? "new")")
        return TerminalCommand(argv: [], cwd: nil, shellLine: "", appleScript: "")
    }
    func markRead(committee: String, thread: UInt64, seen: UInt64) async throws {
        try record("read \(committee) \(thread) \(seen)")
    }
    func reconnect() throws { try record("reconnect") }
    func refresh() throws { try record("refresh") }
    /// The archived committees; Unarchive takes one off.
    var archived: [String] = []
    func stats(from: String, to: String, tz: String) async throws -> StatsReport {
        try record("stats \(from) \(to) \(tz)")
        var hours = Array(repeating: UInt64(0), count: 24)
        hours[14] = 15
        return StatsReport(
            days: [StatsDay(
                date: to, sessions: 2, sessionsStarted: 1, activeSecs: 2340, prompts: 5,
                agentTurns: 6, toolCalls: 12, commands: 4, edits: 5, mcpCalls: 2,
                messages: 5, threadsOpened: 2, threadsClosed: 1, threadsTaken: 0,
                threadsAnswered: 1, approvalsDecided: 1
            )],
            hours: hours, currentStreak: 1, longestStreak: 1,
            topCommittees: [StatsRank(name: "refund-v2", count: 2340)],
            topRepos: [StatsRank(name: "refund-service", count: 2340)],
            indexing: false, indexed: nil, total: nil,
            notes: ["Fixture uses recorded activity."]
        )
    }
    func archivedCommittees() async throws -> [String] {
        try record("archived")
        return lock.withLock { archived }
    }
    func sessionDrop(sessionId: String, name: String, bytes: Data) async throws -> String {
        try record("drop \(sessionId) \(name) \(bytes.count)")
        return "/tmp/senate-fake/drops/fixture/\(name)"
    }
    func unarchiveCommittee(committee: String) async throws {
        try record("unarchive:\(committee)")
        lock.withLock { archived.removeAll { $0 == committee } }
    }
    func reply(committee: String, thread: UInt64, body: String) async throws {
        await hold()
        try record("reply \(committee) \(thread) \(body)")
    }
    func restoreSession(label: String) async throws { try record("restore \(label)") }
    /// History: 12 ended sessions, three committees, newest first.
    var historyRows: [HistoryRow] = (0..<12).map(FakeCore.pastRow)
    static func pastRow(_ i: Int) -> HistoryRow {
        let started = Date(timeIntervalSinceNow: Double(-3600 * (i + 2)))
        let last = Date(timeIntervalSinceNow: Double(-3600 * (i + 1)))
        let runtime = i % 2 == 0 ? "claude" : "codex"
        return HistoryRow(sessionId: "past-\(i)", name: "past-\(i)", label: "c\(i % 3)::past-\(i)",
                          committee: "c\(i % 3)", runtime: runtime, model: nil, dir: "/tmp", branch: nil,
                          title: "Title \(i)", startedAt: started, lastActivity: last,
                          readable: i != 5, restorable: i != 5, itemCount: 4)
    }
    /// Searches answered as partial (indexing) this many times first.
    var indexingFor = 0
    func history(committee: String?, before: String?, limit: UInt32) async throws -> HistoryPage {
        try record("history \(committee ?? "-") \(before ?? "-")")
        let rows = historyRows.filter { committee == nil || $0.committee == committee }
        let from = Int(before ?? "0") ?? 0
        let page = Array(rows.dropFirst(from).prefix(Int(limit)))
        return HistoryPage(rows: page, next: from + page.count < rows.count ? String(from + page.count) : nil)
    }
    func searchSessions(query: String, committee: String?, cursor: String?, limit: UInt32) async throws -> SearchPage {
        try record("search \(query)")
        let hits = [SearchHit(sessionId: "past-3", seq: 2, kind: .reply, snippet: "about \(query) here", at: Date(), live: false)]
        if indexingFor > 0 {
            indexingFor -= 1
            return SearchPage(hits: [], next: nil, indexing: true, indexed: 3, total: 10)
        }
        return SearchPage(hits: hits, next: nil, indexing: false, indexed: nil, total: nil)
    }
    func forgetSession(sessionId: String) async throws {
        try record("forget \(sessionId)")
        historyRows.removeAll { $0.sessionId == sessionId }
    }
    var secretary = SecretaryView(committee: "office", alias: "secretary", senator: nil, state: .notRecorded, action: nil)
    func defaultSecretary() async throws -> SecretaryView { secretary }
    func ensureSecretary() async throws -> SecretaryView {
        try record("ensure secretary")
        secretary.state = .running
        secretary.senator = "claude-secretary"
        secretary.action = "started"
        return secretary
    }
    func senateHome() -> String { "/tmp/fake-senate" }
    func setLanguage(language: String?) {}
    func snapshot() -> AppState { .placeholder }
    func start(listener: CoreListener) throws {
        try record("start")
        self.listener = listener
    }
    func stopSession(label: String) async throws { try record("stop \(label)") }
    func take(committee: String, thread: UInt64) async throws {
        try record("take \(committee) \(thread)")
    }
    func thread(committee: String, thread: UInt64) async throws -> ThreadView {
        try record("thread \(committee) \(thread)")
        return threadView ?? ThreadView(committee: committee, root: thread, title: "", state: nil, stateLabel: nil, kindLabel: "", events: [])
    }
    var committeeThreadRows: [CommitteeThread] = []
    private var activityListeners: [UInt64: ActivityListener] = [:]
    private var nextActivityWatch: UInt64 = 1

    func committeeThreads(committee: String, includeArchived: Bool, limit: UInt32?, cursor: String?) async throws -> CommitteeThreadPage {
        try record("committee-threads \(committee)")
        let before = cursor.flatMap(UInt64.init) ?? UInt64.max
        let rows = committeeThreadRows.filter {
            $0.committee == committee && $0.lastSeq < before && (includeArchived || $0.state != "archived")
        }.sorted { $0.lastSeq > $1.lastSeq }
        let page = Array(rows.prefix(max(1, Int(limit ?? 40))))
        return CommitteeThreadPage(
            rows: page, nextCursor: rows.count > page.count ? page.last.map { String($0.lastSeq) } : nil)
    }

    func watchActivity(committees: [String]?, after: [String: UInt64], listener: ActivityListener) throws -> UInt64 {
        try record("watch-activity")
        return lock.withLock {
            let id = nextActivityWatch
            nextActivityWatch += 1
            activityListeners[id] = listener
            return id
        }
    }

    func stopActivity(id: UInt64) {
        lock.withLock {
            _calls.append("unwatch-activity \(id)")
            activityListeners.removeValue(forKey: id)
        }
    }

    func pushActivity(_ batch: ActivityBatch) {
        let listeners = lock.withLock { Array(activityListeners.values) }
        for listener in listeners {
            listener.onBatch(batch: batch)
        }
    }

    /// What `timeline` returns, by session id.
    var timelines: [String: TimelinePage] = [:]
    /// The listener of the session watched last.
    var timelineListener: TimelineListener?

    func timeline(sessionId: String, before: UInt64?, limit: UInt32?) async throws -> TimelinePage {
        try record("timeline \(sessionId) \(before.map(String.init) ?? "tail")")
        return timelines[sessionId] ?? TimelinePage(
            sessionId: sessionId, available: false, unavailable: "none", items: [], hasOlder: false,
            firstSeq: nil, lastSeq: nil, working: false)
    }
    func watchTimeline(sessionId: String, after: UInt64, listener: TimelineListener) throws {
        try record("watch \(sessionId) \(after)")
        timelineListener = listener
    }
    func stopTimeline(sessionId: String) {
        lock.withLock { _calls.append("unwatch \(sessionId)") }
    }
    /// What `say` answers: the log's committee and seq for the note.
    var nextSent = SentNote(committee: "refund-v2", seq: 300)
    func say(sessionId: String, body: String) async throws -> SentNote {
        try record("say \(sessionId) \(body)")
        return nextSent
    }
    func pauseSession(label: String) async throws { try record("pause \(label)") }

    func watchTerminal() -> TerminalCommand {
        TerminalCommand(argv: [], cwd: nil, shellLine: "", appleScript: "")
    }
}

/// A thread waiting on alice (`forPerson`), or only on her agents.
func thread(
    _ root: UInt64,
    _ title: String,
    committee: String = "refund-v2",
    for agents: [String] = ["claude"],
    escalated: Bool = false,
    news: String? = nil,
    forPerson: Bool = true,
    toDecide: Bool? = nil
) -> InboxThread {
    InboxThread(
        committee: committee, root: root, updated: root, title: title,
        kind: "question", kindLabel: "question", state: "suspended", stateLabel: "suspended",
        from: "[senator::claude]", onBehalfOf: nil, to: agents.map { "[senator::\($0)]" },
        since: .now, owner: nil, triage: nil, triageLabel: nil, reason: nil, escalatedTo: nil,
        takeover: nil, answered: false, forPerson: forPerson, asker: "[senator::claude]", forAgents: agents, escalated: escalated, toDecide: toDecide ?? (forPerson && escalated), unread: news != nil,
        newsFrom: news.map { _ in "[senator::claude]" }, newsKind: nil, newsKindLabel: nil,
        newsLine: news
    )
}

func agent(_ path: String, pending: UInt32 = 0, active: Bool = true) -> AgentInfo {
    AgentInfo(
        displayName: path, active: active, path: path, mention: "[senator::\(path)]", senate: "", runtime: "claude",
        sessions: 1, pending: pending, presiding: false, inbox: true
    )
}

/// Online as alice, with these threads and agents.
func online(_ inbox: [InboxThread] = [], agents: [AgentInfo] = [], docket: Bool = false) -> AppState {
    var s = AppState.placeholder
    s.online = true
    s.person = "alice"
    s.inbox = inbox
    s.agents = agents
    s.counts.threads = UInt32(inbox.count)
    s.menu.docket = docket ? "Docket: 1 awaiting the Speaker" : nil
    return s
}

/// A hosted terminal with a permission prompt that has waited `waited` seconds.
func prompting(_ name: String, tool: String = "cargo publish", waited: TimeInterval = 60) -> HostedSession {
    HostedSession(
        displayName: name, active: true, label: "refund-v2::\(name)", sessionId: "s-\(name)", mention: "[senator::\(name)]",
        committee: "refund-v2", runtime: "claude", model: nil, dir: "/work",
        running: true, startedAt: nil, attached: false, typedSecsAgo: nil,
        promptTool: tool, promptSince: Date.now.addingTimeInterval(-waited), promptAnswerable: true,
        paused: false)
}

/// A Codex approval request received `waited` seconds ago.
func approval(_ id: String, _ summary: String, command: String? = nil, waited: TimeInterval = 30) -> ApprovalRequest {
    ApprovalRequest(
        id: id, thread: "019a-thread", kind: "command", summary: summary, command: command,
        cwd: "/work", reason: nil, files: [], receivedAt: Date.now.addingTimeInterval(-waited))
}

/// An item put to the Speaker on `committee`.
func docketEntry(_ seq: UInt64, _ body: String, committee: String = "payments") -> DocketEntry {
    DocketEntry(
        committee: committee, seq: seq, kindLabel: "question", by: "[senator::auditor]", body: body,
        awaiting: "addressed", why: "addressed to the Speaker", agents: [], since: .now)
}

/// The docket of the one Senate, holding these items.
func docket(_ entries: [DocketEntry]) -> [DocketSenate] {
    let committees = Dictionary(grouping: entries, by: \.committee).map {
        DocketCommittee(committee: $0.key, items: $0.value, seats: [])
    }
    return [DocketSenate(senate: "alice-desk", title: "alice-desk", speaker: nil, committees: committees)]
}

/// The one Senate, with this roster and these chairs.
func senate(roster: [RosterCommittee], chairs: [ChairLine] = []) -> SenateInfo {
    SenateInfo(
        key: "alice-desk", title: "alice-desk", senate: "alice-desk", primary: true,
        serverUrl: "http://127.0.0.1:7001", me: "[senator::alice]", speaker: nil, speakerIsYou: false, presidingAgent: nil,
        committees: [], roster: roster, chairs: chairs, children: [], uplink: nil)
}

func committee(_ name: String, chair: String? = nil, live: Bool = false, members: [(String, Bool)] = []) -> RosterCommittee {
    RosterCommittee(
        committee: name, chairId: chair, chairDisplayName: chair, chairLive: live,
        activeMembers: UInt32(members.filter(\.1).count), pending: 0,
        members: members.map { RosterMember(senatorId: $0.0, displayName: $0.0, active: $0.1,
                                               runtime: nil, sessionState: $0.1 ? "idle" : "away",
                                               statusTitle: nil, sessionId: nil) })
}

/// A session on this machine; hosted (with a prompt) when `prompt` is set.
func sessionLine(
    _ name: String, committee: String? = "refund-v2", status: SessionStatus = .idle,
    active: Bool = true, hosted: HostedSession? = nil
) -> SessionLine {
    SessionLine(
        sessionId: hosted?.sessionId ?? "s-\(name)", displayName: name, mention: "[senator::\(name)]",
        committee: committee, chair: false, runtime: "claude", model: nil, dir: "/work", branch: nil,
        title: nil, status: status, statusLabel: "\(status)", working: status == .working, unread: 0,
        lastActivity: .now, active: active, hosted: hosted, readable: true)
}

/// The sidebar's tree: these sessions under their committees.
func groups(_ sessions: [SessionLine]) -> [SessionGroup] {
    Dictionary(grouping: sessions, by: \.committee).map { committee, lines in
        SessionGroup(
            committee: committee, title: committee ?? "No committee", waiting: 0,
            status: lines.map(\.status).min { $0.rank < $1.rank }, active: lines.contains(where: \.active),
            sessions: lines)
    }
    .sorted { ($0.committee ?? "~") < ($1.committee ?? "~") }
}

extension SessionStatus {
    var rank: Int {
        switch self {
        case .needsYou: 0
        case .working: 1
        case .unread: 2
        case .idle: 3
        case .paused: 4
        case .stopped: 5
        }
    }
}

/// A timeline entry: what the person said, or the agent's reply.
func entry(_ seq: UInt64, _ text: String, user: Bool = false) -> TimelineEntry {
    TimelineEntry(seq: seq, at: nil, truncated: false, body: user ? .user(text: text) : .assistant(text: text))
}
