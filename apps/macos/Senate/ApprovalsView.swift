import SwiftUI
import SenateCore

/// What waits for your permission: prompts in hosted terminals (a
/// permission prompt answered here, any other choice by attaching) and
/// Codex approval requests the daemon relays
/// (answered here; the first answer wins across the TUI, this app and any
/// chat platform). One card each: what is asked, then the ways to answer.
struct ApprovalsView: View {
    let store: CoreStore

    var body: some View {
        let prompts = store.prompts
        let approvals = store.state.approvals
        if prompts.isEmpty && approvals.isEmpty {
            EmptyState(text: L("approvals.empty"))
        } else {
            ReadingColumn {
                PageHeading(text: L("tab.approvals"))
                if !prompts.isEmpty {
                    SectionHeader(L("approvals.terminals"))
                    ForEach(prompts, id: \.sessionId) { session in
                        VStack(alignment: .leading, spacing: Theme.Space.m) {
                            HStack(spacing: 6) {
                                MentionText(session.mention)
                                    .font(Theme.Fonts.heading)
                                if let committee = session.committee { Chip(text: committee) }
                                Chip(text: session.runtime)
                            }
                            PromptBlock(store: store, session: session)
                        }
                        .card()
                        .environment(\.mentionCommittee, session.committee)
                    }
                    Text(verbatim: L("approvals.terminals_note"))
                        .font(Theme.Fonts.meta)
                        .foregroundStyle(Theme.onSurfaceVariant)
                        .fixedSize(horizontal: false, vertical: true)
                        .padding(.horizontal, Theme.Space.xs)
                }
                if !approvals.isEmpty {
                    SectionHeader(L("approvals.codex"))
                    ForEach(approvals) { approval in
                        ApprovalCard(store: store, approval: approval)
                    }
                }
            }
        }
    }
}

/// A permission prompt waiting in a hosted terminal: since when, what for,
/// and the ways to decide it. A Codex or Claude Code permission prompt is
/// answered from here, as the person (`senate session approve`); a choice
/// only seen on the screen is decided by attaching. Inside a session's own
/// card it is `tinted`, so what waits on you stands out from the rest.
struct PromptBlock: View {
    let store: CoreStore
    let session: HostedSession
    var tinted = false

    var body: some View {
        let busy = store.isBusy("session:\(session.label)")
        VStack(alignment: .leading, spacing: Theme.Space.m) {
            VStack(alignment: .leading, spacing: Theme.Space.s) {
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    Image(systemName: "hand.raised.fill")
                        .foregroundStyle(Theme.attention)
                        .accessibilityHidden(true)
                    if let since = session.promptSince {
                        TimedMention(date: since) { L("prompt.waiting", $0) }
                            .font(Theme.Fonts.text)
                            .foregroundStyle(Theme.onSurface)
                    }
                }
                if let tool = session.promptTool {
                    CodeBlock(text: tool, lines: 3)
                }
            }
            .accessibilityElement(children: .combine)
            HStack(spacing: Theme.Space.s) {
                if session.promptAnswerable {
                    Button(L("prompt.allow")) { decide(.allow) }
                        .buttonStyle(.pill(.filled))
                    Button(L("prompt.always")) { decide(.allowForSession) }
                        .buttonStyle(.pill)
                        .help(L("prompt.always_help"))
                    Button(L("prompt.deny")) { decide(.deny) }
                        .buttonStyle(.pill(.danger))
                }
                Button(L("prompt.attach")) { store.attach(session) }
                    .buttonStyle(.pill)
                    .help(L("prompt.attach_help"))
                if busy { ProgressView().controlSize(.small) }
            }
            .disabled(busy || !store.state.online)
        }
        .padding(tinted ? Theme.Space.m : 0)
        .background(
            tinted ? Theme.attentionContainer : .clear,
            in: RoundedRectangle(cornerRadius: Theme.Radius.code)
        )
    }

    private func decide(_ decision: ApprovalDecision) {
        Task { await store.decide(session, decision) }
    }
}

/// One Codex approval request.
private struct ApprovalCard: View {
    let store: CoreStore
    let approval: ApprovalRequest

    var body: some View {
        let a = approval
        let busy = store.isBusy("approval:\(a.id)")
        VStack(alignment: .leading, spacing: Theme.Space.m) {
            VStack(alignment: .leading, spacing: Theme.Space.s) {
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    Text(verbatim: a.summary)
                        .font(Theme.Fonts.heading)
                        .foregroundStyle(Theme.onSurface)
                        .lineLimit(3)
                    Chip(text: words(a.kind))
                    Spacer(minLength: 0)
                }
                if let command = a.command {
                    CodeBlock(text: command)
                }
                VStack(alignment: .leading, spacing: 2) {
                    TimedMention(date: a.receivedAt) { ago in L("approvals.meta", a.thread, ago) }
                    if let cwd = a.cwd {
                        Text(verbatim: L("approvals.in", cwd))
                    }
                    if !a.files.isEmpty {
                        Text(verbatim: L("approvals.files", a.files.joined(separator: ", ")))
                    }
                }
                .font(Theme.Fonts.row)
                .foregroundStyle(Theme.onSurfaceVariant)
                if let reason = a.reason {
                    Text(verbatim: reason)
                        .font(Theme.Fonts.text)
                        .foregroundStyle(Theme.onSurface)
                }
            }
            HStack(spacing: Theme.Space.s) {
                Button(L("approvals.allow")) { decide(.allow) }
                    .buttonStyle(.pill(.filled))
                Button(L("approvals.allow_session")) { decide(.allowForSession) }
                    .buttonStyle(.pill)
                Button(L("approvals.deny")) { decide(.deny) }
                    .buttonStyle(.pill(.danger))
                Menu {
                    Button(L("approvals.deny_stop"), role: .destructive) { decide(.denyAndStop) }
                } label: {
                    Image(systemName: "ellipsis")
                        .frame(width: 30, height: 30)
                        .background(Theme.containerHigh, in: Circle())
                        .contentShape(Circle())
                }
                .menuStyle(.button)
                .buttonStyle(.plain)
                .menuIndicator(.hidden)
                .fixedSize()
                .help(L("approvals.deny_stop_help"))
                .accessibilityLabel(L("thread.more"))
                if busy { ProgressView().controlSize(.small) }
            }
            // The whole card waits while it is answered: the first answer wins.
            .disabled(busy || !store.state.online)
        }
        .card()
    }

    private func decide(_ decision: ApprovalDecision) {
        Task { await store.decide(approval, decision) }
    }
}
