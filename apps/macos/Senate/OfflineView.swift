import SwiftUI
import SenateCore

/// What the window shows while the daemon cannot be reached: why, that the
/// app keeps trying on its own, a way to try now, and how to start the
/// daemon. The app never starts it itself.
struct OfflineView: View {
    let store: CoreStore
    @State private var copied = false

    static let startCommand = "senate setup service install --yes"

    var body: some View {
        ContentUnavailableView {
            Label(L("offline.title"), systemImage: "bolt.horizontal.circle")
        } description: {
            VStack(spacing: 10) {
                if let reason = store.state.offlineReason {
                    Text(verbatim: reason)
                }
                Text(verbatim: L("offline.retrying"))
                    .foregroundStyle(.secondary)
                VStack(alignment: .leading, spacing: 4) {
                    Text(verbatim: L("offline.start_hint"))
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    HStack {
                        Text(verbatim: Self.startCommand)
                            .font(.callout.monospaced())
                            .textSelection(.enabled)
                            .padding(.horizontal, 8)
                            .padding(.vertical, 4)
                            .background(.quaternary.opacity(0.6), in: RoundedRectangle(cornerRadius: 6))
                        Button(copied ? L("offline.copied") : L("offline.copy")) {
                            Mentions.copyText(Self.startCommand)
                            copied = true
                        }
                        .controlSize(.small)
                    }
                }
                .padding(.top, 6)
            }
        } actions: {
            Button(L("offline.retry")) { store.reconnect() }
                .buttonStyle(.borderedProminent)
                .keyboardShortcut("r")
        }
    }
}

/// The core could not start at all (a bad Senate home): nothing works until
/// it does.
struct SetupProblemView: View {
    let store: CoreStore
    let message: String

    var body: some View {
        ContentUnavailableView {
            Label(L("setup.title"), systemImage: "exclamationmark.triangle")
        } description: {
            Text(verbatim: message)
        } actions: {
            Button(L("setup.retry")) { store.connect() }
                .buttonStyle(.borderedProminent)
            SettingsLink { Text(verbatim: L("setup.settings")) }
        }
    }
}
