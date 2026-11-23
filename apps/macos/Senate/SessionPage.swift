import SwiftUI
import SenateCore

/// One session on this machine: who and where it is, what can be done with
/// it, and its conversation as the runtime's transcript has it, followed
/// live. A permission prompt waiting in it is decided above the composer;
/// the composer says something to its senator as you, in its committee's
/// log (it is not typed into the terminal).
struct SessionPage: View {
    @Bindable var store: CoreStore
    let session: SessionLine

    var body: some View {
        VStack(spacing: 0) {
            SessionConversation(store: store, session: session)
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
            if let hosted = session.hosted, hosted.promptSince != nil {
                PromptBlock(store: store, session: hosted, tinted: true)
                    .frame(maxWidth: Theme.composerWidth - 2 * Theme.Space.s)
                    .padding(.horizontal, Theme.Space.l)
                    .padding(.bottom, Theme.Space.xs)
                    .frame(maxWidth: .infinity)
            }
            if session.committee != nil {
                MessageComposer(
                    text: Binding(
                        get: { store.sessionDrafts[session.sessionId] ?? "" },
                        set: { store.sessionDrafts[session.sessionId] = $0.isEmpty ? nil : $0 }
                    ),
                    label: L("session.say"),
                    placeholder: L("session.say_placeholder", session.displayName),
                    footnote: L("session.saying_as", store.state.person ?? "?", session.committee ?? ""),
                    busy: store.isBusy("say:\(session.sessionId)"),
                    online: store.state.online
                ) {
                    Task { await store.say(to: session) }
                }
            }
        }
        .id(session.sessionId)
        .environment(\.mentionCommittee, session.committee)
    }
}

/// The session's name, where it works, what it runs, its state, and its
/// actions.
private struct SessionHeader: View {
    let store: CoreStore
    let session: SessionLine

    var body: some View {
        let s = session
        VStack(alignment: .leading, spacing: Theme.Space.s) {
            HStack(alignment: .firstTextBaseline, spacing: Theme.Space.s) {
                SenatorName(name: s.displayName, id: Mentions.id(s.mention))
                    .font(Theme.Fonts.title)
                    .accessibilityAddTraits(.isHeader)
                Spacer(minLength: Theme.Space.m)
                if let h = s.hosted {
                    HStack(spacing: Theme.Space.s) {
                        if store.isBusy("session:\(h.label)") { ProgressView().controlSize(.small) }
                        Button(L("sessions.attach")) { store.attach(h) }
                            .buttonStyle(.pill)
                            .help(L("sessions.attach_help"))
                        SessionActions(store: store, hosted: h, style: .buttons)
                    }
                    .disabled(store.isBusy("session:\(h.label)") || !store.state.online)
                }
            }
            if let title = s.title {
                Text(verbatim: title)
                    .font(Theme.Fonts.text)
                    .foregroundStyle(Theme.onSurface)
                    .lineLimit(2)
                    .textSelection(.enabled)
            }
            HStack(spacing: Theme.Space.xs) {
                StatusChip(session: s)
                if let committee = s.committee {
                    Button {
                        store.place = .committee(committee)
                    } label: {
                        Chip(text: committee)
                    }
                    .buttonStyle(.plain)
                    .help(L("committees.open", committee))
                }
                if s.chair { Chip(text: L("committees.chair"), tone: .accent) }
                Chip(text: [s.runtime, s.model].compactMap { $0 }.joined(separator: " · "))
                if s.hosted == nil { Chip(text: L("session.yours")) }
                if s.hosted?.attached == true { Chip(text: L("sessions.attached"), tone: .accent) }
            }
            MetaLine {
                Text(verbatim: s.dir)
                    .truncationMode(.middle)
                if let branch = s.branch {
                    Text(verbatim: "·")
                    Label(branch, systemImage: "arrow.triangle.branch")
                        .labelStyle(.titleAndIcon)
                }
                Text(verbatim: "·")
                TimedMention(date: s.lastActivity) { L("session.active_ago", $0) }
            }
        }
        .padding(.top, Theme.Space.xl)
        .padding(.bottom, Theme.Space.m)
        .contextMenu {
            Button(L("mention.copy", s.sessionId)) { Mentions.copyText(s.sessionId) }
        }
    }
}

