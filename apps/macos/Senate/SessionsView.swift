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
            EmptyState(text: L("sessions.empty"))
        } else {
            ReadingColumn {
                PageHeading(text: L("tab.sessions"))
                if !s.sessions.isEmpty {
                    SectionHeader(L("sessions.hosted"))
                    ForEach(s.sessions, id: \.sessionId) { session in
                        HostedCard(store: store, session: session)
                            .environment(\.mentionCommittee, session.committee)
                    }
                }
                if !s.codex.isEmpty {
                    SectionHeader(L("sessions.codex"))
                    ForEach(s.codex) { thread in
                        CodexCard(store: store, thread: thread)
                            .environment(\.mentionCommittee, thread.committee)
                    }
                }
            }
        }
    }
}

/// At the content's top right: watch every hosted terminal, start Codex.
struct SessionsToolbar: ToolbarContent {
    let store: CoreStore

    var body: some ToolbarContent {
        // With no title drawn, this is what keeps them at the trailing edge.
        ToolbarSpacer(.flexible)
        ToolbarItemGroup(placement: .primaryAction) {
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

/// One hosted session: who and where, what it runs, and what can be done
/// with it at the trailing edge. A prompt waiting in it opens below.
private struct HostedCard: View {
    let store: CoreStore
    let session: HostedSession

    var body: some View {
        let h = session
        let busy = store.isBusy("session:\(h.label)")
        VStack(alignment: .leading, spacing: Theme.Space.m) {
            HStack(alignment: .top, spacing: Theme.Space.m) {
                VStack(alignment: .leading, spacing: 6) {
                    HStack(spacing: 6) {
                        StatusDot(online: h.running)
                        MentionText(h.mention)
                            .font(Theme.Fonts.heading)
                        if let committee = h.committee { Chip(text: committee) }
                        Chip(text: h.runtime)
                        if let model = h.model { Chip(text: model) }
                        if h.attached { Chip(text: L("sessions.attached"), tone: .accent) }
                        if h.paused { Chip(text: L("sessions.paused"), tone: .attention) }
                    }
                    MetaLine {
                        Text(verbatim: h.dir)
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
                }
                Spacer(minLength: Theme.Space.s)
                HStack(spacing: Theme.Space.s) {
                    if busy { ProgressView().controlSize(.small) }
                    if h.running {
                        Button(L("sessions.attach")) { store.attach(h) }
                            .help(L("sessions.attach_help"))
                        Button(L("sessions.stop")) { Task { await store.stop(h) } }
                            .help(L("sessions.stop_help"))
                    } else if h.paused {
                        Button(L("sessions.resume")) { Task { await store.resume(h) } }
                            .help(L("sessions.resume_help"))
                    } else {
                        Button(L("sessions.restore")) { Task { await store.restore(h) } }
                            .help(L("sessions.restore_help"))
                    }
                }
                .buttonStyle(.pill)
                .disabled(busy || !store.state.online)
            }
            if h.promptSince != nil {
                PromptBlock(store: store, session: h, tinted: true)
            }
        }
        .card()
        .contextMenu {
            Button(L("mention.copy", h.label)) { Mentions.copyText(h.label) }
        }
    }
}

/// One Codex thread registered with the daemon.
private struct CodexCard: View {
    let store: CoreStore
    let thread: CodexThreadInfo

    var body: some View {
        let c = thread
        HStack(alignment: .top, spacing: Theme.Space.m) {
            VStack(alignment: .leading, spacing: 6) {
                HStack(spacing: 6) {
                    StatusDot(online: c.state == "live")
                    Text(verbatim: c.committee ?? L("codex.no_committee"))
                        .font(Theme.Fonts.heading)
                        .foregroundStyle(Theme.onSurface)
                    MentionText(c.agent)
                        .font(Theme.Fonts.text)
                    Chip(text: c.stateLabel, tone: c.state == "live" ? .success : .neutral)
                    if c.approvals > 0 {
                        Chip(text: L("codex.waiting", Int(c.approvals)), tone: .attention)
                    }
                }
                TimedMention(date: c.lastSeen) { ago in
                    var line = c.cwd
                    if let branch = c.branch { line += " · \(branch)" }
                    return line + L("codex.seen", ago)
                }
                .font(Theme.Fonts.row)
                .foregroundStyle(Theme.onSurfaceVariant)
                .lineLimit(1)
                .truncationMode(.middle)
            }
            Spacer(minLength: Theme.Space.s)
            HStack(spacing: Theme.Space.s) {
                if c.approvals > 0 {
                    Button(L("codex.show_approvals")) { store.place = .section(.approvals) }
                }
                Button(L("codex.open")) { store.openTerminal(thread: c.thread) }
            }
            .buttonStyle(.pill)
        }
        .card()
        .contextMenu {
            Button(L("mention.copy", c.thread)) { Mentions.copyText(c.thread) }
        }
    }
}
