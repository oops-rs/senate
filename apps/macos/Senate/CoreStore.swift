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

    /// The window's fixed pages. Inbox and Approvals head the sidebar;
    /// Senates opens from the account menu at its foot.
    enum Section: String, CaseIterable, Hashable {
        case inbox, approvals, senates
    }

    /// What the window shows: a page, a thread, a session, a committee.
    enum Place: Hashable {
        case section(Section)
        /// A thread waiting on you, by `InboxThread.key`.
        case thread(String)
        /// A session on this machine, by its session id.
        case session(String)
        /// A committee's page, by name.
        case committee(String)
        /// A waiting item that is not a thread (a prompt, an approval, an
        /// item for the Speaker), by its `Waiting.id`: choosing it opens
        /// where it is answered.
        case item(String)
    }

    private(set) var state: AppState = .placeholder
    /// The core could not start (a bad Senate home); `connect()` retries.
    private(set) var setupError: String?
    /// The last local failure (an empty reply, Terminal); the core's own
    /// `lastError` covers what reached the daemon.
    var actionError: String?
    /// What the window shows.
    var place: Place = .section(.inbox) {
        didSet { placeChanged(from: oldValue) }
    }
    /// Show only the threads for (or escalated past) this agent's path;
    /// `nil` is every agent.
    var agentFilter: String?
    /// The inbox search field.
    var search = ""
    /// The open thread as last read, and a failure to read it.
    private(set) var thread: ThreadView?
    private(set) var threadError: String?
    /// The open session's conversation, as read and followed.
    var timeline: SessionTimeline?
    var deskSelection: DocketEntry?
    /// What the person is writing to each session, by session id.
    var sessionDrafts: [String: String] = [:]
    /// Committees folded in the sidebar.
    var folded: Set<String> = [] {
        didSet { defaults.set(Array(folded), forKey: Self.foldedKey) }
    }
    private static let foldedKey = "foldedCommittees"
    /// Show agents, sessions and committees that are no longer active
    /// (the roster's own rule, from the core). Off by default; remembered.
    var showInactive = false {
        didSet { defaults.set(showInactive, forKey: Self.showInactiveKey) }
    }
    private static let showInactiveKey = "showInactive"
    /// Show Senate in the Dock (a regular app with a main menu), or keep it
    /// in the menu bar only. On unless turned off; applies at once.
    var showInDock = true {
        didSet {
            defaults.set(showInDock, forKey: Dock.key)
            if applyDock { Dock.apply(showInDock: showInDock) }
        }
    }
    /// Whether changing `showInDock` changes the running app (not for the
    /// tests' stores).
    @ObservationIgnored private var applyDock = false
    /// Asks the sidebar to open its search (the View menu's Find).
    var searchRequests = 0
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
    @ObservationIgnored private(set) var openSettings: OpenSettingsAction?
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
        showInactive = defaults.bool(forKey: Self.showInactiveKey)
        showInDock = Dock.showsInDock(defaults)
        applyDock = notifications
        folded = Set(defaults.stringArray(forKey: Self.foldedKey) ?? [])
        if notifications {
            notifier.handle = { [weak self] action in await self?.perform(action) }
            notifier.setUp()
        }
        connect()
    }

    /// Settings' Senate home; empty or unset is `$SENATE_HOME`, else
    /// `~/.senate`.
    nonisolated static let homeKey = "senateHome"

    /// The app's store. As the unit tests' host, the app connects to
    /// nothing and posts nothing: the tests make their own stores.
    static func forApp() -> CoreStore {
        if ProcessInfo.processInfo.environment["XCTestConfigurationFilePath"] != nil {
            return CoreStore(makeCore: { throw CoreError.Setup(message: "testing") }, notifications: false)
        }
        return CoreStore()
    }

    /// The real core. The Senate home: `$SENATE_HOME` when set (development
    /// runs), else the one chosen in Settings, else `~/.senate`. The
    /// language is the one the bundle was localized into, so the core's
    /// sentences and the app's labels agree.
    nonisolated static func realCore() throws -> any AppCoreProtocol {
        let env = ProcessInfo.processInfo.environment["SENATE_HOME"].flatMap { $0.isEmpty ? nil : $0 }
        let chosen = UserDefaults.standard.string(forKey: homeKey).flatMap { $0.isEmpty ? nil : $0 }
        return try AppCore(senateHome: env == nil ? chosen : nil, language: appLanguage())
    }

    /// The Senate home the core uses.
    var senateHome: String? { core?.senateHome() }

    /// Start over with another Senate home (Settings): the old core and its
    /// connection go, a new one connects.
    func restart() {
        events?.cancel()
        events = nil
        core = nil
        state = .placeholder
        thread = nil
        deskSelection = nil
        place = .section(.inbox)
        connect()
    }

    /// The app's and the core's unified log, live, in Terminal.
    func openLogStream() {
        let line = "log stream --info --predicate 'subsystem == \"\(Log.subsystem)\"'"
        do {
            try Terminal.run(shell: line)
        } catch {
            actionError = describe(error)
        }
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
            Log.app.error("the core could not start: \(self.setupError ?? "", privacy: .public)")
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

    /// The agents whose inbox the app can read: the active ones, and any
    /// with something waiting, unless inactive ones are shown too.
    var inboxAgents: [AgentInfo] {
        state.agents.filter { $0.inbox && (showInactive || $0.active || $0.pending > 0) }
    }

    /// Every committee of the primary Senate with its chair and members,
    /// as the core's roster reads them. A daemon too old to read the roster
    /// gives the chairs alone.
    var roster: [RosterCommittee] {
        guard let senate = state.senates.first else { return [] }
        if !senate.roster.isEmpty { return senate.roster }
        return senate.chairs.map { chair in
            RosterCommittee(
                committee: chair.committee,
                chairId: chair.chair.map(Mentions.id),
                chairDisplayName: nil, chairLive: false, activeMembers: 0, pending: 0, members: []
            )
        }
    }

    /// Whether a committee is worth showing without "Show inactive": an
    /// active member or chair, or something waiting on the person there.
    func isActive(_ committee: RosterCommittee) -> Bool {
        committee.activeMembers > 0 || committee.chairLive
            || waitingByCommittee.contains { $0.committee == committee.committee }
    }

    /// The search, trimmed; empty when not searching.
    var query: String { search.trimmingCharacters(in: .whitespaces) }

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

    /// Open an item that is not a thread where it is answered.
    private func placeChanged(from old: Place) {
        if case let .item(id) = place {
            if let item = waiting.first(where: { $0.id == id }) {
                open(item)
            } else {
                place = old
            }
            return
        }
        if case let .thread(key) = place, !state.inbox.contains(where: { $0.key == key }),
           thread.map({ "\($0.committee):\($0.root)" }) != key,
           let colon = key.lastIndex(of: ":"), let root = UInt64(key[key.index(after: colon)...]) {
            threadError = nil
            Task { await loadThread(committee: String(key[..<colon]), root: root) }
        }
        if old != place { syncTimeline() }
    }

    /// The open thread, while it is in the inbox.
    var selectedThread: String? {
        if case let .thread(key) = place { return key }
        return nil
    }

    /// The selected thread's row: its inbox row, else one made from what
    /// was read of it (a thread no longer, or never, in the inbox).
    var selectedRow: InboxThread? {
        guard let key = selectedThread else { return nil }
        if let row = state.inbox.first(where: { $0.key == key }) { return row }
        guard let thread, "\(thread.committee):\(thread.root)" == key else { return nil }
        return InboxThread(read: thread)
    }

    /// Open a thread by its committee and root, whether or not it waits on
    /// you: what is not in the inbox is read first (`placeChanged`).
    func openThread(committee: String, root: UInt64) {
        place = .thread("\(committee):\(root)")
    }

    /// The open session, while this machine has it.
    var selectedSession: SessionLine? {
        guard case let .session(id) = place else { return nil }
        return session(id)
    }

    func session(_ id: String) -> SessionLine? {
        for group in state.groups {
            if let s = group.sessions.first(where: { $0.sessionId == id }) { return s }
        }
        return nil
    }

    /// The fixed pages at the head of the sidebar.
    var sections: [Section] { [.inbox, .approvals] }

    /// What waits in a section, for its badge: the threads for you (not
    /// those to decide), and everything waiting for a yes or a no.
    func count(_ section: Section) -> Int {
        switch section {
        case .inbox: inboxThreads.count
        case .approvals: approvalsCount
        case .senates: 0
        }
    }

    // MARK: From the core

    func handle(_ event: CoreEvent) {
        switch event {
        case let .state(next): Perf.measure("apply", "\(next.inbox.count) threads") { apply(next) }
        case let .approval(approval): notifier.approvalRequested(approval)
        case let .person(note): notifier.forPerson(note)
        case let .prompt(session): notifier.promptWaiting(session)
        }
    }

    private func apply(_ next: AppState) {
        Mentions.shared.update()
        // An approval answered anywhere takes its banner down.
        let still = Set(next.approvals.map(\.id))
        for gone in state.approvals.map(\.id) where !still.contains(gone) {
            notifier.withdrawApproval(gone)
        }
        if next.lastError != state.lastError {
            actionError = nil
        }
        state = next
        if applyDock { Dock.badge(state.online ? waitingCount : 0) }
        if let filter = agentFilter, !state.agents.contains(where: { $0.path == filter }) {
            agentFilter = nil
        }
        if let timeline, let line = session(timeline.sessionId) {
            timeline.working = line.working || timeline.working && line.status == .working
        }
    }

    // MARK: Notifications

    /// What a person did with a notification.
    func perform(_ action: NotificationAction) async {
        switch action {
        case let .openThread(committee, root):
            place = .thread("\(committee):\(root)")
            showWindow()
        case let .reply(committee, root, body):
            await reply(committee: committee, root: root, body: body, key: "\(committee):\(root)")
        case let .take(committee, root):
            await run("\(committee):\(root)") { core in
                try await core.take(committee: committee, thread: root)
            }
        case .openApprovals:
            show(.approvals)
        case let .approve(id, allow):
            await run("approval:\(id)") { core in
                try await core.approve(id: id, decision: allow ? .allow : .deny)
            }
        case let .openSession(sessionId):
            // That exact session's page; if it has gone, the approvals, and why.
            if session(sessionId) != nil {
                place = .session(sessionId)
                showWindow()
            } else {
                actionError = L("error.prompt_gone")
                show(.approvals)
            }
        case let .attach(sessionId):
            // That exact session, and only while its prompt still waits:
            // never whatever holds its alias now.
            guard let session = waitingSession(sessionId) else {
                actionError = L("error.prompt_gone")
                show(.approvals)
                return
            }
            attach(session)
        }
    }

    /// The hosted session with this id, if a prompt still waits in it.
    func waitingSession(_ sessionId: String) -> HostedSession? {
        state.sessions.first { $0.sessionId == sessionId && $0.promptSince != nil }
    }

    /// Ask for notification permission (Settings).
    func requestNotifications() async -> Bool {
        await notifier.requestAuthorization()
    }

    func notificationsAuthorized() async -> Bool {
        await notifier.authorized()
    }

    // MARK: Window

    func attach(openWindow: OpenWindowAction, openSettings: OpenSettingsAction) {
        let first = self.openWindow == nil
        self.openWindow = openWindow
        self.openSettings = openSettings
        guard first else { return }
        // ⌘, opens Settings from any of the app's windows.
        NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            let command = event.modifierFlags.intersection(.deviceIndependentFlagsMask) == .command
            // The comma key by its place too: an input method may type "，" there.
            guard command, event.charactersIgnoringModifiers == "," || event.keyCode == 43 else { return event }
            MainActor.assumeIsolated { self?.openSettings?() }
            return nil
        }
        if let v = ProcessInfo.processInfo.environment[Self.showEnv], !v.isEmpty {
            show(.inbox)
        }
        Perf.tour(self)
        Perf.sessionTour(self)
        #if DEBUG
        if let dir = Snapshot.directory {
            show(.inbox)
            Snapshot.run(self, into: dir)
        }
        #endif
    }

    func show(_ section: Section) {
        place = .section(section)
        showWindow()
    }

    /// Bring the window forward, on whatever it last showed.
    func showWindow() {
        openWindow?(id: Self.windowID)
        NSApplication.shared.activate()
    }

    /// The home view itself: everything that waits, no thread open.
    func showHome() {
        place = .section(.inbox)
        showWindow()
    }

    // MARK: Actions

    /// Try to reach the daemon now rather than after the current wait.
    func reconnect() {
        guard let core else {
            connect()
            return
        }
        do { try core.reconnect() } catch { actionError = describe(error) }
    }

    func refresh() {
        guard let core else { return }
        do { try core.refresh() } catch { actionError = describe(error) }
    }

    /// Read the open thread (which marks it seen). Called when it is
    /// chosen and whenever its row says something new was said.
    func deskItem(_ entry: DocketEntry) async throws -> DeskItemView {
        guard let core else { throw CoreError.NotStarted }
        return try await core.deskItem(committee: entry.committee, seq: entry.seq)
    }

    func newDeskActionId() throws -> String {
        guard let core else { throw CoreError.NotStarted }
        return core.newDeskActionId()
    }

    func decideDeskItem(_ entry: DocketEntry, itemId: String, id: String, reason: String?) async throws -> DeskOutcomeView {
        guard let core else { throw CoreError.NotStarted }
        return try await core.decideDeskItem(committee: entry.committee, seq: entry.seq, itemId: itemId, id: id, reason: reason)
    }

    func loadThread(_ row: InboxThread) async {
        await loadThread(committee: row.committee, root: row.root)
    }

    func loadThread(committee: String, root: UInt64) async {
        guard let core else { return }
        let key = "\(committee):\(root)"
        do {
            let view = try await Perf.measure("thread", "#\(root)") {
                try await core.thread(committee: committee, thread: root)
            }
            guard selectedThread == key else { return }
            thread = view
            threadError = nil
        } catch {
            guard selectedThread == key else { return }
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
        if await reply(committee: thread.committee, root: thread.root, body: body, key: thread.key) {
            drafts[thread.key] = nil
            await loadThread(thread)
        }
    }

    /// Reply in a thread as the person; `key` is the row that waits.
    @discardableResult
    func reply(committee: String, root: UInt64, body: String, key: String) async -> Bool {
        guard !body.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            actionError = L("error.empty_reply")
            return false
        }
        return await run(key) { core in
            try await core.reply(committee: committee, thread: root, body: body)
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
        runInTerminal { try $0.codexTerminal(threadId: thread) }
    }

    /// Open a hosted session's terminal in Terminal (`senate attach`).
    func attach(_ session: HostedSession) {
        runInTerminal { $0.attachTerminalForSession(sessionId: session.sessionId) }
    }

    /// Every hosted terminal side by side, in Terminal.
    func openWatch() {
        runInTerminal { $0.watchTerminal() }
    }

    func stop(_ session: HostedSession) async {
        await run("session:\(session.label)") { core in
            try await core.stopSession(label: session.label)
        }
    }

    /// Answer the permission prompt waiting in a hosted terminal, as the
    /// person: the daemon answers it as they would there, and notes it in
    /// the committee log.
    func decide(_ session: HostedSession, _ decision: ApprovalDecision) async {
        await run("session:\(session.label)") { core in
            try await core.approveSession(label: session.label, decision: decision)
        }
    }

    func pause(_ session: HostedSession) async {
        await run("session:\(session.label)") { core in
            try await core.pauseSession(label: session.label)
        }
    }

    /// Say what the person wrote to a session's senator, as them: a note in
    /// its committee's log, delivered to the session.
    func say(to session: SessionLine) async {
        let body = sessionDrafts[session.sessionId] ?? ""
        guard !body.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            actionError = L("error.empty_reply")
            return
        }
        if await run("say:\(session.sessionId)", { core in
            try await core.say(sessionId: session.sessionId, body: body)
        }) {
            sessionDrafts[session.sessionId] = nil
        }
    }

    func restore(_ session: HostedSession) async {
        await run("session:\(session.label)") { core in
            try await core.restoreSession(label: session.label)
        }
    }

    func resume(_ session: HostedSession) async {
        await run("session:\(session.label)") { core in
            try await core.resumeSession(label: session.label)
        }
    }

    private func runInTerminal(_ command: (any AppCoreProtocol) throws -> TerminalCommand) {
        guard let core else { return }
        do {
            try Terminal.run(command(core))
            actionError = nil
        } catch {
            actionError = describe(error)
        }
    }

    func isBusy(_ key: String) -> Bool { busy.contains(key) }

    @discardableResult
    func run(_ key: String, _ op: (any AppCoreProtocol) async throws -> Void) async -> Bool {
        guard let core else { return false }
        busy.insert(key)
        defer { busy.remove(key) }
        do {
            try await op(core)
            actionError = nil
            return true
        } catch {
            actionError = describe(error)
            Log.app.notice("\(key, privacy: .public): \(self.actionError ?? "", privacy: .public)")
            return false
        }
    }
}

