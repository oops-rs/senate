import SwiftUI

/// The open threads waiting on you, merged across your agents or narrowed to
/// one: reply as yourself, take over, or close.
struct InboxView: View {
    @Bindable var store: CoreStore

    var body: some View {
        VStack(spacing: 0) {
            if store.inboxAgents.count > 1 {
                HStack {
                    Picker(L("inbox.agent"), selection: $store.agentFilter) {
                        Text(verbatim: L("inbox.all_agents")).tag(String?.none)
                        ForEach(store.inboxAgents) { agent in
                            Text(verbatim: agent.pending > 0 ? "\(agent.path) (\(agent.pending))" : agent.path)
                                .tag(Optional(agent.path))
                        }
                    }
                    .pickerStyle(.menu)
                    .fixedSize()
                    Spacer()
                }
                .padding(.horizontal, 14)
                .padding(.vertical, 6)
            }
            if store.inbox.isEmpty {
                EmptyNote(online: store.state.online, text: L("inbox.empty"))
            } else {
                List(store.inbox) { thread in
                    ThreadRow(store: store, thread: thread)
                        .padding(.vertical, 4)
                }
            }
        }
    }
}

private struct ThreadRow: View {
    @Bindable var store: CoreStore
    let thread: InboxThread
    @State private var confirmClose = false
    @FocusState private var replyFocused: Bool

    var body: some View {
        let t = thread
        let busy = store.isBusy(t.key)
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                MentionText(t.title).font(.headline).lineLimit(2)
                Tag(text: t.committee)
                Tag(text: t.kindLabel)
                Tag(text: t.stateLabel, tint: t.state == "suspended" ? .orange : .blue)
                if let label = t.triageLabel {
                    Tag(text: label, tint: t.triage == "needs_human" ? .red : .secondary)
                }
                if t.escalated { Tag(text: L("inbox.escalated"), tint: .red) }
            }
            TimedMention(date: t.since) { ago in
                var line = L("inbox.meta", String(t.root), from(t), t.stateLabel, ago)
                if let owner = t.owner { line += L("inbox.owner", owner) }
                if let takeover = t.takeover { line += L("inbox.taken_by", takeover) }
                if let escalated = t.escalatedTo { line += L("inbox.escalated_to", escalated) }
                return line
            }
            .font(.callout)
            .foregroundStyle(.secondary)
            if !t.forAgents.isEmpty {
                MentionText(L("inbox.for", t.to.joined(separator: " ")))
                    .font(.callout).foregroundStyle(.secondary)
            }
            if let why = why(t) {
                MentionText(why).font(.callout).foregroundStyle(.secondary)
            }
            HStack {
                Button(store.replying.contains(t.key) ? L("inbox.cancel_reply") : L("inbox.reply")) {
                    store.toggleReply(t)
                    replyFocused = store.replying.contains(t.key)
                }
                .buttonStyle(.borderedProminent)
                Button(L("inbox.take")) { Task { await store.take(t) } }
                    .help(L("inbox.take_help"))
                    .disabled(t.takeover != nil)
                Button(L("inbox.close"), role: .destructive) { confirmClose = true }
                if busy { ProgressView().controlSize(.small) }
            }
            .disabled(busy)
            if store.replying.contains(t.key) {
                TextEditor(text: draft)
                    .font(.body)
                    .frame(minHeight: 60, maxHeight: 140)
                    .overlay(RoundedRectangle(cornerRadius: 4).stroke(.quaternary))
                    .focused($replyFocused)
                HStack {
                    Text(verbatim: L("inbox.reply_hint", String(t.root), store.state.person.map { "[senator::\($0)]" } ?? ""))
                        .font(.caption).foregroundStyle(.secondary)
                    Spacer()
                    Button(L("inbox.send")) { Task { await store.sendReply(t) } }
                        .keyboardShortcut(.return, modifiers: .command)
                        .buttonStyle(.borderedProminent)
                        .disabled(busy || (store.drafts[t.key] ?? "")
                            .trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                }
            }
        }
        .confirmationDialog(L("inbox.close_title", t.title), isPresented: $confirmClose) {
            Button(L("inbox.close_confirm"), role: .destructive) { Task { await store.close(t) } }
        } message: {
            Text(verbatim: L("inbox.close_message", t.committee))
        }
    }

    private var draft: Binding<String> {
        Binding(
            get: { store.drafts[thread.key] ?? "" },
            set: { store.drafts[thread.key] = $0 }
        )
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
