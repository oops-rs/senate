import AppKit
import SwiftUI
import SenateCore

/// The Senate menu-bar app: a status item with the number waiting on you and
/// a menu, and one window with the inbox, the approvals, the Codex threads,
/// the docket of a Senate your agent presides over, and the Senates served.
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
        .defaultSize(width: 980, height: 660)
        .windowToolbarStyle(.unified)
        .defaultLaunchBehavior(.suppressed)

        Settings {
            SettingsView(store: store)
        }
    }
}

/// The status item: an icon, and the number waiting on you next to it.
struct MenuBarLabel: View {
    let store: CoreStore
    @Environment(\.openWindow) private var openWindow
    @Environment(\.openSettings) private var openSettings

    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: store.state.online
                ? "building.columns.fill"
                : "building.columns")
            if let badge = store.state.menu.badge {
                Text(verbatim: badge)
            }
        }
        .help(store.state.menu.tooltip)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Senate")
        .accessibilityValue(store.state.menu.tooltip)
        .task {
            // The label is the one view alive from launch: hand the store
            // the window opener, for notifications and `SENATE_APP_SHOW`.
            store.attach(openWindow: openWindow, openSettings: openSettings)
        }
    }
}

/// The status item's menu.
struct MenuContent: View {
    let store: CoreStore

    var body: some View {
        Text(verbatim: store.state.menu.title)
        Text(verbatim: store.state.menu.pending)
        if let docket = store.state.menu.docket {
            Text(verbatim: docket)
        }
        if let prompts = store.state.menu.prompts {
            Button(prompts) { store.show(.sessions) }
        }
        if let setup = store.setupError {
            Text(verbatim: L("menu.setup", setup))
        }
        if !store.state.online {
            Button(L("offline.retry")) { store.reconnect() }
        }
        if !store.state.menu.lines.isEmpty {
            Divider()
            ForEach(store.state.menu.lines, id: \.self) { line in
                Text(verbatim: line)
            }
        }
        Divider()
        Button(L("menu.open_inbox")) { store.show(.inbox) }
            .keyboardShortcut("i")
        Button(L("menu.open_approvals")) { store.show(.approvals) }
            .keyboardShortcut("a")
        Button(L("menu.open_sessions")) { store.show(.sessions) }
            .keyboardShortcut("s")
        if store.state.menu.docket != nil {
            Button(L("menu.open_docket")) { store.show(.docket) }
                .keyboardShortcut("d")
        }
        Divider()
        SettingsLink { Text(verbatim: L("menu.settings")) }
            .keyboardShortcut(",")
        Button(L("menu.about")) { About.show() }
        Divider()
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
