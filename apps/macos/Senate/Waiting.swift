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
