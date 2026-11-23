import Foundation
import SenateCore

/// One thing waiting on the person: a permission prompt in a hosted
/// terminal, a Codex approval request, a thread that waits on them (asked,
/// escalated or said to them), or an item put to the Speaker their agent
/// holds. The home view, the sidebar and the menu bar list the same items
/// in the same order.
struct Waiting: Identifiable {
    enum Kind {
        case prompt(HostedSession)
        case approval(ApprovalRequest)
        case thread(InboxThread)
        case docket(DocketEntry)
    }

    let kind: Kind
    /// The committee it is on; its mentions are named for it. A Codex
    /// approval is on its thread's committee, when that is known.
    let committee: String?
    /// Who asks, as a mention: the agent at the prompt or the approval,
    /// who escalated or asked, who put it to the Speaker.
    let asker: String?

    init(kind: Kind, committee: String? = nil, asker: String? = nil) {
        self.kind = kind
        switch kind {
        case let .prompt(session):
            self.committee = session.committee
            self.asker = session.mention
        case .approval:
            self.committee = committee
            self.asker = asker
        case let .thread(thread):
            self.committee = thread.committee
            self.asker = thread.asker
        case let .docket(entry):
            self.committee = entry.committee
            self.asker = entry.by
        }
    }

    var id: String {
        switch kind {
        case let .prompt(session): "prompt:\(session.label)"
        case let .approval(approval): "approval:\(approval.id)"
        case let .thread(thread): "thread:\(thread.key)"
        case let .docket(entry): "docket:\(entry.id)"
        }
    }

    /// Since when it has waited.
    var since: Date {
        switch kind {
        case let .prompt(session): session.promptSince ?? .now
        case let .approval(approval): approval.receivedAt
        case let .thread(thread): thread.since
        case let .docket(entry): entry.since
        }
    }

    var symbol: String {
        switch kind {
        case .prompt: "hand.raised"
        case .approval: "checkmark.shield"
        case let .thread(thread): thread.escalated ? "arrow.up.forward.circle" : "text.bubble"
        case .docket: "tray"
        }
    }

    /// What it is, in one line, as written (mentions still bracketed).
    var title: String {
        switch kind {
        case let .prompt(session): L("waiting.prompt", session.mention)
        case let .approval(approval): approval.summary
        case let .thread(thread): thread.title
        case let .docket(entry): firstLine(entry.body)
        }
    }

    /// What it asks, or what was last said in it, after who says it.
    var detail: String {
        switch kind {
        case let .prompt(session):
            return session.promptTool ?? L("waiting.prompt_detail")
        case let .approval(approval):
            return approval.command ?? approval.cwd ?? words(approval.kind)
        case let .thread(thread):
            let line = thread.newsLine ?? thread.kindLabel
            return "\(thread.asker): \(line)"
        case let .docket(entry):
            return entry.why
        }
    }

    /// What kind of thing it is, in a word: `request`, `escalated`, `to the Speaker`.
    var kindLabel: String? {
        switch kind {
        case .prompt, .approval: nil
        case let .thread(thread): thread.escalated ? L("inbox.escalated") : thread.kindLabel
        case .docket: L("waiting.to_speaker")
        }
    }

    /// It holds an agent at a permission, rather than being a thread.
    var isPermission: Bool {
        switch kind {
        case .prompt, .approval: true
        case .thread, .docket: false
        }
    }

    /// A thread with news you have not read.
    var unread: Bool {
        if case let .thread(thread) = kind { return thread.unread }
        return false
    }
}

/// The first line of a text.
func firstLine(_ text: String) -> String {
    text.split(whereSeparator: \.isNewline).first.map(String.init) ?? ""
}

/// The waiting items of one committee.
struct CommitteeWaiting: Identifiable {
    let committee: String
    let items: [Waiting]
    var id: String { committee }
}

extension CoreStore {
    /// The hosted terminals with a permission prompt waiting.
    var prompts: [HostedSession] {
        state.sessions.filter { $0.promptSince != nil }
    }

    /// Threads that wait on the person, not on their agents.
    var personThreads: [InboxThread] {
        state.inbox.filter(\.forPerson)
    }

    /// Threads their agents are still working on: listed apart, not counted.
    var agentThreads: [InboxThread] {
        state.inbox.filter { !$0.forPerson }
    }

    /// What awaits the Speaker the person's agent holds, every Senate.
    var docketEntries: [DocketEntry] {
        state.docket.flatMap(\.committees).flatMap(\.items)
    }

    /// How many things wait on the person: the number beside the menu bar
    /// icon, and the length of `waiting`.
    var waitingCount: Int {
        prompts.count + state.approvals.count + personThreads.count + docketEntries.count
    }

