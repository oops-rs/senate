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

    /// The window's sections, in sidebar order.
    enum Section: String, CaseIterable, Hashable {
        case inbox, approvals, codex, docket, senates
    }

    private(set) var state: AppState = .placeholder
    /// The core could not start (a bad Senate home); `connect()` retries.
    private(set) var setupError: String?
    /// The last local failure (an empty reply, Terminal); the core's own
    /// `lastError` covers what reached the daemon.
    var actionError: String?
    var section: Section? = .inbox
    /// Show only the threads for (or escalated past) this agent's path;
    /// `nil` is every agent.
    var agentFilter: String?
    /// The inbox search field.
    var search = ""
    /// The thread open beside the inbox, by key.
    var selectedThread: String?
    /// The open thread as last read, and a failure to read it.
    private(set) var thread: ThreadView?
    private(set) var threadError: String?
    /// Which presided Senate's docket is shown, by key; `nil` is the first.
    var docketSenate: String?
    /// Reply drafts by thread key. They outlive the app.
    var drafts: [String: String] = [:] {
        didSet { defaults.set(drafts, forKey: Self.draftsKey) }
    }
    /// Keys of rows with an action in flight.
    private(set) var busy: Set<String> = []

    @ObservationIgnored private let makeCore: MakeCore
    @ObservationIgnored private(set) var core: (any AppCoreProtocol)?
    @ObservationIgnored private var events: Task<Void, Never>?
    @ObservationIgnored private var openWindow: OpenWindowAction?
    @ObservationIgnored private let notifier = Notifier()
    @ObservationIgnored private let defaults: UserDefaults
    private static let draftsKey = "drafts"

    init(
        makeCore: @escaping MakeCore = CoreStore.realCore,
        defaults: UserDefaults = .standard,
        notifications: Bool = true
    ) {
        self.makeCore = makeCore
        self.defaults = defaults
        drafts = defaults.dictionary(forKey: Self.draftsKey) as? [String: String] ?? [:]
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

    /// The inbox, narrowed to one agent when a filter is set and to what
    /// matches the search. A thread put to the person waits on them
    /// whichever agent it was for, so it stays in every agent's view.
    var inbox: [InboxThread] {
        var threads = state.inbox
        if let filter = agentFilter {
            threads = threads.filter { $0.forAgents.contains(filter) || $0.escalated }
        }
        let query = search.trimmingCharacters(in: .whitespaces)
        if !query.isEmpty {
            threads = threads.filter { t in
                [t.title, t.committee, t.from, t.newsLine ?? "", String(t.root)]
                    .contains { $0.localizedStandardContains(query) }
            }
        }
        return threads
    }

    /// The selected thread's row, while it is in the inbox.
    var selectedRow: InboxThread? {
        guard let key = selectedThread else { return nil }
        return state.inbox.first { $0.key == key }
    }

    /// The sections shown: the docket only while one of your agents presides.
    var sections: [Section] {
        Section.allCases.filter { $0 != .docket || state.menu.docket != nil }
    }

    /// What waits in a section, for its badge.
    func count(_ section: Section) -> UInt32 {
        let c = state.counts
        switch section {
        case .inbox: return c.threads
        case .approvals: return c.approvals
        case .codex: return 0
        case .docket: return c.docket
        case .senates: return 0
        }
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
        Mentions.shared.update()
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
        guard first else { return }
        if let v = ProcessInfo.processInfo.environment[Self.showEnv], !v.isEmpty {
            show(.inbox)
        }
        #if DEBUG
        if let dir = Snapshot.directory {
            show(.inbox)
            Snapshot.run(self, into: dir)
        }
        #endif
    }

    func show(_ section: Section) {
        self.section = section
        openWindow?(id: Self.windowID)
        NSApplication.shared.activate()
    }

    // MARK: Actions

    func refresh() {
        guard let core else { return }
        do { try core.refresh() } catch { actionError = describe(error) }
    }

    /// Read the open thread (which marks it seen). Called when it is
    /// chosen and whenever its row says something new was said.
    func loadThread(_ row: InboxThread) async {
        guard let core else { return }
        do {
            let view = try await core.thread(committee: row.committee, thread: row.root)
            guard selectedThread == row.key else { return }
            thread = view
            threadError = nil
        } catch {
            guard selectedThread == row.key else { return }
            threadError = describe(error)
        }
    }

    /// The thread shown for `row`: the one read for it, if that is it.
    func threadView(for row: InboxThread) -> ThreadView? {
        guard let thread, thread.committee == row.committee, thread.root == row.root else {
            return nil
        }
        return thread
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
            await loadThread(thread)
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