/// The session's state, in its status's colour.
private struct StatusChip: View {
    let session: SessionLine

    var body: some View {
        HStack(spacing: 5) {
            StatusMark(status: session.status)
            Text(verbatim: session.statusLabel)
                .font(Theme.Fonts.chip)
                .foregroundStyle(Theme.onSurfaceVariant)
        }
        .padding(.leading, 6)
        .padding(.trailing, Theme.Space.s)
        .padding(.vertical, 3)
        .background(Theme.containerHigh, in: Capsule())
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(session.statusLabel)
    }
}

/// Stop, Pause, Resume or Restore, as the hosted session's state allows:
/// pill buttons in the page's header, items in the sidebar's menu.
struct SessionActions: View {
    enum Style { case buttons, menu }

    let store: CoreStore
    let hosted: HostedSession
    let style: Style

    var body: some View {
        let h = hosted
        if h.running {
            action(L("session.pause"), help: L("session.pause_help")) { await store.pause(h) }
            action(L("sessions.stop"), help: L("sessions.stop_help"), danger: true) { await store.stop(h) }
        } else if h.paused {
            action(L("sessions.resume"), help: L("sessions.resume_help"), filled: true) { await store.resume(h) }
        } else {
            action(L("sessions.restore"), help: L("sessions.restore_help"), filled: true) { await store.restore(h) }
        }
    }

    @ViewBuilder
    private func action(
        _ title: String, help: String, filled: Bool = false, danger: Bool = false,
        _ run: @escaping () async -> Void
    ) -> some View {
        switch style {
        case .buttons:
            Button(title) { Task { await run() } }
                .buttonStyle(.pill(danger ? .danger : filled ? .filled : .tonal))
                .help(help)
        case .menu:
            Button(title, role: danger ? .destructive : nil) { Task { await run() } }
        }
    }
}

/// A session's status as one small mark, most urgent first: a prompt waits
/// (attention), it works (a turning ring), it said something you have not
/// read (accent), idle, paused, stopped.
struct StatusMark: View {
    let status: SessionStatus

    var body: some View {
        Group {
            switch status {
            case .needsYou:
                Circle().fill(Theme.attention)
                    .frame(width: 8, height: 8)
                    .padding(2)
                    .overlay { Circle().stroke(Theme.attention.opacity(0.35), lineWidth: 2) }
            case .working:
                WorkingRing()
            case .unread:
                Circle().fill(Theme.primary).frame(width: 8, height: 8)
            case .idle:
                Circle().fill(Theme.onSurfaceVariant.opacity(0.45)).frame(width: 7, height: 7)
            case .paused:
                Image(systemName: "pause.fill")
                    .font(.system(size: 8, weight: .bold))
                    .foregroundStyle(Theme.onSurfaceVariant)
            case .stopped:
                Circle().stroke(Theme.onSurfaceVariant.opacity(0.6), lineWidth: 1.2).frame(width: 7, height: 7)
            }
        }
        .frame(width: 12, height: 12)
        .accessibilityLabel(status.label)
    }
}

extension SessionStatus {
    var label: String {
        switch self {
        case .needsYou: L("status.needs_you")
        case .working: L("status.working")
        case .unread: L("status.unread")
        case .idle: L("status.idle")
        case .paused: L("status.paused")
        case .stopped: L("status.stopped")
        }
    }
}

/// A quiet ring that turns while a session works.
private struct WorkingRing: View {
    @State private var turning = false

    var body: some View {
        Circle()
            .trim(from: 0, to: 0.72)
            .stroke(Theme.primary, style: StrokeStyle(lineWidth: 1.6, lineCap: .round))
            .frame(width: 9, height: 9)
            .rotationEffect(.degrees(turning ? 360 : 0))
            .animation(.linear(duration: 1.1).repeatForever(autoreverses: false), value: turning)
            .onAppear { turning = true }
    }
}

/// A session's one leading mark: its runtime glyph, carrying its status.
/// Needs you: the glyph in the attention colour with a dot at its corner.
/// Working: a ring turning around it. Unread: an accent dot at its corner.
/// Idle: the glyph as it is. Paused: dimmed, with a pause badge. Stopped:
/// faint. The names after it line up in one column.
struct SessionMark: View {
    let runtime: String
    let status: SessionStatus