    /// Everything waiting on the person, the most pressing first: what
    /// holds an agent at a permission, then each committee's threads and
    /// items for the Speaker.
    var waiting: [Waiting] {
        permissionsWaiting + waitingByCommittee.flatMap(\.items)
    }

    /// The threads and docket items waiting on the person, by committee:
    /// the committee that has waited longest first, each oldest first.
    var waitingByCommittee: [CommitteeWaiting] {
        let items = personThreads.map { Waiting(kind: .thread($0)) }
            + docketEntries.map { Waiting(kind: .docket($0)) }
        let groups = Dictionary(grouping: items) { $0.committee ?? "" }
        return groups
            .map { CommitteeWaiting(committee: $0.key, items: $0.value.sorted { $0.since < $1.since }) }
            .sorted { ($0.items.first?.since ?? .now, $0.committee) < ($1.items.first?.since ?? .now, $1.committee) }
    }

    /// The prompts and approvals of `waiting`.
    var permissionsWaiting: [Waiting] {
        let prompts = prompts
            .sorted { ($0.promptSince ?? .now) < ($1.promptSince ?? .now) }
            .map { Waiting(kind: .prompt($0)) }
        let approvals = state.approvals
            .sorted { $0.receivedAt < $1.receivedAt }
            .map { approval in
                let codex = state.codex.first { $0.thread == approval.thread }
                return Waiting(
                    kind: .approval(approval),
                    committee: codex?.committee,
                    asker: codex.map { "[senator::\($0.agent)]" }
                )
            }
        return prompts + approvals
    }

    /// Open the window on what waits: a thread itself, a prompt or an
    /// approval among the approvals, an item for the Speaker in the docket.
    func open(_ item: Waiting) {
        switch item.kind {
        case .prompt, .approval:
            show(.approvals)
        case let .thread(thread):
            selectedThread = thread.key
            show(.inbox)
        case .docket:
            show(.docket)
        }
    }
}

/// The menu bar's menu: one line saying how much waits, and the first few
/// items. Nothing else about Senate is in the menu.
struct WaitingMenu {
    struct Row: Identifiable {
        let item: Waiting
        /// What it is, with mentions as names.
        let title: String
        /// What it asks or where it is, who, and for how long.
        let subtitle: String

        var id: String { item.id }
    }

    /// How many items the menu lists before "Show All".
    static let limit = 8

    /// `3 waiting on you`, `Nothing waiting on you`, `Offline`.
    let header: String
    let rows: [Row]
    /// Everything that waits, when the menu shows only the first of it.
    let total: Int

    var truncated: Bool { rows.count < total }
}

extension CoreStore {
    /// What the menu bar's menu shows now.
    func waitingMenu(now: Date = .now) -> WaitingMenu {
        if setupError != nil {
            return WaitingMenu(header: L("setup.title"), rows: [], total: 0)
        }
        guard state.online else {
            return WaitingMenu(header: L("menu.offline_short"), rows: [], total: 0)
        }
        let total = waitingCount
        guard total > 0 else {
            return WaitingMenu(header: L("menu.clear"), rows: [], total: 0)
        }
        let rows = waiting.prefix(WaitingMenu.limit).map { item in
            WaitingMenu.Row(
                item: item,
                title: Self.short(Mentions.shared.plain(item.title, committee: item.committee), 64),
                subtitle: Self.menuSubtitle(item, now: now)
            )
        }
        return WaitingMenu(header: L("menu.waiting", Int64(total)), rows: rows, total: total)
    }

    private static func menuSubtitle(_ item: Waiting, now: Date) -> String {
        var parts: [String] = []
        switch item.kind {
        case let .prompt(session):
            if let tool = session.promptTool { parts.append(short(tool, 48)) }
            if let committee = session.committee { parts.append(committee) }
        case let .approval(approval):
            if let command = approval.command { parts.append(short(command, 48)) }
            if let asker = item.asker { parts.append(Mentions.shared.plain(asker, committee: item.committee)) }
        case let .thread(thread):
            parts.append(thread.committee)
            parts.append(Mentions.shared.plain(thread.asker, committee: thread.committee))
        case let .docket(entry):
            parts.append(entry.committee)
            parts.append(Mentions.shared.plain(entry.by, committee: entry.committee))
        }
        parts.append(Ago.text(item.since, now: now))
        return parts.joined(separator: " · ")
    }

    /// One line, no longer than `limit` characters: a menu grows as wide as
    /// its longest item.
    private static func short(_ text: String, _ limit: Int) -> String {
        let line = text.split(whereSeparator: \.isNewline).first.map(String.init) ?? ""
        return line.count > limit ? line.prefix(limit - 1) + "…" : line
    }
}
