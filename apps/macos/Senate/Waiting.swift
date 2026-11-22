import Foundation
import SenateCore

/// One thing waiting on the person: a permission prompt in a hosted
/// terminal, a Codex approval request, or a thread. The home view and the
/// menu bar list the same items in the same order.
struct Waiting: Identifiable {
    enum Kind {
        case prompt(HostedSession)
        case approval(ApprovalRequest)
        case thread(InboxThread)
    }

    let kind: Kind

    var id: String {
        switch kind {
        case let .prompt(session): "prompt:\(session.label)"
        case let .approval(approval): "approval:\(approval.id)"
        case let .thread(thread): "thread:\(thread.key)"
        }
    }

    /// Since when it has waited.
    var since: Date {
        switch kind {
        case let .prompt(session): session.promptSince ?? .now
        case let .approval(approval): approval.receivedAt
        case let .thread(thread): thread.since
        }
    }

    var symbol: String {
        switch kind {
        case .prompt: "hand.raised"
        case .approval: "checkmark.shield"
        case .thread: "text.bubble"
        }
    }

    /// The committee its mentions are named for.
    var committee: String? {
        switch kind {
        case let .prompt(session): session.committee
        case .approval: nil
        case let .thread(thread): thread.committee
        }
    }

    /// What it is, in one line, as written (mentions still bracketed).
    var title: String {
        switch kind {
        case let .prompt(session): L("waiting.prompt", session.mention)
        case let .approval(approval): approval.summary
        case let .thread(thread): thread.title
        }
    }

    /// What it asks, or what was last said in it.
    var detail: String {
        switch kind {
        case let .prompt(session):
            return session.promptTool ?? L("waiting.prompt_detail")
        case let .approval(approval):
            return approval.command ?? approval.cwd ?? words(approval.kind)
        case let .thread(thread):
            guard let line = thread.newsLine else { return thread.from }
            return "\(thread.newsFrom ?? thread.from): \(line)"
        }
    }

    /// It holds an agent at a permission, rather than being a thread.
    var isPermission: Bool {
        if case .thread = kind { return false }
        return true
    }

    /// A thread with news you have not read.
    var unread: Bool {
        if case let .thread(thread) = kind { return thread.unread }
        return false
    }
}

extension CoreStore {
    /// The hosted terminals with a permission prompt waiting.
    var prompts: [HostedSession] {
        state.sessions.filter { $0.promptSince != nil }
    }

    /// How many things wait on the person: threads, Codex approvals and
    /// terminal prompts. The number beside the menu bar icon.
    var waitingCount: Int {
        state.inbox.count + state.approvals.count + prompts.count
    }

    /// Everything waiting on the person, the most pressing first: what
    /// holds an agent at a permission (terminal prompts, then Codex
    /// approvals, each oldest first), then the threads as the inbox
    /// orders them.
    var waiting: [Waiting] {
        permissionsWaiting + state.inbox.map { Waiting(kind: .thread($0)) }
    }

    /// The prompts and approvals of `waiting`.
    var permissionsWaiting: [Waiting] {
        let prompts = prompts
            .sorted { ($0.promptSince ?? .now) < ($1.promptSince ?? .now) }
            .map { Waiting(kind: .prompt($0)) }
        let approvals = state.approvals
            .sorted { $0.receivedAt < $1.receivedAt }
            .map { Waiting(kind: .approval($0)) }
        return prompts + approvals
    }

    /// Open the window on what waits: a thread itself, a prompt or an
    /// approval among the approvals.
    func open(_ item: Waiting) {
        switch item.kind {
        case .prompt, .approval:
            show(.approvals)
        case let .thread(thread):
            selectedThread = thread.key
            show(.inbox)
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
        case let .thread(thread):
            parts.append(thread.committee)
            parts.append(Mentions.shared.plain(thread.newsFrom ?? thread.from, committee: thread.committee))
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
