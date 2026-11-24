import SwiftUI
import SenateCore

/// One thread waiting on you, opened from the sidebar, the home view, the
/// menu bar or a notification: read it, reply as yourself, take it over, or
/// close it. It is read like a conversation: a centred column, what others
/// said as plain text, what you said in a bubble on your side, and the
/// reply box floating at the foot.
struct ThreadPage: View {
    @Bindable var store: CoreStore
    let row: InboxThread
    @State private var confirmClose = false

    var body: some View {
        // The reply box sits under the conversation rather than over it (two
        // things at one point confuse hit-testing for assistive tools); the
        // conversation fades out above it, so it reads as floating.
        VStack(spacing: 0) {
            Conversation(store: store, row: row)
                .overlay(alignment: .bottom) {
                    LinearGradient(
                        colors: [Theme.surface.opacity(0), Theme.surface],
                        startPoint: .top,
                        endPoint: .bottom
                    )
                    .frame(height: Theme.Space.xl)
                    .allowsHitTesting(false)
                    .accessibilityHidden(true)
                }
            Composer(store: store, row: row)
        }
            .id(row.key)
            .environment(\.mentionCommittee, row.committee)
            // Read again when something new is said in it.
            .task(id: "\(row.key)@\(row.updated)") { await store.loadThread(row) }
            .toolbar { toolbar }
            .confirmationDialog(
                L("inbox.close_title", Mentions.shared.plain(row.title, committee: row.committee)),
                isPresented: $confirmClose
            ) {
                Button(L("inbox.close_confirm"), role: .destructive) { Task { await store.close(row) } }
            } message: {
                Text(verbatim: L("inbox.close_message", row.committee))
            }
    }

    /// At the content's top right: the thread's state, and what can be done
    /// with the thread as a whole.
    @ToolbarContentBuilder private var toolbar: some ToolbarContent {
        let busy = store.isBusy(row.key)
        // The thread's title, where it is, at the toolbar's leading edge.
        ToolbarItem(placement: .navigation) {
            VStack(alignment: .leading, spacing: 0) {
                MentionText(row.title)
                    .font(.headline)
                    .lineLimit(1)
                Text(verbatim: "#\(row.root) · \(row.committee)")
                    .font(.caption)
                    .foregroundStyle(Theme.onSurfaceVariant)
            }
            .padding(.horizontal, Theme.Space.xs)
            .accessibilityElement(children: .combine)
            .accessibilityAddTraits(.isHeader)
        }
        // A title, on the window's ground: no glass behind it.
        .sharedBackgroundVisibility(.hidden)
        ToolbarSpacer(.flexible)
        ToolbarItem(placement: .primaryAction) {
            HStack(spacing: 6) {
                if busy { ProgressView().controlSize(.small) }
                Text(verbatim: row.triage == "needs_human" ? (row.triageLabel ?? row.stateLabel) : row.stateLabel)
                    .font(Theme.Fonts.label)
                    .foregroundStyle(Theme.onSurface)
            }
            .padding(.horizontal, Theme.Space.s)
        }
        ToolbarItemGroup(placement: .primaryAction) {
            Button { Task { await store.take(row) } } label: {
                Label(L("inbox.take"), systemImage: "hand.raised")
                    .labelStyle(.titleAndIcon)
            }
                .help(L("inbox.take_help"))
                .disabled(row.takeover != nil || busy || !store.state.online)
            Menu {
                Button(role: .destructive) { confirmClose = true } label: {
                    Label(L("inbox.close"), systemImage: "xmark.circle")
                }
                    .disabled(busy || !store.state.online)
                Divider()
                Button { store.refresh() } label: { Label(L("toolbar.refresh"), systemImage: "arrow.clockwise") }
                Button(L("mention.copy", "#\(row.root)")) { Mentions.copyText("#\(row.root)") }
            } label: {
                Label(L("thread.more"), systemImage: "ellipsis")
            }
            .menuIndicator(.hidden)
        }
    }
}

/// A thread's right-click menu in the sidebar: what the thread's own buttons do.
struct ThreadMenu: View {
    let store: CoreStore
    let thread: InboxThread

    var body: some View {
        Button(L("inbox.take")) { Task { await store.take(thread) } }
            .disabled(thread.takeover != nil || store.isBusy(thread.key))
        Button(L("inbox.close"), role: .destructive) { Task { await store.close(thread) } }
            .disabled(store.isBusy(thread.key))
    }
}

/// What the thread is (native), then every event in it, the latest at the
/// foot: the window's one web page (ConversationPage), shown for this
/// thread. It shows the latest events, a page more at a time.
private struct Conversation: View {
    let store: CoreStore
    let row: InboxThread

    var body: some View {
        VStack(spacing: 0) {
            ThreadHeader(row: row)
                .frame(maxWidth: Theme.readingWidth, alignment: .leading)
                .padding(.horizontal, Theme.Space.xl)
                .padding(.bottom, Theme.Space.s)
                .frame(maxWidth: .infinity)
            if store.threadView(for: row) != nil {
                ConversationPane(store: store, source: .thread(committee: row.committee, root: row.root))
            } else if let error = store.threadError {
                VStack(alignment: .leading, spacing: Theme.Space.xs) {
                    Text(verbatim: L("inbox.thread_failed"))
                        .font(Theme.Fonts.heading)
                        .foregroundStyle(Theme.onSurface)
                    Text(verbatim: error)
                        .font(Theme.Fonts.text)
                        .foregroundStyle(Theme.onSurfaceVariant)
                        .textSelection(.enabled)
                }
                .frame(maxWidth: Theme.readingWidth, alignment: .leading)
                .padding(Theme.Space.xl)
                Spacer()
            } else {
                ProgressView()
                    .controlSize(.small)
                    .frame(maxWidth: .infinity)
                    .padding(.top, Theme.Space.xl)
                Spacer()
            }
        }
    }
}

