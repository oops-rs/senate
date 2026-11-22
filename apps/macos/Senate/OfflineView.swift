import SwiftUI
import SenateCore

/// What the window shows while the daemon cannot be reached: why, that the
/// app keeps trying on its own, how to start the daemon, and a way to try
/// now. The app never starts it itself.
struct OfflineView: View {
    let store: CoreStore
    @State private var copied = false

    static let startCommand = "senate setup service install --yes"

    var body: some View {
        CenteredPage(width: 520) {
          VStack(spacing: Theme.Space.xl) {
            VStack(spacing: Theme.Space.s) {
                Text(verbatim: L("offline.title"))
                    .font(Theme.Fonts.display)
                    .foregroundStyle(Theme.onSurface)
                    .accessibilityAddTraits(.isHeader)
                if let reason = store.state.offlineReason {
                    Text(verbatim: reason)
                        .font(Theme.Fonts.text)
                        .foregroundStyle(Theme.onSurface)
                        .textSelection(.enabled)
                }
                Text(verbatim: L("offline.retrying"))
                    .font(Theme.Fonts.text)
                    .foregroundStyle(Theme.onSurfaceVariant)
            }
            .multilineTextAlignment(.center)
            VStack(alignment: .leading, spacing: Theme.Space.s) {
                Text(verbatim: L("offline.start_hint"))
                    .font(Theme.Fonts.row)
                    .foregroundStyle(Theme.onSurfaceVariant)
                    .fixedSize(horizontal: false, vertical: true)
                HStack(spacing: Theme.Space.s) {
                    CodeBlock(text: Self.startCommand)
                    Button(copied ? L("offline.copied") : L("offline.copy")) {
                        Mentions.copyText(Self.startCommand)
                        copied = true
                    }
                    .buttonStyle(.pill)
                }
            }
            .card()
            Button(L("offline.retry")) { store.reconnect() }
                .buttonStyle(.pill(.filled))
          }
        }
    }
}

/// The core could not start at all (a bad Senate home): nothing works until
/// it does.
struct SetupProblemView: View {
    let store: CoreStore
    let message: String

    var body: some View {
        CenteredPage(width: 520) {
          VStack(spacing: Theme.Space.xl) {
            VStack(spacing: Theme.Space.s) {
                Text(verbatim: L("setup.title"))
                    .font(Theme.Fonts.display)
                    .foregroundStyle(Theme.onSurface)
                    .accessibilityAddTraits(.isHeader)
                Text(verbatim: message)
                    .font(Theme.Fonts.text)
                    .foregroundStyle(Theme.onSurfaceVariant)
                    .textSelection(.enabled)
            }
            .multilineTextAlignment(.center)
            HStack(spacing: Theme.Space.s) {
                Button(L("setup.retry")) { store.connect() }
                    .buttonStyle(.pill(.filled))
                SettingsLink { Text(verbatim: L("setup.settings")) }
                    .buttonStyle(.pill)
            }
          }
        }
    }
}
