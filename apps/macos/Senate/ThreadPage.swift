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
        // Everything the thread is, once, at the toolbar's leading edge:
        // its title; where it is, who it is from and what kind; its state
        // and how long it has been so. The title gives way first.
        ToolbarItem(placement: .navigation) {
            ThreadTitle(row: row, busy: busy)
        }
        // A title, on the window's ground: no glass behind it.
        .sharedBackgroundVisibility(.hidden)
        ToolbarSpacer(.flexible)
        ToolbarItemGroup(placement: .primaryAction) {
            Button { Task { await store.take(row) } } label: {
                Label(L("inbox.take"), systemImage: "hand.raised")
            }
                // The page's one decisive action: tinted.
                .buttonStyle(.borderedProminent)
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

/// The thread in the toolbar: its title, then "#root · committee · from
/// X · kind" (and who took it over), then its state with its age, or what
/// a person is needed for; why, in the state's tooltip.
private struct ThreadTitle: View {
    let row: InboxThread
    let busy: Bool

    var body: some View {
        let t = row
        HStack(alignment: .center, spacing: Theme.Space.s) {
            VStack(alignment: .leading, spacing: 0) {
                MentionText(t.title)
                    .font(.headline)
                    .lineLimit(1)
                    .truncationMode(.tail)
                HStack(spacing: 4) {
                    Text(verbatim: "#\(t.root) · \(t.committee) · " + L("thread.from"))
                    MentionText(t.onBehalfOf.map { L("inbox.from_for", t.from, $0) } ?? t.from)
                    Text(verbatim: "· \(t.kindLabel)")
                    if let takeover = t.takeover {
                        Text(verbatim: "· " + L("thread.taken_by", takeover))
                    }
                }
                .font(.caption)
                .foregroundStyle(Theme.onSurfaceVariant)
                .lineLimit(1)
            }
            .layoutPriority(0)
            // The state is never cut: the title is.
            HStack(spacing: 4) {
                if busy { ProgressView().controlSize(.mini) }
                Text(verbatim: state(t))
                    .font(Theme.Fonts.label)
                    .foregroundStyle(t.triage == "needs_human" || t.escalated ? Theme.attention : Theme.onSurface)
                Text(verbatim: "·").foregroundStyle(Theme.onSurfaceVariant)
                Ago(date: t.since)
                    .font(Theme.Fonts.label)
                    .foregroundStyle(Theme.onSurfaceVariant)
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 3)
            .background(Theme.containerHigh, in: Capsule())
            .fixedSize()
            .layoutPriority(1)
            .help(why(t) ?? "")
        }
        .padding(.horizontal, Theme.Space.xs)
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(.isHeader)
    }

    private func state(_ t: InboxThread) -> String {
        t.triage == "needs_human" ? (t.triageLabel ?? t.stateLabel) : t.stateLabel
    }

    private func why(_ t: InboxThread) -> String? {
        var parts = [t.reason].compactMap { $0 }
        if let escalated = t.escalatedTo { parts.append(L("thread.escalated_to", escalated)) }
        if let owner = t.owner { parts.append(L("thread.owner", owner)) }
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
            hint: L("inbox.replying_as", store.state.person ?? "?", String(row.root)),
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
    /// Under the text, only when it says what the reader cannot otherwise
    /// see (the Secretary's first message starts it); else nil, and what
    /// it would have said is VoiceOver's hint.
    var footnote: String?
    var hint: String?
    let busy: Bool
    let online: Bool
    let send: () -> Void
    @FocusState private var focused: Bool

    /// Whether ⌘↩ (or the button) sends now: something to send, not
    /// already sending, online, and no input method mid-composition.
    nonisolated static func canSend(_ text: String, busy: Bool, online: Bool, composing: Bool) -> Bool {
        !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && !busy && online && !composing
    }

    /// An input method is composing in the focused text (zh input): ⌘↩
    /// is its, not ours.
    private static var composing: Bool {
        (NSApp.keyWindow?.firstResponder as? NSTextView)?.hasMarkedText() ?? false
    }

    private func trySend() {
        guard Self.canSend(text, busy: busy, online: online, composing: Self.composing) else { return }
        send()
    }

    var body: some View {
        let ready = Self.canSend(text, busy: busy, online: online, composing: false)
        VStack(alignment: .leading, spacing: Theme.Space.xs) {
            TextEditor(text: $text)
                .font(Theme.Fonts.reading)
                .scrollContentBackground(.hidden)
                .frame(minHeight: 24, maxHeight: 190)
                .fixedSize(horizontal: false, vertical: true)
                .focused($focused)
                .accessibilityLabel(label)
                .accessibilityHint(hint ?? footnote ?? "")
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
                if let footnote {
                    Text(verbatim: footnote)
                        .font(Theme.Fonts.meta)
                        .foregroundStyle(Theme.onSurfaceVariant)
                        .lineLimit(1)
                        .padding(.leading, 5)
                }
                Spacer()
                if busy { ProgressView().controlSize(.small) }
                Button(action: trySend) {
                    // The key equivalent beside the icon, dimmer, as system
                    // buttons show theirs.
                    HStack(spacing: 5) {
                        Text(verbatim: "⌘↩")
                            .font(.system(size: 11, weight: .medium))
                            .opacity(0.65)
                        Image(systemName: "arrow.up")
                            .font(.system(size: 13, weight: .semibold))
                    }
                    .foregroundStyle(ready ? Theme.onPrimary : Theme.onSurfaceVariant)
                    .padding(.horizontal, 10)
                    .frame(height: 30)
                    .background(ready ? Theme.primary : Theme.containerHigh, in: Capsule())
                    .contentShape(Capsule())
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
