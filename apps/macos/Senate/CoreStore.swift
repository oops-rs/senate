import AppKit
import Foundation
import Observation
import SwiftUI
import SenateCore

/// The app's one source of truth: the latest `AppState` snapshot from the
/// Rust core, plus what only the UI knows (the tab, the agent filter, reply
/// drafts, buttons in flight). Every action goes to the core; its result
/// comes back as state.
///
/// The core is reached only through `AppCoreProtocol`, UniFFI's protocol
/// for `AppCore`: the app's one door to the daemon (tests pass a fake).
@Observable
final class CoreStore {
    static let windowID = "main"
    /// Set (non-empty) to open the window at launch (development).
    static let showEnv = "SENATE_APP_SHOW"

    /// Makes the core: the real one reads `$SENATE_HOME`, else `~/.senate`.
    typealias MakeCore = () throws -> any AppCoreProtocol

    enum Tab: Hashable {
        case inbox, approvals, codex, docket, senates
    }

    private(set) var state: AppState = .placeholder
    /// The core could not start (a bad Senate home); `connect()` retries.
    private(set) var setupError: String?
    /// The last local failure (an empty reply, Terminal); the core's own
    /// `lastError` covers what reached the daemon.
    var actionError: String?
    var tab: Tab = .inbox
    /// Show only the threads for (or escalated past) this agent's path;
    /// `nil` is every agent.
    var agentFilter: String?
    /// Which presided Senate's docket is shown, by key; `nil` is the first.
    var docketSenate: String?
    /// Reply drafts by thread key, and which threads have the box open.
    var drafts: [String: String] = [:]
    var replying: Set<String> = []
    /// Keys of rows with an action in flight.
    private(set) var busy: Set<String> = []

    @ObservationIgnored private let makeCore: MakeCore
    @ObservationIgnored private(set) var core: (any AppCoreProtocol)?
    @ObservationIgnored private var events: Task<Void, Never>?
    @ObservationIgnored private var openWindow: OpenWindowAction?
    @ObservationIgnored private let notifier = Notifier()

    init(makeCore: @escaping MakeCore = CoreStore.realCore, notifications: Bool = true) {
        self.makeCore = makeCore
        if notifications {
            notifier.onOpen = { [weak self] in self?.show(.approvals) }
            notifier.setUp()
        }
        connect()
    }

    /// The real core. The language is the one the bundle was localized
    /// into, so the core's sentences and the app's labels agree.
    nonisolated static func realCore() throws -> any AppCoreProtocol {
        try AppCore(senateHome: nil, language: appLanguage())
    }

    /// Make and start the core. After a setup error, tries again.
    func connect() {
        guard core == nil else { return }
        let (stream, continuation) = AsyncStream.makeStream(of: CoreEvent.self)
        do {
            let core = try makeCore()
            state = core.snapshot()
            try core.start(listener: CoreEvents(continuation))
            self.core = core
            setupError = nil
        } catch {
            continuation.finish()
            setupError = describe(error)
            return
        }
        // The core's callbacks, in the order it made them, on the main actor.
        events = Task { [weak self] in
            for await event in stream {
                guard let self else { return }
                self.handle(event)
            }
        }
    }

    deinit {
        events?.cancel()
    }

    /// The error line under the header.
    var shownError: String? {
        setupError ?? actionError ?? state.lastError
    }

    /// The agents whose inbox the app can read.
    var inboxAgents: [AgentInfo] {
        state.agents.filter { $0.inbox }
    }

    /// The inbox, narrowed to one agent when a filter is set. A thread put to
    /// the person waits on them whichever agent it was for, so it stays.
    var inbox: [InboxThread] {
        guard let filter = agentFilter else { return state.inbox }
        return state.inbox.filter { $0.forAgents.contains(filter) || $0.escalated }
    }

    /// The docket shown: the chosen presided Senate, else the first.
    var docket: DocketSenate? {
        state.docket.first { $0.senate == docketSenate } ?? state.docket.first
    }

    // MARK: From the core

    func handle(_ event: CoreEvent) {
        switch event {
        case let .state(next): apply(next)
        case let .approval(approval): notifier.approvalRequested(approval)
        case let .person(note): notifier.forPerson(note)
        }
    }

    private func apply(_ next: AppState) {
        if next.lastError != state.lastError {
            actionError = nil
        }
        state = next
        if let filter = agentFilter, !state.agents.contains(where: { $0.path == filter }) {
            agentFilter = nil
        }
    }

    // MARK: Window

    func attach(openWindow: OpenWindowAction) {
        let first = self.openWindow == nil
        self.openWindow = openWindow
        if first, let v = ProcessInfo.processInfo.environment[Self.showEnv], !v.isEmpty {
            show(.inbox)
        }
    }

    func show(_ tab: Tab) {
        self.tab = tab
        openWindow?(id: Self.windowID)
        NSApplication.shared.activate()
    }

