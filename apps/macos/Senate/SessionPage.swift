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
        .toolbar { SessionToolbar(store: store, session: session) }
    }
}

/// The session's name, where it works, what it runs, its state, and its
/// actions.
struct SessionHeader: View {
    let store: CoreStore
    let session: SessionLine

    var body: some View {
        let s = session
        // Its name, state and actions are in the window's toolbar
        // (SessionToolbar); here, what describes it.
        VStack(alignment: .leading, spacing: Theme.Space.s) {
            if let title = s.title {
                Text(verbatim: title)
                    .font(Theme.Fonts.text)
                    .foregroundStyle(Theme.onSurface)
                    .lineLimit(2)
                    .textSelection(.enabled)
            }
            HStack(spacing: Theme.Space.xs) {
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
        .padding(.top, Theme.Space.l)
        .padding(.bottom, Theme.Space.s)
        .contextMenu {
            Button(L("mention.copy", s.sessionId)) { Mentions.copyText(s.sessionId) }
        }
    }
}

/// Stop, Pause, Resume or Restore, as the hosted session's state allows:
/// pill buttons in the page's header, items in the sidebar's menu.
struct SessionActions: View {
    enum Style { case buttons, menu, toolbar }

    let store: CoreStore
    let hosted: HostedSession
    let style: Style

    var body: some View {
        let h = hosted
        if h.running {
            action(L("session.pause"), "pause", help: L("session.pause_help")) { await store.pause(h) }
            // On the toolbar, Stop sits in the session's menu, not as a button.
            if style != .toolbar {
                action(L("sessions.stop"), "stop", help: L("sessions.stop_help"), danger: true) { await store.stop(h) }
            }
        } else if h.paused {
            action(L("sessions.resume"), "play", help: L("sessions.resume_help"), filled: true) { await store.resume(h) }
        } else {
            action(L("sessions.restore"), "arrow.counterclockwise", help: L("sessions.restore_help"), filled: true, decisive: true) {
                await store.restore(h)
            }
        }
    }

    @ViewBuilder
    private func action(
        _ title: String, _ symbol: String, help: String, filled: Bool = false, danger: Bool = false,
        decisive: Bool = false,
        _ run: @escaping () async -> Void
    ) -> some View {
        switch style {
        case .toolbar:
            // Every action is its icon, its title in the tooltip; the
            // page's one decisive action (Restore) is tinted.
            if decisive {
                Button { Task { await run() } } label: { Label(title, systemImage: symbol) }
                    .buttonStyle(.borderedProminent)
                    .help(title)
            } else {
                Button { Task { await run() } } label: { Label(title, systemImage: symbol) }
                    .help(title)
            }
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
/// Working: the glyph's own frame carries the progress, a segment of its
/// outline travelling round it (WorkingRuntimeMark). Unread: an accent dot
/// at its corner.
/// Idle: the glyph as it is. Paused: dimmed, with a pause badge. Stopped:
/// faint. The names after it line up in one column.
struct SessionMark: View {
    let runtime: String
    let status: SessionStatus

    var body: some View {
        Group {
            if status == .working {
                WorkingRuntimeMark(runtime: runtime)
            } else {
                RuntimeMark(runtime: runtime, tint: tint)
            }
        }
            .opacity(status == .stopped ? 0.4 : status == .paused ? 0.6 : 1)
            .frame(width: 20, height: 20)
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

/// The header (native), then the conversation: the window's one web page
/// (ConversationPage), shown for this session.
private struct SessionConversation: View {
    @Bindable var store: CoreStore
    let session: SessionLine

    var body: some View {
        let t = store.timeline.flatMap { $0.sessionId == session.sessionId ? $0 : nil }
        VStack(spacing: 0) {
            SessionHeader(store: store, session: session)
                .frame(maxWidth: Theme.readingWidth, alignment: .leading)
                .padding(.horizontal, Theme.Space.xl)
                .frame(maxWidth: .infinity)
            if let t, !t.loading, !t.available || !session.readable {
                Nothing(text: t.unavailable ?? L("session.unreadable"), hosted: session.hosted, store: store)
                    .frame(maxWidth: Theme.readingWidth)
                    .padding(Theme.Space.xl)
                Spacer()
            } else {
                ConversationPane(store: store, source: .session(session.sessionId))
            }
        }
    }
}

/// Why there is nothing to read, and the way to see it anyway.
struct Nothing: View {
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

/// A session's name, state and actions, in the window's toolbar: its glyph
/// carrying its state and its name with the state in words at the leading
/// edge; Attach and Pause, Resume or Restore as the trailing items; Stop
/// and the copies in the session's menu.
struct SessionToolbar: ToolbarContent {
    let store: CoreStore
    let session: SessionLine

    var body: some ToolbarContent {
        let s = session
        ToolbarItem(placement: .navigation) {
            HStack(spacing: Theme.Space.s) {
                SessionMark(runtime: s.runtime, status: s.status)
                VStack(alignment: .leading, spacing: 0) {
                    SenatorName(name: s.displayName, id: Mentions.id(s.mention))
                        .font(.headline)
                        .lineLimit(1)
                    Text(verbatim: s.statusLabel)
                        .font(.caption)
                        .foregroundStyle(Theme.onSurfaceVariant)
                }
            }
            .padding(.horizontal, Theme.Space.xs)
            .accessibilityElement(children: .combine)
            .accessibilityAddTraits(.isHeader)
        }
        // A title, on the window's ground: no glass behind it.
        .sharedBackgroundVisibility(.hidden)
        ToolbarSpacer(.flexible)
        if let h = s.hosted {
            ToolbarItemGroup(placement: .primaryAction) {
                if store.isBusy("session:\(h.label)") { ProgressView().controlSize(.small) }
                Button { store.attach(h) } label: { Label(L("sessions.attach"), systemImage: "apple.terminal") }
                    .help(L("sessions.attach_help"))
                    .disabled(!store.state.online)
                SessionActions(store: store, hosted: h, style: .toolbar)
                    .disabled(store.isBusy("session:\(h.label)") || !store.state.online)
            }
        }
        ToolbarItem(placement: .primaryAction) {
            Menu {
                if let h = s.hosted, h.running {
                    Button(role: .destructive) { Task { await store.stop(h) } } label: {
                        Label(L("sessions.stop"), systemImage: "stop")
                    }
                    .disabled(store.isBusy("session:\(h.label)") || !store.state.online)
                    Divider()
                }
                if let h = s.hosted {
                    Button(L("mention.copy", h.label)) { Mentions.copyText(h.label) }
                }
                Button(L("mention.copy", s.sessionId)) { Mentions.copyText(s.sessionId) }
            } label: {
                Label(L("thread.more"), systemImage: "ellipsis")
            }
            .menuIndicator(.hidden)
        }
    }
}
