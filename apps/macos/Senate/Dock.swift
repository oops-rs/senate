import AppKit
import SwiftUI

/// Whether Senate shows in the Dock, and what that makes of it.
///
/// The bundle declares `LSUIElement` (menu-bar only), so nothing is in the
/// Dock until the app says so. At launch, in `applicationWillFinishLaunching`
/// (before the Dock is told the app is running), the setting picks the
/// policy: shown in the Dock (on, the default, also when nothing is stored
/// yet) makes it a regular app from its first frame, with a Dock icon, the
/// app switcher and a main menu; off leaves it menu-bar only, with no icon
/// flashing at launch. Changing the setting applies at once.
enum Dock {
    nonisolated static let key = "showInDock"

    /// The setting; on when nothing is stored (people upgrading included).
    nonisolated static func showsInDock(_ defaults: UserDefaults = .standard) -> Bool {
        defaults.object(forKey: key) == nil ? true : defaults.bool(forKey: key)
    }

    /// The activation policy for the setting.
    nonisolated static func policy(showInDock: Bool) -> NSApplication.ActivationPolicy {
        showInDock ? .regular : .accessory
    }

    /// Apply the setting to the running app.
    static func apply(showInDock: Bool) {
        let app = NSApplication.shared
        guard app.activationPolicy() != policy(showInDock: showInDock) else { return }
        app.setActivationPolicy(policy(showInDock: showInDock))
        if showInDock {
            // Made regular while running: come forward, so the menu bar is
            // Senate's and the windows stay in front.
            app.activate()
        }
    }

    /// The number beside the Dock icon: what waits on you, as beside the
    /// status item; none at zero.
    static func badge(_ count: Int) {
        let label = count > 0 ? String(count) : nil
        if NSApplication.shared.dockTile.badgeLabel != label {
            NSApplication.shared.dockTile.badgeLabel = label
        }
    }
}

/// The app's delegate: the policy before launch, the window on a Dock click,
/// and the status item kept when the last window closes.
final class AppDelegate: NSObject, NSApplicationDelegate {
    /// Set by `SenateApp` once its store exists.
    var store: CoreStore?

    func applicationWillFinishLaunching(_ notification: Notification) {
        NSApplication.shared.setActivationPolicy(Dock.policy(showInDock: Dock.showsInDock()))
    }

    /// A click on the Dock icon brings the window back.
    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows: Bool) -> Bool {
        if !hasVisibleWindows { store?.showWindow() }
        return true
    }

    /// Closing the window leaves the app running in the menu bar; ⌘Q quits.
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }
}

/// The main menu, while Senate is in the Dock. About with Senate's own
/// credits; the View menu's sections, Refresh and Find. Settings…, Quit,
/// Close, Edit (copy, paste, select all), Window and Help are the system's.
/// The same shortcuts work menu-bar only, from the window's own buttons.
struct SenateCommands: Commands {
    let store: CoreStore

    var body: some Commands {
        CommandGroup(replacing: .appInfo) {
            Button(L("menu.about")) { About.show() }
        }
        CommandGroup(before: .sidebar) {
            ForEach(Array(CoreStore.Section.allCases.enumerated()), id: \.element) { index, section in
                Button(section.title) {
                    store.place = .section(section)
                    store.showWindow()
                }
                .keyboardShortcut(KeyEquivalent(Character(String(index + 1))), modifiers: .command)
            }
            Divider()
            Button(L("toolbar.refresh")) {
                if store.state.online { store.refresh() } else { store.reconnect() }
            }
            .keyboardShortcut("r")
            Button(L("sidebar.search")) {
                store.showWindow()
                store.searchRequests += 1
            }
            .keyboardShortcut("f")
            Divider()
        }
    }
}
