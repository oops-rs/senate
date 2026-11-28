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
        content
            .toolbar {
                ToolbarSpacer(.flexible)
                ToolbarItem(placement: .primaryAction) { RefreshButton(store: store) }
            }
    }

    @ViewBuilder private var content: some View {
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

/// Something awaiting the Speaker (a question, a note, a request between
/// committees): the same card as what is put to you, opening the item,
/// where it is answered, forwarded or refused, or read when it waits on
/// someone else.
private struct EntryCard: View {
    let store: CoreStore
    let item: DocketEntry
    @State private var hovered = false

    var body: some View {
        // `addressed` is what the kind's chip already says; the others say
        // whom or what it waits on.
        let telling = item.awaiting != "addressed"
        Button {
            store.place = .desk(item)
        } label: {
            VStack(alignment: .leading, spacing: Theme.Space.s) {
                HStack(spacing: 6) {
                    Chip(text: item.kindLabel, tone: .accent)
                    Chip(text: item.committee)
                    if telling {
                        Chip(text: L("docket.\(item.awaiting)"), tone: item.awaiting == "away" ? .attention : .neutral)
                    }
                    Spacer(minLength: Theme.Space.s)
                    TimedMention(date: item.since) { ago in
                        L("docket.meta", String(item.seq), item.by, ago)
                    }
                    .font(Theme.Fonts.meta)
                    .foregroundStyle(Theme.onSurfaceVariant)
                    .lineLimit(1)
                }
                MentionText(item.body)
                    .font(Theme.Fonts.heading)
                    .foregroundStyle(Theme.onSurface)
                    .lineLimit(4)
                    .multilineTextAlignment(.leading)
                if telling {
                    MentionText(item.why)
                        .font(Theme.Fonts.text)
                        .foregroundStyle(Theme.onSurfaceVariant)
                        .lineLimit(2)
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
        .help(L("desk.open_item_help"))
        .accessibilityIdentifier("desk-open-\(item.id)")
    }
}

struct DeskEntryDetail: View {
    let store: CoreStore
    let item: DocketEntry

    private struct Attempt {
        let id: String
        let choice: DeskChoice
    }

    @State private var detail: DeskItemView?
    @State private var loadError: String?
    @State private var actionError: String?
    @State private var attempt: Attempt?
    @State private var outcome: DeskOutcomeView?
    @State private var busy = false
    @State private var refusing = false
    @State private var closing = false
    @State private var reason = ""
    @State private var answer = ""

    var body: some View {
        ReadingColumn {
            if let detail {
                // What it is and how it stands, when there is something to say.
                let said = detail.origin != nil || outcome != nil || actionError != nil || busy
                    || (detail.actions.contains(.forward) && detail.destination != nil)
                    || (detail.actions.isEmpty && (detail.waitsOn ?? detail.blocked) != nil)
                if said {
                VStack(alignment: .leading, spacing: Theme.Space.m) {
                    if let origin = detail.origin {
                        Text(verbatim: L("desk.origin", origin)).font(Theme.Fonts.meta)
                            .foregroundStyle(Theme.onSurfaceVariant)
                    }
                    // Where it would go: only when it can be forwarded there.
                    if outcome == nil, detail.actions.contains(.forward),
                       let destination = detail.destination, let label = detail.destinationLabel {
                        Text(verbatim: L("desk.destination", label)).font(Theme.Fonts.text)
                            .foregroundStyle(Theme.onSurface)
                            .help(destination)
                            .accessibilityLabel(L("desk.destination", destination))
                    }
                    controls(detail)
                }
                .card()
                .environment(\.mentionCommittee, item.committee)
                }
                ForEach(detail.thread.events, id: \.seq) { event in
                    VStack(alignment: .leading, spacing: Theme.Space.s) {
                        HStack {
                            Chip(text: event.kindLabel)
                            MentionText(event.by)
                            Spacer()
                            Text(verbatim: "#\(event.seq)").font(Theme.Fonts.meta)
                        }
                        if let via = event.via {
                            MentionText(via)
                                .font(Theme.Fonts.meta)
                                .foregroundStyle(Theme.onSurfaceVariant)
                        }
                        MentionText(event.body)
                            .font(Theme.Fonts.text)
                            .textSelection(.enabled)
                    }
                    .card()
                    .environment(\.mentionCommittee, item.committee)
                }
            } else if let loadError {
                VStack(alignment: .leading, spacing: Theme.Space.s) {
                    Text(verbatim: loadError).foregroundStyle(Theme.onSurface)
                    Button(L("desk.retry")) { Task { await load() } }
                        .buttonStyle(.pill)
                }.card()
            } else {
                ProgressView().accessibilityLabel(L("desk.loading"))
            }
        }
        .safeAreaInset(edge: .bottom, spacing: 0) {
            // Awaiting an answer: answered here, as the Speaker.
            if let detail, outcome == nil, detail.actions.contains(.answer) {
                MessageComposer(
                    text: $answer,
                    label: L("desk.answer"),
                    placeholder: L("desk.answer_placeholder"),
                    hint: L("desk.answer_hint"),
                    busy: busy,
                    online: store.state.online
                ) {
                    Task { await submit(.answer(body: answer)) }
                }
            }
        }
        .task { await load() }
        .toolbar { toolbar }
        .sheet(isPresented: $refusing) { refusalSheet }
        .sheet(isPresented: $closing) { closeSheet }
        #if DEBUG
        .onReceive(NotificationCenter.default.publisher(for: Snapshot.deskPresentation)) { note in
            if let scene = note.object as? Snapshot.DeskPresentation { showSnapshot(scene) }
        }
        #endif
    }

    private var refusalSheet: some View {
        VStack(alignment: .leading, spacing: Theme.Space.m) {
            Text(verbatim: L("desk.refuse_heading")).font(Theme.Fonts.heading)
            TextField(L("desk.reason"), text: $reason, axis: .vertical)
                .accessibilityIdentifier("desk-reason")
            HStack(spacing: Theme.Space.s) {
                Spacer()
                Button(L("desk.cancel")) { refusing = false }
                    .buttonStyle(.pill)
                    .keyboardShortcut(.cancelAction)
                Button(L("desk.confirm_refuse")) {
                    refusing = false
                    Task { await submit(.refuse(reason: reason)) }
                }
                .buttonStyle(.pill(.danger))
                .disabled(busy || !store.state.online || reason.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(Theme.Space.xl)
        .frame(width: 420)
    }

    /// Close it with no answer: why, if the person says.
    private var closeSheet: some View {
        VStack(alignment: .leading, spacing: Theme.Space.m) {
            Text(verbatim: L("desk.close_heading")).font(Theme.Fonts.heading)
            TextField(L("desk.close_reason"), text: $reason, axis: .vertical)
                .accessibilityIdentifier("desk-close-reason")
            HStack(spacing: Theme.Space.s) {
                Spacer()
                Button(L("desk.cancel")) { closing = false }
                    .buttonStyle(.pill)
                    .keyboardShortcut(.cancelAction)
                Button(L("desk.confirm_close")) {
                    closing = false
                    let why = reason.trimmingCharacters(in: .whitespacesAndNewlines)
                    Task { await submit(.close(reason: why.isEmpty ? nil : why)) }
                }
                .buttonStyle(.pill(.danger))
                .disabled(busy || !store.state.online)
            }
        }
        .padding(Theme.Space.xl)
        .frame(width: 420)
    }

    #if DEBUG
    private func showSnapshot(_ scene: Snapshot.DeskPresentation) {
        switch scene {
        case .delivered:
            guard let detail, let destination = detail.destination,
                  let label = detail.destinationLabel else { return }
            outcome = DeskOutcomeView(kind: .delivered, delivered: true, destination: destination,
                                      destinationLabel: label, committee: "payments", seq: 75, reason: nil)
        case .failed:
            attempt = Attempt(id: "", choice: .forward)
            actionError = "The destination is temporarily unavailable."
        case .refusing:
            outcome = nil
            refusing = true
        case .dismissRefusal:
            refusing = false
        }
    }
    #endif

    @ViewBuilder private func controls(_ detail: DeskItemView) -> some View {
        if let outcome {
            switch outcome.kind {
            case .delivered, .refused:
                let key = outcome.kind == .delivered ? "desk.delivered" : "desk.refused"
                Text(verbatim: L(key, outcome.destinationLabel, String(outcome.seq)))
                    .help(outcome.destination)
                    .accessibilityLabel(L(key, outcome.destination, String(outcome.seq)))
                    .accessibilityIdentifier("desk-outcome")
            case .answered:
                Text(verbatim: L("desk.answered", outcome.committee, String(outcome.seq)))
                    .accessibilityIdentifier("desk-outcome")
            case .closed:
                Text(verbatim: L("desk.closed", String(outcome.seq)))
                    .accessibilityIdentifier("desk-outcome")
            }
        } else if let actionError {
            Text(verbatim: actionError)
                .foregroundStyle(Theme.attention)
                .textSelection(.enabled)
                .accessibilityIdentifier("desk-action-error")
            if let attempt {
                Button(L("desk.retry")) { Task { await run(attempt) } }
                    .buttonStyle(.pill)
                    .disabled(busy || !store.state.online)
            }
        } else if detail.actions.isEmpty, let waits = detail.waitsOn ?? detail.blocked {
            // Nothing to do from here: whom or what it waits on.
            Text(verbatim: waits).foregroundStyle(Theme.onSurfaceVariant)
                .accessibilityIdentifier("desk-read-only")
        } else if busy {
            ProgressView().controlSize(.small)
        }
    }

    /// What the item is; what can be done with it now (Forward and Refuse
    /// for a request between committees; answering is the composer below);
    /// Close, the original thread and the committee in its menu.
    @ToolbarContentBuilder private var toolbar: some ToolbarContent {
        ToolbarItem(placement: .navigation) {
            Text(verbatim: DeskEntryDetail.title(item, detail))
                .font(.headline)
                .lineLimit(1)
                .padding(.horizontal, Theme.Space.xs)
                .accessibilityAddTraits(.isHeader)
        }
        .sharedBackgroundVisibility(.hidden)
        ToolbarSpacer(.flexible)
        if let detail, outcome == nil, actionError == nil,
           detail.actions.contains(.forwardAndAuthorize) || detail.actions.contains(.forward) || detail.actions.contains(.refuse) {
            ToolbarItemGroup(placement: .primaryAction) {
                if detail.actions.contains(.forwardAndAuthorize) {
                Button { Task { await submit(.forwardAndAuthorize(note: nil)) } } label: {
                    Label(L("desk.forward_authorize_title"), systemImage: "checkmark.seal.fill")
                }
                .buttonStyle(.borderedProminent)
                .accessibilityIdentifier("desk-forward-authorize")
                .help(L("desk.forward_authorize_title"))
                .disabled(busy || !store.state.online)
                }
                if detail.actions.contains(.forward) {
                Button { Task { await submit(.forward) } } label: {
                    // Send, not Share's arrow: it goes on to its addressee without authorizing execution.
                    Label(L("desk.forward_only_title"), systemImage: "paperplane")
                }
                .buttonStyle(.bordered)
                .accessibilityIdentifier("desk-forward")
                .help(L("desk.forward_only_title"))
                .disabled(busy || !store.state.online)
                }
                if detail.actions.contains(.refuse) {
                Button { refusing = true } label: {
                    // A no-entry sign, unlike Take's raised hand.
                    Label(L("desk.refuse_title"), systemImage: "nosign")
                }
                .help(L("desk.refuse_title"))
                .disabled(busy || !store.state.online)
                }
            }
        }
        ToolbarItem(placement: .primaryAction) {
            Menu {
                if let detail, outcome == nil, detail.actions.contains(.close) {
                    Button(role: .destructive) { reason = ""; closing = true } label: {
                        Label(L("desk.close"), systemImage: "archivebox")
                    }
                    .disabled(busy || !store.state.online)
                    Divider()
                }
                if let detail {
                    Button {
                        store.openThread(committee: detail.thread.committee, root: detail.thread.root)
                    } label: {
                        Label(L("desk.original_thread"), systemImage: "bubble.left.and.bubble.right")
                    }
                }
                Button { store.place = .committee(item.committee) } label: {
                    Label(L("committees.open", item.committee), systemImage: "person.3")
                }
            } label: {
                Label(L("thread.more"), systemImage: "ellipsis")
            }
            .menuIndicator(.hidden)
        }
    }

    /// The page's title by what the item is, as its card's chip says: a
    /// question or note to the Speaker, or a request for the committee it
    /// would be forwarded to.
    static func title(_ item: DocketEntry, _ detail: DeskItemView? = nil) -> String {
        let at = "\(item.committee) #\(item.seq)"
        if let detail,
           detail.actions.contains(.forward) || detail.actions.contains(.forwardAndAuthorize),
           let to = detail.destinationLabel {
            return L("desk.title_request_for", to, at)
        }
        let kind = item.kindLabel.prefix(1).uppercased() + item.kindLabel.dropFirst()
        return L("desk.title_to_speaker", kind, at)
    }

    private func load() async {
        loadError = nil
        do { detail = try await store.deskItem(item) }
        catch { loadError = describe(error) }
    }

    private func submit(_ choice: DeskChoice) async {
        guard !busy else { return }
        do {
            let value = Attempt(id: try store.newDeskActionId(), choice: choice)
            attempt = value
            await run(value)
        } catch { actionError = describe(error) }
    }

    private func run(_ attempt: Attempt) async {
        guard !busy, let detail else { return }
        busy = true
        defer { busy = false }
        do {
            outcome = try await store.decideDeskItem(item, itemId: detail.eventId, id: attempt.id, choice: attempt.choice)
            actionError = nil
            if case .answer = attempt.choice { answer = "" }
        } catch { actionError = describe(error) }
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
