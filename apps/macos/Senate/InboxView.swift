import SwiftUI
import SenateCore

/// The open threads waiting on you, merged across your agents or narrowed to
/// one, beside the chosen thread: read it, reply as yourself, take it over,
/// or close it.
struct InboxView: View {
    @Bindable var store: CoreStore
    @FocusState private var searching: Bool

    var body: some View {
        HSplitView {
            list
                .frame(minWidth: 260, idealWidth: 320, maxWidth: 480)
            pane
                .frame(minWidth: 320, maxWidth: .infinity, maxHeight: .infinity)
        }
        .searchable(text: $store.search, placement: .toolbar, prompt: L("inbox.search"))
        .searchFocused($searching)
        .background {
            // ⌘F: to the search field.
            Button(L("inbox.search")) { searching = true }
                .keyboardShortcut("f")
                .opacity(0)
                .frame(width: 0, height: 0)
                .accessibilityHidden(true)
        }
        .toolbar {
            if store.inboxAgents.count > 1 {
                ToolbarItem {
                    AgentFilter(store: store)
                }
            }
        }
    }

    @ViewBuilder private var list: some View {
        let threads = store.inbox
        if threads.isEmpty {
            if store.search.isEmpty {
                EmptyNote(online: store.state.online, text: L("inbox.empty"), symbol: "tray")
            } else {
                ContentUnavailableView.search(text: store.search)
            }
        } else {
            List(threads, selection: $store.selectedThread) { thread in
                ThreadRow(thread: thread)
                    .environment(\.mentionCommittee, thread.committee)
                    .tag(thread.key)
                    .contextMenu { RowMenu(store: store, thread: thread) }
            }
            .listStyle(.inset)
        }
    }

    @ViewBuilder private var pane: some View {
        if let row = store.selectedRow {
            ThreadPane(store: store, row: row)
                .environment(\.mentionCommittee, row.committee)
                // Read again when something new is said in it.
                .task(id: "\(row.key)@\(row.updated)") { await store.loadThread(row) }
        } else {
            ContentUnavailableView(
                L("inbox.no_selection"),
                systemImage: "text.bubble",
                description: Text(verbatim: L("inbox.no_selection_hint"))
            )
        }
    }
}

private struct AgentFilter: View {
    @Bindable var store: CoreStore

    var body: some View {
        Picker(L("inbox.agent"), selection: $store.agentFilter) {
            Text(verbatim: L("inbox.all_agents")).tag(String?.none)
            ForEach(store.inboxAgents) { agent in
                Text(verbatim: agent.pending > 0 ? "\(agent.path) (\(agent.pending))" : agent.path)
                    .tag(Optional(agent.path))
            }
        }
        .pickerStyle(.menu)
        .help(L("inbox.agent"))
    }
}

/// One thread in the list: whether it has news for you, its title, what was
/// last said, where and how long ago.
private struct ThreadRow: View {
    let thread: InboxThread

    var body: some View {
        let t = thread
        HStack(alignment: .top, spacing: 8) {
            Circle()
                .fill(t.unread ? Color.accentColor : .clear)
                .frame(width: 8, height: 8)
                .padding(.top, 5)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 3) {
                HStack(alignment: .firstTextBaseline) {
                    MentionText(t.title)
                        .font(t.unread ? .body.weight(.semibold) : .body)
                        .lineLimit(2)
                    Spacer(minLength: 4)
                    Ago(date: t.since)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                if let line = t.newsLine {
                    MentionText(news(t, line))
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                }
                HStack(spacing: 4) {
                    Tag(text: t.committee)
                    Tag(text: t.stateLabel, tint: t.state == "suspended" ? .orange : .blue)
                    if t.escalated { Tag(text: L("inbox.escalated"), tint: .red) }
                    if t.takeover != nil { Tag(text: L("inbox.taken"), tint: .purple) }
                }
            }
        }
        .padding(.vertical, 3)
        .accessibilityElement(children: .combine)
        .accessibilityValue(t.unread ? L("a11y.unread") : "")
    }

    private func news(_ t: InboxThread, _ line: String) -> String {
        guard let from = t.newsFrom else { return line }
        return "\(from) \(t.newsKindLabel ?? ""): \(line)"
    }
}

/// The row's right-click menu: what the pane's buttons do.
private struct RowMenu: View {
    let store: CoreStore
    let thread: InboxThread

    var body: some View {
        Button(L("inbox.take")) { Task { await store.take(thread) } }
            .disabled(thread.takeover != nil || store.isBusy(thread.key))
        Button(L("inbox.close"), role: .destructive) { Task { await store.close(thread) } }
            .disabled(store.isBusy(thread.key))
    }
}

