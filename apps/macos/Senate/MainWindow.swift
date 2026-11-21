import SwiftUI
import SenateCore

/// The window: a connection header, the last error, and the tabs.
struct MainWindow: View {
    @Bindable var store: CoreStore

    var body: some View {
        VStack(spacing: 0) {
            Header(store: store)
            if let error = store.shownError {
                ErrorBar(message: error) { store.actionError = nil }
            }
            TabView(selection: $store.tab) {
                InboxView(store: store)
                    .tabItem { Text(verbatim: label(L("tab.inbox"), store.state.counts.threads)) }
                    .tag(CoreStore.Tab.inbox)
                ApprovalsView(store: store)
                    .tabItem { Text(verbatim: label(L("tab.approvals"), store.state.counts.approvals)) }
                    .tag(CoreStore.Tab.approvals)
                CodexView(store: store)
                    .tabItem { Text(verbatim: label(L("tab.codex"), store.state.counts.codex)) }
                    .tag(CoreStore.Tab.codex)
                // Only while one of your agents presides over a Senate.
                if store.state.menu.docket != nil {
                    DocketView(store: store)
                        .tabItem { Text(verbatim: label(L("tab.docket"), store.state.counts.docket)) }
                        .tag(CoreStore.Tab.docket)
                }
                SenatesView(store: store)
                    .tabItem { Text(verbatim: L("tab.senates")) }
                    .tag(CoreStore.Tab.senates)
            }
            .padding(.top, 8)
        }
        .toolbar {
            ToolbarItem {
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
    }

    private func label(_ title: String, _ count: UInt32) -> String {
        count > 0 ? "\(title) (\(count))" : title
    }
}

private struct Header: View {
    let store: CoreStore

    var body: some View {
        let s = store.state
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                Circle()
                    .fill(s.online ? Color.green : Color.secondary)
                    .frame(width: 8, height: 8)
                if s.online {
                    Text(verbatim: L("header.online_as", s.person ?? "?")).bold()
                    if let primary = s.senates.first {
                        Text(verbatim: primary.serverUrl).foregroundStyle(.secondary)
                    }
                } else {
                    Text(verbatim: L("header.offline")).bold()
                    if let reason = s.offlineReason {
                        Text(verbatim: reason).foregroundStyle(.secondary).lineLimit(1)
                    }
                }
                Spacer()
            }
            if s.senates.count > 1 {
                HStack(spacing: 6) {
                    ForEach(s.senates) { senate in
                        Tag(text: senate.title, tint: senate.committees.allSatisfy(\.live) ? .green : .orange)
                    }
                }
            }
        }
        .font(.callout)
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
    }
}

private struct ErrorBar: View {
    let message: String
    let dismiss: () -> Void

    var body: some View {
        HStack(alignment: .top) {
            Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
            MentionText(message).textSelection(.enabled)
            Spacer()
            Button(action: dismiss) { Image(systemName: "xmark") }
                .buttonStyle(.borderless)
        }
        .font(.callout)
        .padding(.horizontal, 14)
        .padding(.vertical, 6)
        .background(Color.orange.opacity(0.12))
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

    var body: some View {
        VStack {
            Spacer()
            Text(verbatim: online ? text : L("empty.daemon_down"))
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .padding()
            Spacer()
        }
        .frame(maxWidth: .infinity)
    }
}

/// `needs_human` → `needs human`.
func words(_ s: String) -> String {
    s.replacingOccurrences(of: "_", with: " ")
}
