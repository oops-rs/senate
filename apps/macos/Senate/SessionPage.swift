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
struct SessionHeader: View {
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