/// The thread's title, where it is and why it waits on you.
private struct ThreadHeader: View {
    let row: InboxThread

    var body: some View {
        let t = row
        // Its title is in the window's toolbar; here, where it is and why.
        VStack(alignment: .leading, spacing: Theme.Space.s) {
            VStack(alignment: .leading, spacing: 2) {
                TimedMention(date: t.since) { ago in
                    var line = L("inbox.meta", String(t.root), from(t), t.stateLabel, ago)
                    if let owner = t.owner { line += L("inbox.owner", owner) }
                    if let takeover = t.takeover { line += L("inbox.taken_by", takeover) }
                    if let escalated = t.escalatedTo { line += L("inbox.escalated_to", escalated) }
                    return line
                }
                if let why = why(t) {
                    MentionText(why)
                }
            }
            .font(Theme.Fonts.row)
            .foregroundStyle(Theme.onSurfaceVariant)
            HStack(spacing: Theme.Space.xs) {
                Chip(text: t.committee)
                Chip(text: t.kindLabel)
                if t.escalated { Chip(text: L("inbox.escalated"), tone: .attention) }
                if t.takeover != nil { Chip(text: L("inbox.taken"), tone: .accent) }
            }
        }
        .padding(.top, Theme.Space.l)
    }

    private func from(_ t: InboxThread) -> String {
        if let behalf = t.onBehalfOf { return L("inbox.from_for", t.from, behalf) }
        return t.from
    }

    private func why(_ t: InboxThread) -> String? {
        let parts = [t.triageLabel.map { L("inbox.triage", $0) }, t.reason].compactMap { $0 }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }
}

/// The reply box: answers in the thread as you, `[senator::<you>]`.
private struct Composer: View {
    @Bindable var store: CoreStore
    let row: InboxThread

    var body: some View {
        MessageComposer(
            text: Binding(
                get: { store.drafts[row.key] ?? "" },
                set: { store.drafts[row.key] = $0.isEmpty ? nil : $0 }
            ),
            label: L("inbox.reply"),
            placeholder: L("inbox.reply_placeholder"),
            footnote: L("inbox.replying_as", store.state.person ?? "?", String(row.root)),
            busy: store.isBusy(row.key),
            online: store.state.online
        ) {
            Task { await store.sendReply(row) }
        }
    }
}

/// A box to write in at the foot of a conversation, a little wider than
/// the text above it, and its send button (⌘↩).
struct MessageComposer: View {
    @Binding var text: String
    let label: String
    let placeholder: String
    /// Under the text: who it is sent as, and where.
    let footnote: String
    let busy: Bool
    let online: Bool
    let send: () -> Void
    @FocusState private var focused: Bool

    var body: some View {
        let empty = text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        let ready = !(busy || empty || !online)
        VStack(alignment: .leading, spacing: Theme.Space.xs) {
            TextEditor(text: $text)
                .font(Theme.Fonts.reading)
                .scrollContentBackground(.hidden)
                .frame(minHeight: 24, maxHeight: 190)
                .fixedSize(horizontal: false, vertical: true)
                .focused($focused)
                .accessibilityLabel(label)
                .overlay(alignment: .topLeading) {
                    if text.isEmpty {
                        Text(verbatim: placeholder)
                            .font(Theme.Fonts.reading)
                            .foregroundStyle(Theme.onSurfaceVariant)
                            .padding(.leading, 5)
                            .allowsHitTesting(false)
                            .accessibilityHidden(true)
                    }
                }
            HStack(spacing: Theme.Space.s) {
                Text(verbatim: footnote)
                    .font(Theme.Fonts.meta)
                    .foregroundStyle(Theme.onSurfaceVariant)
                    .lineLimit(1)
                    .padding(.leading, 5)
                Spacer()
                if busy { ProgressView().controlSize(.small) }
                Button(action: send) {
                    Image(systemName: "arrow.up")
                        .font(.system(size: 14, weight: .semibold))
                        .foregroundStyle(ready ? Theme.onPrimary : Theme.onSurfaceVariant)
                        .frame(width: 30, height: 30)
                        .background(ready ? Theme.primary : Theme.containerHigh, in: Circle())
                        .contentShape(Circle())
                }
                .buttonStyle(.plain)
                .keyboardShortcut(.return, modifiers: .command)
                .disabled(!ready)
                .help(L("inbox.send_help"))
                .accessibilityLabel(L("inbox.send"))
            }
        }
        .padding(.horizontal, Theme.Space.m)
        .padding(.top, 14)
        .padding(.bottom, 10)
        .background(Theme.bright, in: RoundedRectangle(cornerRadius: Theme.Radius.composer))
        .overlay {
            RoundedRectangle(cornerRadius: Theme.Radius.composer)
                .strokeBorder(Theme.outline, lineWidth: 0.5)
                .allowsHitTesting(false)
                .accessibilityHidden(true)
        }
        .shadow(color: .black.opacity(0.07), radius: 14, y: 3)
        .frame(maxWidth: Theme.composerWidth)
        .padding(.horizontal, Theme.Space.s)
        .padding(.top, Theme.Space.xs)
        .padding(.bottom, 10)
        .frame(maxWidth: .infinity)
    }
}