    var body: some View {
        RuntimeMark(runtime: runtime, tint: tint)
            .opacity(status == .stopped ? 0.4 : status == .paused ? 0.6 : 1)
            .frame(width: 20, height: 20)
            .overlay {
                if status == .working {
                    TurningRing(size: 20, width: 1.4)
                }
            }
            .overlay(alignment: .topTrailing) { badge.offset(x: 2, y: -1) }
            .accessibilityElement(children: .ignore)
            .accessibilityLabel("\(runtime), \(status.label)")
    }

    private var tint: Color {
        switch status {
        case .needsYou: Theme.attention
        case .working, .unread: Theme.primary
        case .idle, .paused, .stopped: Theme.onSurfaceVariant
        }
    }

    @ViewBuilder private var badge: some View {
        switch status {
        case .needsYou: dot(Theme.attention)
        case .unread: dot(Theme.primary)
        case .paused:
            Image(systemName: "pause.fill")
                .font(.system(size: 5, weight: .black))
                .foregroundStyle(Theme.sidebar)
                .frame(width: 9, height: 9)
                .background(Theme.onSurfaceVariant, in: Circle())
                .overlay { Circle().stroke(Theme.sidebar, lineWidth: 1.5) }
        case .working, .idle, .stopped: EmptyView()
        }
    }

    private func dot(_ color: Color) -> some View {
        Circle().fill(color)
            .frame(width: 7, height: 7)
            .overlay { Circle().stroke(Theme.sidebar, lineWidth: 1.5) }
    }
}

/// A ring that turns while a session works, around its mark.
struct TurningRing: View {
    var size: CGFloat = 9
    var width: CGFloat = 1.6
    @State private var turning = false

    var body: some View {
        Circle()
            .trim(from: 0, to: 0.3)
            .stroke(Theme.primary, style: StrokeStyle(lineWidth: width, lineCap: .round))
            .frame(width: size, height: size)
            .rotationEffect(.degrees(turning ? 360 : 0))
            .animation(.linear(duration: 1.1).repeatForever(autoreverses: false), value: turning)
            .onAppear { turning = true }
            .accessibilityHidden(true)
    }
}

/// The runtime a session runs, as a small glyph in the text's colour
/// (`RuntimeClaude`, `RuntimeCodex`, `RuntimeAgy`, else a terminal); its
/// name is in the tooltip. One asset per runtime, so a real mark can
/// replace a glyph by swapping its file (Assets.xcassets/RUNTIME-ICONS.md).
struct RuntimeMark: View {
    let runtime: String
    var tint: Color = Theme.onSurfaceVariant

    var body: some View {
        Image(Self.asset(runtime))
            .renderingMode(.template)
            .resizable()
            .interpolation(.high)
            .frame(width: 14, height: 14)
            .foregroundStyle(tint)
            .help(runtime)
            .accessibilityLabel(runtime)
    }

    static func asset(_ runtime: String) -> String {
        switch runtime {
        case "claude": "RuntimeClaude"
        case "codex": "RuntimeCodex"
        case "agy": "RuntimeAgy"
        default: "RuntimeTerminal"
        }
    }
}

// MARK: - The conversation

/// The header, then the conversation, the latest at the foot. It stays at
/// the foot as the session goes on, unless you scrolled up; reaching the
/// top reads the page before.
private struct SessionConversation: View {
    @Bindable var store: CoreStore
    let session: SessionLine
    @State private var position = ScrollPosition(edge: .bottom)

