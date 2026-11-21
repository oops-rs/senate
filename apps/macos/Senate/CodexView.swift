import SwiftUI
import SenateCore

/// The Codex threads registered with the daemon, each openable in Terminal.
struct CodexView: View {
    let store: CoreStore

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Text(verbatim: L("codex.intro"))
                    .font(.callout).foregroundStyle(.secondary)
                Spacer()
                Button(L("codex.new")) { store.openTerminal(thread: nil) }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 6)
            if store.state.codex.isEmpty {
                EmptyNote(online: store.state.online, text: L("codex.empty"))
            } else {
                List(store.state.codex) { thread in
                    CodexRow(store: store, thread: thread)
                        .padding(.vertical, 4)
                }
            }
        }
    }
}

private struct CodexRow: View {
    let store: CoreStore
    let thread: CodexThreadInfo

    var body: some View {
        let c = thread
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                Text(verbatim: c.committee ?? L("codex.no_committee")).font(.headline)
                Tag(text: c.agent)
                Tag(text: c.stateLabel, tint: c.state == "live" ? .green : .secondary)
                if c.approvals > 0 {
                    Tag(text: L("codex.waiting", Int(c.approvals)), tint: .red)
                }
            }
            Text(verbatim: L("codex.thread", c.thread))
                .font(.system(.callout, design: .monospaced))
                .foregroundStyle(.secondary)
                .textSelection(.enabled)
            TimedMention(date: c.lastSeen) { ago in
                var line = c.cwd
                if let branch = c.branch { line += " · \(branch)" }
                return line + L("codex.seen", ago)
            }
            .font(.callout).foregroundStyle(.secondary)
            HStack {
                Button(L("codex.open")) { store.openTerminal(thread: c.thread) }
                if c.approvals > 0 {
                    Button(L("codex.show_approvals")) { store.tab = .approvals }
                }
            }
        }
    }
}