/// What the core tells the app, in the order it says it.
enum CoreEvent: Sendable {
    case state(AppState)
    case approval(ApprovalRequest)
    case person(PersonNote)
    case prompt(HostedSession)
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
    func onPromptWaiting(session: HostedSession) { continuation.yield(.prompt(session)) }
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
    nonisolated static let placeholder = AppState(
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
            prompts: nil,
            badge: nil
        ),
        inbox: [],
        approvals: [],
        codex: [],
        docket: [],
        sessions: [],
        groups: [],
        counts: Counts(threads: 0, approvals: 0, codex: 0, docket: 0, prompts: 0, forPerson: 0, pending: 0),
        lastError: nil
    )
}

extension InboxThread {
    /// A thread that is in no inbox, as its read says: its title, kind and
    /// state, and nothing waiting on anyone.
    init(read view: ThreadView) {
        let root = view.events.first { $0.seq == view.root } ?? view.events.first
        self.init(
            committee: view.committee, root: view.root, updated: view.events.last?.seq ?? view.root,
            title: view.title, kind: root?.kind ?? "", kindLabel: view.kindLabel,
            state: view.state ?? "active", stateLabel: view.stateLabel ?? "",
            from: root?.by ?? "", onBehalfOf: nil, to: root?.to ?? [], since: root?.at ?? .now,
            owner: nil, triage: nil, triageLabel: nil, reason: nil, escalatedTo: nil, takeover: nil,
            answered: false, forPerson: false, asker: root?.by ?? "", forAgents: [], escalated: false,
            toDecide: false, unread: false, newsFrom: nil, newsKind: nil, newsKindLabel: nil, newsLine: nil)
    }
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

extension RosterCommittee: @retroactive Identifiable {
    public var id: String { committee }
}

extension RosterMember: @retroactive Identifiable {
    public var id: String { senatorId }
}

extension SeatLine: @retroactive Identifiable {
    public var id: String { agent }
}
