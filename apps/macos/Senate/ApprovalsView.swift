import SwiftUI
import SenateCore

/// Codex approval requests the daemon relays. The first answer wins across
/// the TUI, this app and any chat platform.
struct ApprovalsView: View {
    let store: CoreStore

    var body: some View {
        if store.state.approvals.isEmpty {
            EmptyNote(online: store.state.online, text: L("approvals.empty"))
        } else {
            List(store.state.approvals) { approval in
                ApprovalRow(store: store, approval: approval)
                    .padding(.vertical, 4)
            }
        }
    }
}

private struct ApprovalRow: View {
    let store: CoreStore
    let approval: ApprovalRequest

    var body: some View {
        let a = approval
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                Text(verbatim: a.summary).font(.headline).lineLimit(3)
                Tag(text: words(a.kind))
            }
            TimedMention(date: a.receivedAt) { ago in L("approvals.meta", a.thread, ago) }
                .font(.callout).foregroundStyle(.secondary)
            if let command = a.command {
                Text(verbatim: command)
                    .font(.system(.callout, design: .monospaced))
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
                if store.isBusy("approval:\(a.id)") { ProgressView().controlSize(.small) }
            }
            // The whole row waits while it is answered: the first answer wins.
            .disabled(store.isBusy("approval:\(a.id)"))
        }
    }

    private func decide(_ decision: ApprovalDecision) {
        Task { await store.decide(approval, decision) }
    }
}