    var body: some View {
        let t = store.timeline.flatMap { $0.sessionId == session.sessionId ? $0 : nil }
        GeometryReader { geometry in
        ScrollView {
            LazyVStack(alignment: .leading, spacing: Theme.Space.l) {
                SessionHeader(store: store, session: session)
                if let t {
                    body(t)
                } else {
                    ProgressView().controlSize(.small).frame(maxWidth: .infinity)
                }
            }
            .accessibilityElement(children: .contain)
            .accessibilityLabel(session.displayName)
            .frame(maxWidth: Theme.readingWidth, alignment: .leading)
            .padding(.horizontal, Theme.Space.xl)
            .padding(.top, Theme.Space.s)
            .padding(.bottom, Theme.Space.xl)
            // Shorter than the view, it starts at the top all the same.
            .frame(maxWidth: .infinity, minHeight: geometry.size.height, alignment: .top)
        }
        // Opens at the foot, and stays there as items arrive while you are
        // there; scrolled up, what you read stays put.
        .defaultScrollAnchor(.bottom)
        .defaultScrollAnchor(.bottom, for: .sizeChanges)
        .scrollPosition($position)
        #if DEBUG
        // The snapshot tour scrolls to the top, to check that reading the
        // page before keeps what is on screen where it is.
        .onReceive(NotificationCenter.default.publisher(for: Snapshot.scrollToTop)) { _ in
            position.scrollTo(edge: .top)
        }
        #endif
        }
    }

    /// Read the page before, keeping what was first on screen where it was:
    /// without this, at the very top the view would stay at offset zero and
    /// jump onto what was just read.
    private func older(_ t: SessionTimeline) async {
        guard t.hasOlder, !t.loadingOlder, let first = t.firstSeq else { return }
        await store.loadOlder()
        guard t.firstSeq != first else { return }
        var still = Transaction()
        still.disablesAnimations = true
        withTransaction(still) { position.scrollTo(id: first, anchor: .top) }
    }

    @ViewBuilder private func body(_ t: SessionTimeline) -> some View {
        if t.loading {
            ProgressView().controlSize(.small).frame(maxWidth: .infinity).padding(.top, Theme.Space.xl)
        } else if !t.available || !session.readable {
            Nothing(text: t.unavailable ?? L("session.unreadable"), hosted: session.hosted, store: store)
        } else {
            if t.hasOlder {
                HStack {
                    Spacer()
                    if t.loadingOlder {
                        ProgressView().controlSize(.small)
                    } else {
                        Button(L("session.earlier")) { Task { await older(t) } }
                            .buttonStyle(.pill)
                    }
                    Spacer()
                }
                .onAppear { Task { await older(t) } }
            }
            if t.items.isEmpty {
                Nothing(text: L("session.empty"), hosted: nil, store: store)
            }
            ForEach(t.items) { item in
                EntryView(store: store, entry: item, committee: session.committee)
                    .id(item.seq)
            }
            if t.working {
                HStack(spacing: Theme.Space.s) {
                    WorkingRing()
                    Text(verbatim: L("status.working"))
                        .font(Theme.Fonts.meta)
                        .foregroundStyle(Theme.onSurfaceVariant)
                }
                .padding(.leading, Theme.Space.xs)
            }
            if let error = t.error {
                Text(verbatim: error)
                    .font(Theme.Fonts.meta)
                    .foregroundStyle(Theme.danger)
            }
        }
    }
}

/// Why there is nothing to read, and the way to see it anyway.
private struct Nothing: View {
    let text: String
    let hosted: HostedSession?
    let store: CoreStore

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.m) {
            Text(verbatim: text)
                .font(Theme.Fonts.text)
                .foregroundStyle(Theme.onSurfaceVariant)
                .fixedSize(horizontal: false, vertical: true)
            if let hosted {
                Button(L("sessions.attach")) { store.attach(hosted) }
                    .buttonStyle(.pill(.filled))
            }
        }
        .card()
    }
}

/// One item of the conversation.
private struct EntryView: View {
    let store: CoreStore
    let entry: TimelineEntry
    let committee: String?

    var body: some View {
        switch entry.body {
        case let .user(text):
            Text(verbatim: text)
                .font(Theme.Fonts.reading)
                .lineSpacing(Theme.readingLineSpacing)
                .foregroundStyle(Theme.onSurface)
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.horizontal, Theme.Space.l)
                .padding(.vertical, 10)
                .background(Theme.container, in: RoundedRectangle(cornerRadius: Theme.Radius.bubble))
                .padding(.leading, 96)
                .frame(maxWidth: .infinity, alignment: .trailing)
                .padding(.top, Theme.Space.s)
        case let .assistant(text):
            MarkdownText(text: text)
        case let .reasoning(text):
            Reasoning(text: text)
        case let .tool(call):
            ToolRow(call: call)
        case let .todo(items):
            TodoList(items: items)
        case let .notice(level, text):
            Label(text, systemImage: level == "error" ? "exclamationmark.triangle" : "info.circle")
                .font(Theme.Fonts.meta)
                .foregroundStyle(level == "error" ? Theme.danger : Theme.onSurfaceVariant)
        case let .senate(message):
            SenateCard(store: store, message: message)
        }
    }
}

