import SwiftUI
import SenateCore

/// This machine's hosted sessions, and the Codex threads registered with the
/// daemon. A hosted terminal opens in Terminal (`senate attach`); a prompt
/// waiting in one is decided there, by you. Starting a senator, joining and
/// setup stay in the CLI.
struct SessionsView: View {
    let store: CoreStore

    var body: some View {
        let s = store.state
        if s.sessions.isEmpty && s.codex.isEmpty {
            EmptyNote(online: s.online, text: L("sessions.empty"), symbol: "terminal")
        } else {
            List {
                if !s.sessions.isEmpty {
                    Section {
                        ForEach(s.sessions, id: \.sessionId) { session in
                            HostedRow(store: store, session: session)
                                .environment(\.mentionCommittee, session.committee)
                        }
                    } header: {
                        Text(verbatim: L("sessions.hosted"))
                    }
                }
                if !s.codex.isEmpty {
                    Section {
                        ForEach(s.codex) { thread in
                            CodexRow(store: store, thread: thread)
                                .environment(\.mentionCommittee, thread.committee)
                        }
                    } header: {
                        Text(verbatim: L("sessions.codex"))
                    }
                }
            }
            .listStyle(.inset)
        }
    }
}

/// The toolbar's terminal actions: watch every hosted terminal, start Codex.
struct SessionsToolbar: ToolbarContent {
    let store: CoreStore

    var body: some ToolbarContent {
        ToolbarItemGroup {
            Button {
                store.openWatch()
            } label: {
                Label(L("sessions.watch"), systemImage: "rectangle.split.3x1")
            }
            .help(L("sessions.watch_help"))
            .disabled(store.state.sessions.allSatisfy { !$0.running })
            Button {
                store.openTerminal(thread: nil)
            } label: {
                Label(L("codex.new"), systemImage: "plus.rectangle.on.rectangle")
            }
            .help(L("codex.new"))
        }
    }
}

private struct HostedRow: View {
    let store: CoreStore
    let session: HostedSession

    var body: some View {
        let h = session
        let busy = store.isBusy("session:\(h.label)")
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                StatusDot(online: h.running)
                MentionText(h.mention).font(.headline)
                if let committee = h.committee { Tag(text: committee) }
                Tag(text: h.runtime)
                if let model = h.model { Tag(text: model) }
                if h.attached { Tag(text: L("sessions.attached"), tint: .blue) }
                Spacer()
                if busy { ProgressView().controlSize(.small) }
            }
            if let since = h.promptSince {
                PromptBanner(tool: h.promptTool, since: since) {
                    store.attach(h)
                }
            }
            HStack(spacing: 4) {
                Text(verbatim: h.dir)
                    .lineLimit(1)
                    .truncationMode(.middle)
                if h.running, let started = h.startedAt {
                    Text(verbatim: "·")
                    TimedMention(date: started) { L("sessions.up", $0) }
                }
                if let typed = h.typedSecsAgo {
                    Text(verbatim: "·")
                    Text(verbatim: L("sessions.typed", Ago.text(.now.addingTimeInterval(-Double(typed)), now: .now)))
                }
            }
            .font(.callout)
            .foregroundStyle(.secondary)
            HStack {
                if h.running {
                    Button(L("sessions.attach")) { store.attach(h) }
                        .buttonStyle(.borderedProminent)
                        .help(L("sessions.attach_help"))
                    Button(L("sessions.stop")) { Task { await store.stop(h) } }
                        .help(L("sessions.stop_help"))
                } else {
                    Button(L("sessions.restore")) { Task { await store.restore(h) } }
                        .help(L("sessions.restore_help"))
                }
            }
            .disabled(busy || !store.state.online)
        }
        .padding(.vertical, 4)
        .contextMenu {
            Button(L("mention.copy", h.label)) { Mentions.copyText(h.label) }
        }
    }
}

/// A permission prompt waiting in a hosted terminal: what for, since when,
/// and the way to it. The app never answers it.
struct PromptBanner: View {
    let tool: String?
    let since: Date
    let attach: () -> Void

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: "hand.raised.fill")
                .foregroundStyle(.orange)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                TimedMention(date: since) { L("prompt.waiting", $0) }
                    .font(.callout.weight(.medium))
                if let tool {
                    Text(verbatim: tool)
                        .font(.callout.monospaced())
                        .lineLimit(2)
                        .textSelection(.enabled)
                }
            }
            Spacer()
            Button(L("prompt.attach"), action: attach)
                .help(L("prompt.attach_help"))
        }
        .padding(8)
        .background(.orange.opacity(0.12), in: RoundedRectangle(cornerRadius: 8))
        .accessibilityElement(children: .combine)
    }
}

private struct CodexRow: View {
    let store: CoreStore
    let thread: CodexThreadInfo

    var body: some View {
        let c = thread
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                StatusDot(online: c.state == "live")
                Text(verbatim: c.committee ?? L("codex.no_committee")).font(.headline)
                MentionText(c.agent)
                Tag(text: c.stateLabel, tint: c.state == "live" ? .green : .secondary)
                if c.approvals > 0 {
                    Tag(text: L("codex.waiting", Int(c.approvals)), tint: .red)
                }
            }
            TimedMention(date: c.lastSeen) { ago in
                var line = c.cwd
                if let branch = c.branch { line += " · \(branch)" }
                return line + L("codex.seen", ago)
            }
            .font(.callout)
            .foregroundStyle(.secondary)
            HStack {
                Button(L("codex.open")) { store.openTerminal(thread: c.thread) }
                if c.approvals > 0 {
                    Button(L("codex.show_approvals")) { store.section = .approvals }
                }
            }
        }
        .padding(.vertical, 4)
        .contextMenu {
            Button(L("mention.copy", c.thread)) { Mentions.copyText(c.thread) }
        }
    }
}
