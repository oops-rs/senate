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
                               to: ["speaker"], body: "Full request", at: Date(), mine: false)
        let thread = ThreadView(committee: committee, root: seq, title: "Full request",
                                state: nil, stateLabel: nil, kindLabel: "Request", events: [item])
        return DeskItemView(eventId: "source-item", item: item, thread: thread, origin: nil,
                            destination: "payments::senator::ledger", blocked: nil)
    }
    func newDeskActionId() -> String { "desk-test-action" }
    func decideDeskItem(committee: String, seq: UInt64, itemId: String, id: String, reason: String?) async throws -> DeskOutcomeView {
        try record("desk-decide \(committee) \(seq) \(id) \(reason ?? "forward")")
        return DeskOutcomeView(delivered: reason == nil, destination: "payments::senator::ledger",
                               committee: "payments", seq: 75)
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
    func reply(committee: String, thread: UInt64, body: String) async throws {
        await hold()
        try record("reply \(committee) \(thread) \(body)")
    }
    func restoreSession(label: String) async throws { try record("restore \(label)") }
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
    func say(sessionId: String, body: String) async throws {
        try record("say \(sessionId) \(body)")
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
        serverUrl: "http://127.0.0.1:7001", me: "[senator::alice]", speaker: nil, presidingAgent: nil,
        committees: [], roster: roster, chairs: chairs, children: [], uplink: nil)
}

func committee(_ name: String, chair: String? = nil, live: Bool = false, members: [(String, Bool)] = []) -> RosterCommittee {
    RosterCommittee(
        committee: name, chairId: chair, chairDisplayName: chair, chairLive: live,
        activeMembers: UInt32(members.filter(\.1).count), pending: 0,
        members: members.map { RosterMember(senatorId: $0.0, displayName: $0.0, active: $0.1) })
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