/// Its reasoning: one dimmed line, folded; open, the whole of it.
private struct Reasoning: View {
    let text: String
    @State private var open = false

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.xs) {
            Button {
                open.toggle()
            } label: {
                HStack(spacing: 6) {
                    Image(systemName: open ? "chevron.down" : "chevron.right")
                        .font(.system(size: 9, weight: .semibold))
                        .frame(width: 12)
                    Text(verbatim: open ? L("session.reasoning") : firstLine(text))
                        .lineLimit(1)
                        .italic()
                }
                .font(Theme.Fonts.row)
                .foregroundStyle(Theme.onSurfaceVariant)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel(L("session.reasoning"))
            .accessibilityValue(open ? L("a11y.expanded") : L("a11y.collapsed"))
            if open {
                Text(verbatim: text)
                    .font(Theme.Fonts.text)
                    .foregroundStyle(Theme.onSurfaceVariant)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.leading, 18)
            }
        }
    }
}

/// One tool call: an icon for its kind, the command or path, and how it
/// went. It opens to what it printed, or to its diff.
private struct ToolRow: View {
    let call: ToolCall
    @State private var open = false
    @State private var hovered = false

    var body: some View {
        let more = call.output != nil || call.diff != nil
        VStack(alignment: .leading, spacing: Theme.Space.xs) {
            Button {
                if more { open.toggle() }
            } label: {
                HStack(spacing: Theme.Space.s) {
                    Image(systemName: symbol)
                        .font(.system(size: 11, weight: .medium))
                        .foregroundStyle(Theme.onSurfaceVariant)
                        .frame(width: 16)
                        .accessibilityHidden(true)
                    Text(verbatim: call.title)
                        .font(Theme.Fonts.code)
                        .foregroundStyle(Theme.onSurface)
                        .lineLimit(1)
                        .truncationMode(.middle)
                    Spacer(minLength: Theme.Space.s)
                    if let exit = call.exitCode, exit != 0 {
                        Text(verbatim: "exit \(exit)")
                            .font(Theme.Fonts.meta)
                            .foregroundStyle(Theme.danger)
                    }
                    status
                    if more {
                        Image(systemName: open ? "chevron.down" : "chevron.right")
                            .font(.system(size: 9, weight: .semibold))
                            .foregroundStyle(Theme.onSurfaceVariant)
                            .accessibilityHidden(true)
                    }
                }
                .padding(.horizontal, Theme.Space.m)
                .padding(.vertical, 7)
                .background(hovered && more ? Theme.containerHigh : Theme.container,
                            in: RoundedRectangle(cornerRadius: Theme.Radius.code))
                .contentShape(RoundedRectangle(cornerRadius: Theme.Radius.code))
            }
            .buttonStyle(.plain)
            .onHover { hovered = $0 }
            .help(call.name)
            .accessibilityElement(children: .ignore)
            .accessibilityLabel("\(call.name): \(call.title)")
            .accessibilityValue(call.statusLabel)
            .accessibilityAddTraits(more ? .isButton : [])
            #if DEBUG
            .onReceive(NotificationCenter.default.publisher(for: Snapshot.expandTools)) { _ in
                if more { open = true }
            }
            #endif
            if open {
                if let diff = call.diff {
                    DiffView(diff: diff)
                } else if let output = call.output {
                    BoundedLines(text: output)
                }
            }
        }
    }

    @ViewBuilder private var status: some View {
        switch call.status {
        case "running":
            ProgressView().controlSize(.mini)
        case "failed":
            Image(systemName: "xmark.circle.fill").foregroundStyle(Theme.danger)
                .font(.system(size: 11))
        default:
            Image(systemName: "checkmark").foregroundStyle(Theme.success)
                .font(.system(size: 10, weight: .bold))
        }
    }

