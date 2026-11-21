import SwiftUI
import SenateCore

/// The window: a sidebar of sections with their counts and the connection
/// at its foot, and the chosen section beside it.
struct MainWindow: View {
    @Bindable var store: CoreStore

    var body: some View {
        NavigationSplitView {
            Sidebar(store: store)
                .navigationSplitViewColumnWidth(min: 180, ideal: 210, max: 280)
        } detail: {
            detail
                .navigationTitle((store.section ?? .inbox).title)
                .safeAreaInset(edge: .top, spacing: 0) {
                    if let error = store.shownError {
                        ErrorBar(message: error) { store.actionError = nil }
                    }
                }
        }
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                Button {
                    store.refresh()
                } label: {
                    Label(L("toolbar.refresh"), systemImage: "arrow.clockwise")
                }
                .help(L("toolbar.refresh_help"))
                .disabled(!store.state.online)
                .keyboardShortcut("r")
            }
        }
        .onChange(of: store.state.menu.docket == nil) { _, gone in
            if gone && store.section == .docket { store.section = .inbox }
        }
    }

    @ViewBuilder private var detail: some View {
        switch store.section {
        case .inbox, nil: InboxView(store: store)
        case .approvals: ApprovalsView(store: store)
        case .codex: CodexView(store: store)
        case .docket: DocketView(store: store)
        case .senates: SenatesView(store: store)
        }
    }
}

/// The sections, each with what waits there, and the connection below.
private struct Sidebar: View {
    @Bindable var store: CoreStore

    var body: some View {
        List(selection: $store.section) {
            ForEach(store.sections, id: \.self) { section in
                Label(section.title, systemImage: section.symbol)
                    .badge(Int(store.count(section)))
                    .tag(section)
            }
        }
        .safeAreaInset(edge: .bottom, spacing: 0) {
            ConnectionFooter(state: store.state)
        }
    }
}

extension CoreStore.Section {
    var title: String {
        switch self {
        case .inbox: L("tab.inbox")
        case .approvals: L("tab.approvals")
        case .codex: L("tab.codex")
        case .docket: L("tab.docket")
        case .senates: L("tab.senates")
        }
    }

    var symbol: String {
        switch self {
        case .inbox: "tray"
        case .approvals: "checkmark.shield"
        case .codex: "terminal"
        case .docket: "list.bullet.rectangle"
        case .senates: "building.columns"
        }
    }
}

/// Who this machine is online as, or why it is offline.
private struct ConnectionFooter: View {
    let state: AppState

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            StatusDot(online: state.online)
                .padding(.top, 4)
            VStack(alignment: .leading, spacing: 2) {
                if state.online {
                    Text(verbatim: L("header.online_as", state.person ?? "?"))
                        .font(.callout.weight(.medium))
                    if let primary = state.senates.first {
                        Text(verbatim: primary.serverUrl)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                            .truncationMode(.middle)
                    }
                } else {
                    Text(verbatim: L("header.offline")).font(.callout.weight(.medium))
                    if let reason = state.offlineReason {
                        Text(verbatim: reason)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(3)
                    }
                }
            }
            Spacer(minLength: 0)
        }
        .padding(12)
        .accessibilityElement(children: .combine)
    }
}

/// Online or not, with a label for VoiceOver.
struct StatusDot: View {
    let online: Bool
    var size: CGFloat = 8

    var body: some View {
        Circle()
            .fill(online ? Color.green : Color.secondary)
            .frame(width: size, height: size)
            .accessibilityLabel(online ? L("a11y.online") : L("a11y.offline"))
    }
}

private struct ErrorBar: View {
    let message: String
    let dismiss: () -> Void

    var body: some View {
        HStack(alignment: .top) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
                .accessibilityHidden(true)
            MentionText(message).textSelection(.enabled)
            Spacer()
            Button(action: dismiss) {
                Image(systemName: "xmark")
            }
            .buttonStyle(.borderless)
            .accessibilityLabel(L("a11y.dismiss"))
        }
        .font(.callout)
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
        .background(.orange.opacity(0.12))
    }
}

/// A small rounded label.
struct Tag: View {
    let text: String
    var tint: Color = .secondary

    var body: some View {
        Text(verbatim: text)
            .font(.caption)
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .background(tint.opacity(0.15), in: Capsule())
            .foregroundStyle(tint == .secondary ? Color.primary : tint)
    }
}

/// What a list shows when it is empty or the daemon is offline.
struct EmptyNote: View {
    let online: Bool
    let text: String
    var symbol: String = "tray"

    var body: some View {
        if online {
            ContentUnavailableView(text, systemImage: symbol)
        } else {
            ContentUnavailableView(
                L("empty.daemon_down"),
                systemImage: "bolt.horizontal.circle"
            )
        }
    }
}

/// `needs_human` → `needs human`.
func words(_ s: String) -> String {
    s.replacingOccurrences(of: "_", with: " ")
}
