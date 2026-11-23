import SwiftUI
import SenateCore

/// Everything waiting for your yes or no, one list in three parts: the
/// permission prompts in hosted terminals (answered here; any other choice
/// by attaching), Codex approval requests the daemon relays (the first
/// answer wins across the TUI, this app and any chat platform), and the
/// Speaker's desk: what is put to you (escalations, requests and decisions
/// waiting on you) and what awaits the Speaker your agent holds. Each card
/// names its session or committee and opens it.
struct ApprovalsView: View {
    let store: CoreStore

    var body: some View {
        let prompts = store.prompts
        let approvals = store.state.approvals
        let desk = store.deskThreads
        let docket = store.docketEntries
        if prompts.isEmpty && approvals.isEmpty && desk.isEmpty && docket.isEmpty {
            EmptyState(text: L("approvals.empty"))
        } else {
            ReadingColumn {
                PageHeading(text: L("tab.approvals"))
                if !prompts.isEmpty {
                    SectionHeader(L("approvals.terminals"))
                    ForEach(prompts, id: \.sessionId) { session in
                        VStack(alignment: .leading, spacing: Theme.Space.m) {
                            HStack(spacing: 6) {
                                Button {
                                    store.place = .session(session.sessionId)
                                } label: {
                                    HStack(spacing: 4) {
                                        MentionText(session.mention)
                                            .font(Theme.Fonts.heading)
                                        Image(systemName: "chevron.right")
                                            .font(.system(size: 10, weight: .semibold))
                                            .foregroundStyle(Theme.onSurfaceVariant)
                                            .accessibilityHidden(true)
                                    }
                                }
                                .buttonStyle(.plain)
                                .help(L("approvals.open_session"))
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
                if !desk.isEmpty || !docket.isEmpty {
                    SectionHeader(L("approvals.desk"))
                    ForEach(desk) { thread in
                        DeskCard(store: store, thread: thread)
                    }
                    ForEach(docket) { item in
                        EntryCard(store: store, item: item)
                            .environment(\.mentionCommittee, item.committee)
                    }
                    if !docket.isEmpty {
                        Text(verbatim: L("docket.note"))
                            .font(Theme.Fonts.meta)
                            .foregroundStyle(Theme.onSurfaceVariant)
                            .fixedSize(horizontal: false, vertical: true)
                            .padding(.horizontal, Theme.Space.xs)
                    }
                }
            }
        }
    }
}

/// Something put to you: an escalation, a request or a decision waiting on
/// you. It opens its thread, where you answer.
private struct DeskCard: View {
    let store: CoreStore
    let thread: InboxThread
    @State private var hovered = false

    var body: some View {
        let t = thread
        Button {
            store.place = .thread(t.key)
        } label: {
            VStack(alignment: .leading, spacing: Theme.Space.s) {
                HStack(spacing: 6) {
                    Chip(text: t.escalated ? L("inbox.escalated") : t.kindLabel, tone: .attention)
                    Chip(text: t.committee)
                    Spacer(minLength: Theme.Space.s)
                    TimedMention(date: t.since) { ago in L("approvals.desk_meta", t.asker, ago) }
                        .font(Theme.Fonts.meta)
                        .foregroundStyle(Theme.onSurfaceVariant)
                        .lineLimit(1)
                }
                MentionText(t.title)
                    .font(Theme.Fonts.heading)
                    .foregroundStyle(Theme.onSurface)
                    .multilineTextAlignment(.leading)
                if let line = t.newsLine ?? t.reason {
                    MentionText(line)
                        .font(Theme.Fonts.text)
                        .foregroundStyle(Theme.onSurfaceVariant)
                        .lineLimit(3)
                        .multilineTextAlignment(.leading)
                }
            }
            .padding(Theme.Space.l)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(hovered ? Theme.containerHigh : Theme.container,
                        in: RoundedRectangle(cornerRadius: Theme.Radius.card))
            .contentShape(RoundedRectangle(cornerRadius: Theme.Radius.card))
        }
        .buttonStyle(.plain)
        .onHover { hovered = $0 }
        .environment(\.mentionCommittee, t.committee)
        .help(L("approvals.open_thread"))
        .contextMenu { ThreadMenu(store: store, thread: t) }
    }
}

/// One item awaiting the Speaker your agent holds: what kind, what the
/// Speaker is to do with it, what was said, and why it is here. Read-only:
/// routing and covering are the presiding agent's own tools. Its committee
/// opens.
private struct EntryCard: View {
    let store: CoreStore
    let item: DocketEntry

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.s) {
            HStack(spacing: 6) {
                Chip(text: L("waiting.to_speaker"), tone: .accent)
                Button {
                    store.place = .committee(item.committee)
                } label: {
                    Chip(text: item.committee)
                }
                .buttonStyle(.plain)
                .help(L("committees.open", item.committee))
                Chip(text: L("docket.\(item.awaiting)"), tone: item.awaiting == "away" ? .attention : .neutral)
                Spacer(minLength: Theme.Space.s)
                TimedMention(date: item.since) { ago in
                    L("docket.meta", String(item.seq), item.by, ago)
                }
                .font(Theme.Fonts.meta)
                .foregroundStyle(Theme.onSurfaceVariant)
                .lineLimit(1)
            }
            MentionText(item.body)
                .font(Theme.Fonts.text)
                .foregroundStyle(Theme.onSurface)
                .lineLimit(4)
                .textSelection(.enabled)
            MentionText(item.why)
                .font(Theme.Fonts.row)
                .foregroundStyle(Theme.onSurfaceVariant)
        }
        .card()
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
                    if let session = store.session(a.thread) {
                        Button {
                            store.place = .session(session.sessionId)
                        } label: {
                            HStack(spacing: 4) {
                                Text(verbatim: session.displayName)
                                Image(systemName: "chevron.right").font(.system(size: 9, weight: .semibold))
                            }
                            .font(Theme.Fonts.label)
                            .foregroundStyle(Theme.agent)
                        }
                        .buttonStyle(.plain)
                        .help(L("approvals.open_session"))
                    }
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