    private var symbol: String {
        switch call.kind {
        case "shell": "terminal"
        case "read": "doc.text"
        case "edit": "pencil"
        case "write": "square.and.pencil"
        case "search": "magnifyingglass"
        case "fetch": "globe"
        case "sub_agent": "person.2"
        default: "wrench.and.screwdriver"
        }
    }
}

/// A unified diff, its added and removed lines in their colours, in a
/// bounded block that scrolls.
private struct DiffView: View {
    let diff: String

    var body: some View {
        BoundedLines(text: diff) { line in
            if line.hasPrefix("+") && !line.hasPrefix("+++") { return (Theme.success, Theme.successContainer) }
            if line.hasPrefix("-") && !line.hasPrefix("---") { return (Theme.danger, Theme.dangerContainer) }
            if line.hasPrefix("@@") { return (Theme.onSurfaceVariant, .clear) }
            return (Theme.onSurface, .clear)
        }
    }
}

/// What a tool printed, or a diff: at most `maxHeight` tall, scrolling
/// inside, its lines drawn as they come into view, so a 64 KiB output costs
/// what is on screen. A line longer than `lineLimit` characters is cut.
struct BoundedLines: View {
    let text: String
    var maxHeight: CGFloat = 320
    var style: (Substring) -> (Color, Color) = { _ in (Theme.onSurface, .clear) }
    static let lineLimit = 2_000

    init(text: String, maxHeight: CGFloat = 320, style: @escaping (Substring) -> (Color, Color) = { _ in (Theme.onSurface, .clear) }) {
        self.text = text
        self.maxHeight = maxHeight
        self.style = style
    }

    var body: some View {
        let lines = text.split(separator: "\n", omittingEmptySubsequences: false)
        ScrollView(.vertical) {
            LazyVStack(alignment: .leading, spacing: 0) {
                ForEach(lines.indices, id: \.self) { i in
                    let line = lines[i]
                    let (fg, bg) = style(line)
                    Text(verbatim: line.isEmpty ? " " : line.count > Self.lineLimit
                        ? String(line.prefix(Self.lineLimit)) + "…" : String(line))
                        .font(Theme.Fonts.code)
                        .foregroundStyle(fg)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, Theme.Space.m)
                        .background(bg)
                }
            }
            .padding(.vertical, Theme.Space.s)
            .textSelection(.enabled)
        }
        .frame(maxHeight: maxHeight)
        .fixedSize(horizontal: false, vertical: lines.count < 16)
        .background(Theme.containerHigh, in: RoundedRectangle(cornerRadius: Theme.Radius.code))
        .clipShape(RoundedRectangle(cornerRadius: Theme.Radius.code))
    }
}

/// Its todo list, as a checklist.
private struct TodoList: View {
    let items: [TodoLine]

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            ForEach(Array(items.enumerated()), id: \.offset) { _, item in
                HStack(alignment: .firstTextBaseline, spacing: Theme.Space.s) {
                    Image(systemName: symbol(item.status))
                        .foregroundStyle(item.status == "completed" ? Theme.success
                            : item.status == "in_progress" ? Theme.primary : Theme.onSurfaceVariant)
                        .accessibilityLabel(words(item.status))
                    Text(verbatim: item.text)
                        .foregroundStyle(item.status == "completed" ? Theme.onSurfaceVariant : Theme.onSurface)
                        .strikethrough(item.status == "completed", color: Theme.onSurfaceVariant)
                }
                .font(Theme.Fonts.text)
            }
        }
        .card(padding: Theme.Space.m)
        .accessibilityElement(children: .contain)
        .accessibilityLabel(L("session.todos"))
    }

    private func symbol(_ status: String) -> String {
        switch status {
        case "completed": "checkmark.circle.fill"
        case "in_progress": "circle.dotted.circle"
        default: "circle"
        }
    }
}

/// A Senate message delivered to the session: who, what kind, its first
/// lines. It opens its thread.
private struct SenateCard: View {
    let store: CoreStore
    let message: SenateLine
    @State private var hovered = false

