import AppKit
import SwiftUI
import SenateCore

/// The Senate menu-bar app: a status item with the number waiting on you
/// and a menu of just those items, and one window with everything else
/// (the threads, the approvals, the sessions, the docket of a Senate your
/// agent presides over, the Senates served, Settings).
/// Everything shown comes from the Rust core (`CoreStore`).
@main
struct SenateApp: App {
    @State private var store = CoreStore.forApp()

    init() {
        Diagnostics.shared.start()
        Perf.watchMainThread()
    }

    var body: some Scene {
        MenuBarExtra {
            MenuContent(store: store)
        } label: {
            MenuBarLabel(store: store)
        }
        // The window waits to be opened from the menu.
        Window("Senate", id: CoreStore.windowID) {
            MainWindow(store: store)
                .frame(minWidth: 760, minHeight: 460)
        }
        .defaultSize(width: 1120, height: 740)
        .windowToolbarStyle(.unified)
        .defaultLaunchBehavior(.suppressed)

        Settings {
            SettingsView(store: store)
        }
    }
}

/// The status item: an icon, and the number waiting on you next to it
/// (threads, Codex approvals and terminal prompts) when there is any.
struct MenuBarLabel: View {
    let store: CoreStore
    @Environment(\.openWindow) private var openWindow
    @Environment(\.openSettings) private var openSettings

    var body: some View {
        let count = store.state.online ? store.waitingCount : 0
        HStack(spacing: 3) {
            Image(systemName: store.state.online
                ? "building.columns.fill"
                : "building.columns")
            if count > 0 {
                Text(verbatim: String(count))
            }
        }
        .help(store.state.menu.tooltip)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Senate")
        .accessibilityValue(store.waitingMenu().header)
        .task {
            // The label is the one view alive from launch: hand the store
            // the window opener, for notifications and `SENATE_APP_SHOW`.
            store.attach(openWindow: openWindow, openSettings: openSettings)
        }
    }
}

/// The status item's menu: what waits on you and nothing else. One line
/// saying how much, the first few items (each opens in the window), then
/// Open Senate and Quit. Settings, About and everything about the
/// connection live in the window.
struct MenuContent: View {
    let store: CoreStore

    var body: some View {
        let menu = store.waitingMenu()
        Text(verbatim: menu.header)
        ForEach(menu.rows) { row in
            Button {
                store.open(row.item)
            } label: {
                Text(verbatim: row.title)
                Text(verbatim: row.subtitle)
            }
        }
        if menu.truncated {
            Button(L("menu.show_all", Int64(menu.total))) { store.showHome() }
        }
        Divider()
        Button(L("menu.open")) { store.showWindow() }
            .keyboardShortcut("o")
        Button(L("menu.quit")) { NSApplication.shared.terminate(nil) }
            .keyboardShortcut("q")
    }
}

/// The standard About panel, with what Senate is.
enum About {
    static func show() {
        NSApplication.shared.activate()
        NSApplication.shared.orderFrontStandardAboutPanel(options: [
            .credits: NSAttributedString(
                string: L("about.credits"),
                attributes: [
                    .font: NSFont.systemFont(ofSize: NSFont.smallSystemFontSize),
                    .foregroundColor: NSColor.secondaryLabelColor,
                ]
            ),
        ])
    }
}