/// The open thread: what it is, every event in it, and the reply box.
private struct ThreadPane: View {
    @Bindable var store: CoreStore
    let row: InboxThread
    @State private var confirmClose = false

    var body: some View {
        VStack(spacing: 0) {
            ThreadHeader(store: store, row: row, confirmClose: $confirmClose)
            Divider()
            events
            Divider()
            Composer(store: store, row: row)
        }
        .confirmationDialog(L("inbox.close_title", row.title), isPresented: $confirmClose) {
            Button(L("inbox.close_confirm"), role: .destructive) { Task { await store.close(row) } }
        } message: {
            Text(verbatim: L("inbox.close_message", row.committee))
        }
    }

    @ViewBuilder private var events: some View {
        if let view = store.threadView(for: row) {
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 10) {
                        ForEach(view.events, id: \.seq) { event in
                            EventBubble(event: event).id(event.seq)
                        }
                    }
                    .padding(14)
                }
                .onAppear { proxy.scrollTo(view.events.last?.seq, anchor: .bottom) }
                .onChange(of: view.events.last?.seq) { _, last in
                    withAnimation { proxy.scrollTo(last, anchor: .bottom) }
                }
            }
        } else if let error = store.threadError {
            ContentUnavailableView(
                L("inbox.thread_failed"),
                systemImage: "exclamationmark.bubble",
                description: Text(verbatim: error)
            )
        } else {
            ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }
}

private struct ThreadHeader: View {
    let store: CoreStore
    let row: InboxThread
    @Binding var confirmClose: Bool

    var body: some View {
        let t = row
        let busy = store.isBusy(t.key)
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .firstTextBaseline) {
                MentionText(t.title)
                    .font(.title3.weight(.semibold))
                    .textSelection(.enabled)
                Spacer()
                if busy { ProgressView().controlSize(.small) }
                Button(L("inbox.take")) { Task { await store.take(t) } }
                    .help(L("inbox.take_help"))
                    .disabled(t.takeover != nil || busy || !store.state.online)
                Button(L("inbox.close"), role: .destructive) { confirmClose = true }
                    .disabled(busy || !store.state.online)
            }
            HStack(spacing: 4) {
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
            if let why = why(t) {
                MentionText(why).font(.callout).foregroundStyle(.secondary)
            }
        }
        .padding(14)
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

/// One event: who wrote it, its kind, when, and what was said. Yours sit on
/// the other side, tinted.
private struct EventBubble: View {
    let event: ThreadEvent

    var body: some View {
        HStack {
            if event.mine { Spacer(minLength: 40) }
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 6) {
                    MentionText(event.by).font(.caption.weight(.medium))
                    Text(verbatim: event.kindLabel).font(.caption).foregroundStyle(.secondary)
                    Ago(date: event.at).font(.caption).foregroundStyle(.secondary)
                }
                MentionText(event.body)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(10)
            .background(
                event.mine ? AnyShapeStyle(Color.accentColor.opacity(0.14)) : AnyShapeStyle(.quaternary.opacity(0.6)),
                in: RoundedRectangle(cornerRadius: 10)
            )
            if !event.mine { Spacer(minLength: 40) }
        }
        .accessibilityElement(children: .combine)
    }
}

/// The reply box: answers in the thread as you, `[senator::<you>]`.
private struct Composer: View {
    @Bindable var store: CoreStore
    let row: InboxThread
    @FocusState private var focused: Bool

    var body: some View {
        let busy = store.isBusy(row.key)
        let empty = draft.wrappedValue.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        VStack(alignment: .leading, spacing: 6) {
            TextEditor(text: draft)
                .font(.body)
                .scrollContentBackground(.hidden)
                .frame(minHeight: 54, maxHeight: 140)
                .focused($focused)
                .accessibilityLabel(L("inbox.reply"))
                .overlay(alignment: .topLeading) {
                    if draft.wrappedValue.isEmpty {
                        Text(verbatim: L("inbox.reply_placeholder"))
                            .foregroundStyle(.tertiary)
                            .padding(.leading, 5)
                            .allowsHitTesting(false)
                    }
                }
            HStack {
                Text(verbatim: L("inbox.replying_as", store.state.person ?? "?", String(row.root)))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Spacer()
                Button(L("inbox.send")) { Task { await store.sendReply(row) } }
                    .keyboardShortcut(.return, modifiers: .command)
                    .buttonStyle(.borderedProminent)
                    .disabled(busy || empty || !store.state.online)
            }
        }
        .padding(12)
    }

    private var draft: Binding<String> {
        Binding(
            get: { store.drafts[row.key] ?? "" },
            set: { store.drafts[row.key] = $0.isEmpty ? nil : $0 }
        )
    }
}
