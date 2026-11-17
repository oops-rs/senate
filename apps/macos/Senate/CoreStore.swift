import AppKit
import Foundation
import Observation
import SwiftUI

/// The app's one source of truth: the latest `AppState` snapshot from the
/// Rust core, plus what only the UI knows (the tab, the agent filter, reply
/// drafts, buttons in flight). Every action goes to the core; its result
/// comes back as state.
@MainActor
@Observable
final class CoreStore {
    static let windowID = "main"
    /// Set (non-empty) to open the window at launch (development).
    static let showEnv = "SENATE_APP_SHOW"

    enum Tab: Hashable {
        case inbox, approvals, codex, docket, senates
    }

    private(set) var state: AppState = .placeholder
    /// The core could not start (a bad Senate home); nothing works.
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

    @ObservationIgnored private var core: AppCore?
    @ObservationIgnored private var listener: ListenerBridge?
    @ObservationIgnored private var openWindow: OpenWindowAction?
    @ObservationIgnored private let notifier = Notifier()

    init() {
        notifier.onOpen = { [weak self] in self?.show(.approvals) }
        notifier.setUp()
        do {
            // nil home: the core reads $SENATE_HOME, else ~/.senate. The
            // language is the one the bundle was localized into, so the
            // core's sentences and the app's labels agree.
            let core = try AppCore(senateHome: nil, language: appLanguage())
            self.core = core
            state = core.snapshot()
            let listener = ListenerBridge(store: self)
            self.listener = listener
            try core.start(listener: listener)
        } catch {
            setupError = describe(error)
        }
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

    fileprivate func apply(_ next: AppState) {
        if next.lastError != state.lastError {
            actionError = nil
        }
        state = next
        if let filter = agentFilter, !state.agents.contains(where: { $0.path == filter }) {
            agentFilter = nil
        }
    }

    fileprivate func approvalRequested(_ approval: ApprovalRequest) {
        notifier.approvalRequested(approval)
    }

    fileprivate func forPerson(_ note: PersonNote) {
        notifier.forPerson(note)
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
    private func run(_ key: String, _ op: @Sendable (AppCore) async throws -> Void) async -> Bool {
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

/// The core's listener: hands each callback to the main thread, in order.
final class ListenerBridge: CoreListener, @unchecked Sendable {
    // Written once in init, read on the core's threads.
    private weak var store: CoreStore?

    @MainActor init(store: CoreStore) {
        self.store = store
    }

    func onState(state: AppState) {
        let store = self.store
        DispatchQueue.main.async {
            MainActor.assumeIsolated { store?.apply(state) }
        }
    }

    func onApprovalRequested(approval: ApprovalRequest) {
        let store = self.store
        DispatchQueue.main.async {
            MainActor.assumeIsolated { store?.approvalRequested(approval) }
        }
    }

    func onForPerson(note: PersonNote) {
        let store = self.store
        DispatchQueue.main.async {
            MainActor.assumeIsolated { store?.forPerson(note) }
        }
    }
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

extension InboxThread: Identifiable {
    /// Unique across committees: what drafts and busy rows are keyed by.
    var key: String { "\(committee):\(root)" }
    public var id: String { key }
}

extension ApprovalRequest: Identifiable {}

extension CodexThreadInfo: Identifiable {
    public var id: String { thread }
}

extension SenateInfo: Identifiable {
    public var id: String { key }
}

extension AgentInfo: Identifiable {
    public var id: String { "\(senate)|\(path)" }
}

extension CommitteeLine: Identifiable {
    public var id: String { "\(agent)|\(committee)" }
}

extension DocketSenate: Identifiable {
    public var id: String { senate }
}

extension DocketCommittee: Identifiable {
    public var id: String { committee }
}

extension DocketEntry: Identifiable {
    public var id: String { "\(committee):\(seq)" }
}

extension SeatLine: Identifiable {
    public var id: String { agent }
}
