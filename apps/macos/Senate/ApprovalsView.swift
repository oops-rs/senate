import SwiftUI
import SenateCore

/// What waits for your permission: prompts in hosted terminals (decided
/// there: attach to answer) and Codex approval requests the daemon relays
/// (answered here; the first answer wins across the TUI, this app and any
/// chat platform).
struct ApprovalsView: View {
    let store: CoreStore

    var body: some View {
        let prompts = store.state.sessions.filter { $0.promptSince != nil }
        let approvals = store.state.approvals
        if prompts.isEmpty && approvals.isEmpty {
            EmptyNote(online: store.state.online, text: L("approvals.empty"), symbol: "checkmark.shield")
        } else {
            List {
                if !prompts.isEmpty {
                    Section {
                        ForEach(prompts, id: \.sessionId) { session in
                            VStack(alignment: .leading, spacing: 6) {
                                HStack(spacing: 6) {
                                    MentionText(session.mention).font(.headline)
                                    if let committee = session.committee { Tag(text: committee) }
                                    Tag(text: session.runtime)
                                }
                                if let since = session.promptSince {
                                    PromptBanner(tool: session.promptTool, since: since) {
                                        store.attach(session)
                                    }
                                }
                            }
                            .padding(.vertical, 4)
                            .environment(\.mentionCommittee, session.committee)
                        }
                    } header: {
                        Text(verbatim: L("approvals.terminals"))
                    } footer: {
                        Text(verbatim: L("approvals.terminals_note"))
                    }
                }
                if !approvals.isEmpty {
                    Section {
                        ForEach(approvals) { approval in
                            ApprovalRow(store: store, approval: approval)
                                .padding(.vertical, 4)
                        }
                    } header: {
                        Text(verbatim: L("approvals.codex"))
                    }
                }
            }
            .listStyle(.inset)
        }
    }
}

private struct ApprovalRow: View {
    let store: CoreStore
    let approval: ApprovalRequest

    var body: some View {
        let a = approval
        let busy = store.isBusy("approval:\(a.id)")
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                Text(verbatim: a.summary).font(.headline).lineLimit(3)
                Tag(text: words(a.kind))
            }
            TimedMention(date: a.receivedAt) { ago in L("approvals.meta", a.thread, ago) }
                .font(.callout).foregroundStyle(.secondary)
            if let command = a.command {
                Text(verbatim: command)
                    .font(.callout.monospaced())
                    .textSelection(.enabled)
                    .padding(6)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 4))
            }
            if let cwd = a.cwd {
                Text(verbatim: L("approvals.in", cwd)).font(.callout).foregroundStyle(.secondary)
            }
            if !a.files.isEmpty {
                Text(verbatim: L("approvals.files", a.files.joined(separator: ", ")))
                    .font(.callout).foregroundStyle(.secondary)
            }
            if let reason = a.reason {
                Text(verbatim: reason).font(.callout)
            }
            HStack {
                Button(L("approvals.allow")) { decide(.allow) }
                    .buttonStyle(.borderedProminent)
                Button(L("approvals.allow_session")) { decide(.allowForSession) }
                Button(L("approvals.deny"), role: .destructive) { decide(.deny) }
                Button(L("approvals.deny_stop")) { decide(.denyAndStop) }
                    .help(L("approvals.deny_stop_help"))
                if busy { ProgressView().controlSize(.small) }
            }
            // The whole row waits while it is answered: the first answer wins.
            .disabled(busy || !store.state.online)
        }
    }

    private func decide(_ decision: ApprovalDecision) {
        Task { await store.decide(approval, decision) }
    }
}