    var body: some View {
        Button(action: open) {
            VStack(alignment: .leading, spacing: Theme.Space.xs) {
                HStack(spacing: 6) {
                    Image(systemName: "envelope")
                        .font(.system(size: 11))
                        .foregroundStyle(Theme.committee)
                        .accessibilityHidden(true)
                    if let from = message.from {
                        MentionText(from).font(Theme.Fonts.label)
                    }
                    if let kind = message.kindLabel {
                        Text(verbatim: kind)
                            .font(Theme.Fonts.meta)
                            .foregroundStyle(Theme.onSurfaceVariant)
                    }
                    Spacer(minLength: Theme.Space.s)
                    Text(verbatim: [message.committee, message.thread.map { "#\($0)" }].compactMap { $0 }.joined(separator: " · "))
                        .font(Theme.Fonts.meta)
                        .foregroundStyle(Theme.onSurfaceVariant)
                }
                MentionText(message.text)
                    .font(Theme.Fonts.text)
                    .foregroundStyle(Theme.onSurface)
                    .lineLimit(3)
                    .multilineTextAlignment(.leading)
            }
            .padding(.horizontal, Theme.Space.m)
            .padding(.vertical, 10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(hovered ? Theme.containerHigh : Theme.container,
                        in: RoundedRectangle(cornerRadius: Theme.Radius.code))
            .overlay(alignment: .leading) {
                RoundedRectangle(cornerRadius: 1.5)
                    .fill(Theme.committee.opacity(0.6))
                    .frame(width: 3)
                    .padding(.vertical, Theme.Space.s)
                    .accessibilityHidden(true)
            }
            .contentShape(RoundedRectangle(cornerRadius: Theme.Radius.code))
        }
        .buttonStyle(.plain)
        .onHover { hovered = $0 }
        .environment(\.mentionCommittee, message.committee)
        .help(L("session.open_thread"))
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(.isButton)
    }

    private func open() {
        guard let committee = message.committee else { return }
        if let root = message.thread {
            store.openThread(committee: committee, root: root)
        } else {
            store.place = .committee(committee)
        }
    }
}

/// The agent's Markdown, as it reads: paragraphs, lists and headings with
/// their inline styles, fenced code in a code block.
struct MarkdownText: View {
    let text: String

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.s) {
            ForEach(Array(Self.blocks(text).enumerated()), id: \.offset) { _, block in
                switch block {
                case let .code(code):
                    CodeBlock(text: code)
                case let .heading(line):
                    Text(Self.inline(line))
                        .font(Theme.Fonts.heading)
                        .foregroundStyle(Theme.onSurface)
                case let .paragraph(para):
                    Text(Self.inline(para))
                        .font(Theme.Fonts.reading)
                        .lineSpacing(Theme.readingLineSpacing)
                        .foregroundStyle(Theme.onSurface)
                }
            }
        }
        .textSelection(.enabled)
        .fixedSize(horizontal: false, vertical: true)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    enum Block { case paragraph(String), heading(String), code(String) }

    static func blocks(_ text: String) -> [Block] {
        var out: [Block] = []
        var para: [String] = []
        var code: [String]?
        func flush() {
            if !para.isEmpty { out.append(.paragraph(para.joined(separator: "\n"))) }
            para = []
        }
        for raw in text.components(separatedBy: "\n") {
            if raw.trimmingCharacters(in: .whitespaces).hasPrefix("```") {
                if let c = code {
                    out.append(.code(c.joined(separator: "\n")))
                    code = nil
                } else {
                    flush()
                    code = []
                }
                continue
            }
            if code != nil {
                code?.append(raw)
                continue
            }
            let line = raw.trimmingCharacters(in: .whitespaces)
            if line.isEmpty {
                flush()
            } else if line.hasPrefix("#") {
                flush()
                out.append(.heading(String(line.drop { $0 == "#" }).trimmingCharacters(in: .whitespaces)))
            } else if line.hasPrefix("- ") || line.hasPrefix("* ") {
                para.append("•  " + line.dropFirst(2))
            } else {
                para.append(raw)
            }
        }
        if let c = code { out.append(.code(c.joined(separator: "\n"))) }
        flush()
        return out
    }

    static func inline(_ text: String) -> AttributedString {
        (try? AttributedString(
            markdown: text,
            options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace)
        )) ?? AttributedString(text)
    }
}
