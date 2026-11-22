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
        return threadView ?? ThreadView(committee: committee, root: thread, events: [])
    }
    func watchTerminal() -> TerminalCommand {
        TerminalCommand(argv: [], cwd: nil, shellLine: "", appleScript: "")
    }
}

/// A thread waiting on alice.
func thread(
    _ root: UInt64,
    _ title: String,
    committee: String = "refund-v2",
    for agents: [String] = ["claude"],
    escalated: Bool = false,
    news: String? = nil
) -> InboxThread {
    InboxThread(
        committee: committee, root: root, updated: root, title: title,
        kind: "question", kindLabel: "question", state: "suspended", stateLabel: "suspended",
        from: "[senator::claude]", onBehalfOf: nil, to: agents.map { "[senator::\($0)]" },
        since: .now, owner: nil, triage: nil, triageLabel: nil, reason: nil, escalatedTo: nil,
        takeover: nil, answered: false, forAgents: agents, escalated: escalated, unread: news != nil,
        newsFrom: news.map { _ in "[senator::claude]" }, newsKind: nil, newsKindLabel: nil,
        newsLine: news
    )
}

func agent(_ path: String, pending: UInt32 = 0) -> AgentInfo {
    AgentInfo(
        path: path, mention: "[senator::\(path)]", senate: "", runtime: "claude",
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
        label: "refund-v2::\(name)", sessionId: "s-\(name)", mention: "[senator::\(name)]",
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