    // MARK: Actions

    func refresh() {
        guard let core else { return }
        do { try core.refresh() } catch { actionError = describe(error) }
    }

    func toggleReply(_ thread: InboxThread) {
        if replying.contains(thread.key) {
            replying.remove(thread.key)
        } else {
            replying.insert(thread.key)
        }
    }

    func sendReply(_ thread: InboxThread) async {
        let body = drafts[thread.key] ?? ""
        guard !body.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            actionError = L("error.empty_reply")
            return
        }
        let ok = await run(thread.key) { core in
            try await core.reply(committee: thread.committee, thread: thread.root, body: body)
        }
        if ok {
            drafts[thread.key] = nil
            replying.remove(thread.key)
        }
    }

    func take(_ thread: InboxThread) async {
        await run(thread.key) { core in
            try await core.take(committee: thread.committee, thread: thread.root)
        }
    }

    func close(_ thread: InboxThread) async {
        await run(thread.key) { core in
            try await core.close(committee: thread.committee, thread: thread.root)
        }
    }

    func decide(_ approval: ApprovalRequest, _ decision: ApprovalDecision) async {
        await run("approval:\(approval.id)") { core in
            try await core.approve(id: approval.id, decision: decision)
        }
    }

    /// Open Terminal.app running Codex: resuming `thread` in its directory,
    /// or a new one.
    func openTerminal(thread: String?) {
        guard let core else { return }
        do {
            let command = try core.codexTerminal(threadId: thread)
            try Terminal.run(command)
            actionError = nil
        } catch {
            actionError = describe(error)
        }
    }

    func isBusy(_ key: String) -> Bool { busy.contains(key) }

    @discardableResult
    private func run(_ key: String, _ op: (any AppCoreProtocol) async throws -> Void) async -> Bool {
        guard let core else { return false }
        busy.insert(key)
        defer { busy.remove(key) }
        do {
            try await op(core)
            actionError = nil
            return true
        } catch {
            actionError = describe(error)
            return false
        }
    }
}

/// What the core tells the app, in the order it says it.
enum CoreEvent: Sendable {
    case state(AppState)
    case approval(ApprovalRequest)
    case person(PersonNote)
}

/// The core's listener: called on the core's threads, it only yields each
/// callback into the stream `CoreStore` reads on the main actor.
nonisolated final class CoreEvents: CoreListener {
    private let continuation: AsyncStream<CoreEvent>.Continuation

    init(_ continuation: AsyncStream<CoreEvent>.Continuation) {
        self.continuation = continuation
    }

    func onState(state: AppState) { continuation.yield(.state(state)) }
    func onApprovalRequested(approval: ApprovalRequest) { continuation.yield(.approval(approval)) }
    func onForPerson(note: PersonNote) { continuation.yield(.person(note)) }
}

/// An error as the user should read it. What the core words (`Invalid`,
/// `Failed`) is already in the app's language.
func describe(_ error: Error) -> String {
    switch error {
    case let e as CoreError:
        switch e {
        case .NotStarted: L("error.not_started")
        case .AlreadyStarted: L("error.already_started")
        case let .Invalid(message), let .Failed(message), let .Setup(message): message
        }
    default:
        error.localizedDescription
    }
}

extension AppState {
    /// Before the core's first snapshot.
    static let placeholder = AppState(
        online: false,
        offlineReason: nil,
        person: nil,
        senates: [],
        agents: [],
        menu: MenuState(
            title: L("menu.offline"),
            tooltip: L("menu.offline"),
            pending: L("menu.daemon_down"),
            lines: [],
            docket: nil,
            badge: nil
        ),
        inbox: [],
        approvals: [],
        codex: [],
        docket: [],
        counts: Counts(threads: 0, approvals: 0, codex: 0, docket: 0, pending: 0),
        lastError: nil
    )
}

extension InboxThread: @retroactive Identifiable {
    /// Unique across committees: what drafts and busy rows are keyed by.
    var key: String { "\(committee):\(root)" }
    public var id: String { key }
}

extension ApprovalRequest: @retroactive Identifiable {}

extension CodexThreadInfo: @retroactive Identifiable {
    public var id: String { thread }
}

extension SenateInfo: @retroactive Identifiable {
    public var id: String { key }
}

extension AgentInfo: @retroactive Identifiable {
    public var id: String { "\(senate)|\(path)" }
}

extension CommitteeLine: @retroactive Identifiable {
    public var id: String { "\(agent)|\(committee)" }
}

extension DocketSenate: @retroactive Identifiable {
    public var id: String { senate }
}

extension DocketCommittee: @retroactive Identifiable {
    public var id: String { committee }
}

extension DocketEntry: @retroactive Identifiable {
    public var id: String { "\(committee):\(seq)" }
}

extension SeatLine: @retroactive Identifiable {
    public var id: String { agent }
}
