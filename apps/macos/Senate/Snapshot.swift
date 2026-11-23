#if DEBUG
import AppKit
import ScreenCaptureKit
import SwiftUI
import SenateCore

/// Development only: with `SENATE_APP_SNAPSHOT=<dir>`, open the window, show
/// the inbox, a thread, approvals, a committee, sessions and the Senates,
/// then Settings and the status item's menu; write each as
/// `<dir>/<name>.png`, and quit.
/// The app draws its own windows, so no screen-recording permission is
/// needed. `SENATE_APP_SNAPSHOT_PREFIX` names the files (`zh-inbox.png`);
/// `SENATE_APP_SNAPSHOT_APPEARANCE` (`light` or `dark`) draws the app in that
/// appearance whatever the system's is.
enum Snapshot {
    static let env = "SENATE_APP_SNAPSHOT"
    /// Posted to scroll the open session's conversation to its top.
    static let scrollToTop = Notification.Name("SenateSnapshotScrollToTop")
    /// Posted to open every tool call's output in the open conversation.
    static let expandTools = Notification.Name("SenateSnapshotExpandTools")

    nonisolated static let deskPresentation = Notification.Name("SenateSnapshotDeskPresentation")
    enum DeskPresentation { case delivered, failed, refusing, dismissRefusal }

    /// The window's size in every snapshot.
    static let size = NSSize(width: 1120, height: 740)

    /// The appearance asked for, if any.
    static var appearance: NSAppearance? {
        switch ProcessInfo.processInfo.environment["SENATE_APP_SNAPSHOT_APPEARANCE"] {
        case "dark": NSAppearance(named: .darkAqua)
        case "light": NSAppearance(named: .aqua)
        default: nil
        }
    }

    static var directory: URL? {
        ProcessInfo.processInfo.environment[env].flatMap { $0.isEmpty ? nil : URL(fileURLWithPath: $0) }
    }

    static func run(_ store: CoreStore, into dir: URL) {
        setvbuf(stdout, nil, _IOLBF, 0)
        let prefix = ProcessInfo.processInfo.environment["SENATE_APP_SNAPSHOT_PREFIX"] ?? ""
        if let appearance { NSApplication.shared.appearance = appearance }
        Task {
            try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            for _ in 0..<50 where !store.state.online {
                try? await Task.sleep(for: .milliseconds(200))
            }
            try? await Task.sleep(for: .seconds(1))
            // One size for every run, whatever frame the window last had.
            if let window = NSApplication.shared.windows.first(where: { isMain($0) }) {
                window.setContentSize(size)
                window.center()
            }
            func shot(_ name: String) async {
                try? await Task.sleep(for: .seconds(1.5))
                await capture(dir.appendingPathComponent("\(prefix)\(name).png")) { isMain($0) }
            }
            // Inbox, a thread, Approvals, a committee, sessions, Senates.
            store.place = .section(.inbox)
            await shot("inbox")
            if let thread = store.inboxThreads.first {
                store.place = .thread(thread.key)
                await shot("thread")
            }
            store.place = .section(.approvals)
            await shot("approvals")
            if let item = store.docketEntries.first(where: { $0.seq == 60 }) {
                func present(_ scene: DeskPresentation) {
                    NotificationCenter.default.post(name: deskPresentation, object: scene)
                }
                store.deskSelection = item
                await shot("desk-forward")
                present(.delivered)
                await shot("desk-delivered")
                present(.refusing)
                await shot("desk-refusal")
                present(.dismissRefusal)
                if let failing = store.docketEntries.first(where: { $0.seq == 61 }) {
                    store.deskSelection = failing
                    try? await Task.sleep(for: .seconds(1))
                    present(.failed)
                    await shot("desk-failed")
                }
                if let blocked = store.docketEntries.first(where: { $0.seq == 62 }) {
                    store.deskSelection = blocked
                    await shot("desk-read-only")
                }
                store.deskSelection = nil
            }
            if let busiest = store.roster.max(by: { $0.activeMembers < $1.activeMembers }) {
                store.place = .committee(busiest.committee)
                await shot("committee")
            }
            // A session at a prompt (its conversation, the prompt card), then
            // one with nothing to read; each as the sidebar lists it.
            let sessions = store.shownSessions
            if let prompt = sessions.first(where: { $0.status == .needsYou }) {
                store.place = .session(prompt.sessionId)
                try? await Task.sleep(for: .seconds(1.5))
                await shot("session")
            }
            // Scrolled to the top, the page before is read: what is on screen
            // stays where it is (compare the two pictures).
            if sessions.contains(where: { $0.status == .needsYou }) {
                NotificationCenter.default.post(name: scrollToTop, object: nil)
                try? await Task.sleep(for: .milliseconds(300))
                await capture(dir.appendingPathComponent("\(prefix)session-top.png")) { isMain($0) }
                await shot("session-older")
                // Every call open, the 64 KiB log and the 600-line diff with
                // them: how long the main thread is held.
                let start = ContinuousClock.now
                NotificationCenter.default.post(name: expandTools, object: nil)
                await Task.yield()
                print("snapshot: opening every tool call held the main thread \(ContinuousClock.now - start)")
                await shot("session-expanded")
            }
            if let working = sessions.first(where: { $0.status == .working && $0.readable }) {
                store.place = .session(working.sessionId)
                await shot("session-working")
            }
            if let unreadable = sessions.first(where: { !$0.readable }) {
                store.place = .session(unreadable.sessionId)
                await shot("session-unreadable")
            }
            store.place = .section(.senates)
            await shot("senates")
            // Settings, in its own window, opened as a person would: ⌘,.
            if let comma = NSEvent.keyEvent(
                with: .keyDown, location: .zero, modifierFlags: .command,
                timestamp: ProcessInfo.processInfo.systemUptime,
                windowNumber: NSApplication.shared.windows.first(where: { isMain($0) })?.windowNumber ?? 0,
                context: nil, characters: ",", charactersIgnoringModifiers: ",", isARepeat: false, keyCode: 43
            ) {
                NSApplication.shared.postEvent(comma, atStart: false)
            }
            try? await Task.sleep(for: .seconds(1.5))
            await capture(dir.appendingPathComponent("\(prefix)settings.png")) { window in
                !isMain(window) && window.frame.width > 300
            }
            await menu(store, dir, prefix)
            NSApplication.shared.terminate(nil)
        }
    }

    private static func isMain(_ window: NSWindow) -> Bool {
        window.identifier?.rawValue.contains(CoreStore.windowID) == true
    }

    /// The status item's menu. The status item itself is not a window of
    /// this process that can be captured or clicked from here, so the same
    /// `MenuContent` is opened from a menu button in a small window of the
    /// app's own: the same view, drawn by the same system menu.
    ///
    /// A menu is tracked in a run loop of its own and `performClick` returns
    /// only when it closes, so what captures it and closes it is started
    /// first.
    private static func menu(_ store: CoreStore, _ dir: URL, _ prefix: String) async {
        // The menu opens over this window, which is what its glass shows
        // through to in the picture.
        let host = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 560, height: 520),
            styleMask: [.borderless], backing: .buffered, defer: false
        )
        host.isReleasedWhenClosed = false
        host.contentView = NSHostingView(rootView:
            Menu {
                MenuContent(store: store)
            } label: {
                MenuBarLabel(store: store)
            }
            .menuStyle(.button)
            .menuIndicator(.hidden)
            .fixedSize()
            .padding(.top, 16)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
            .background(Theme.sidebar)
        )
        // High on the screen, so the menu opens downwards in full.
        if let screen = NSScreen.main {
            host.setFrameTopLeftPoint(NSPoint(x: screen.visibleFrame.midX - 280, y: screen.visibleFrame.maxY - 40))
        }
        host.makeKeyAndOrderFront(nil)
        try? await Task.sleep(for: .seconds(1))
        let known = Set(NSApplication.shared.windows.compactMap { CGWindowID(exactly: $0.windowNumber) })
        // If the menu cannot be closed from here, do not hang the run.
        DispatchQueue.global().asyncAfter(deadline: .now() + 15) {
            print("snapshot: the menu did not close")
            exit(1)
        }
        Task {
            try? await Task.sleep(for: .seconds(1))
            await captureMenuWindow(
                dir.appendingPathComponent("\(prefix)menu.png"), known: known,
                over: CGWindowID(exactly: host.windowNumber)
            )
            if let escape = NSEvent.keyEvent(
                with: .keyDown, location: .zero, modifierFlags: [],
                timestamp: ProcessInfo.processInfo.systemUptime, windowNumber: 0, context: nil,
                characters: "\u{1b}", charactersIgnoringModifiers: "\u{1b}", isARepeat: false, keyCode: 53
            ) {
                NSApplication.shared.postEvent(escape, atStart: true)
            }
        }
        // A click on the button, sent to the app's own window.
        let centre = NSPoint(x: host.contentLayoutRect.midX, y: host.contentLayoutRect.maxY - 28)
        for type in [NSEvent.EventType.leftMouseDown, .leftMouseUp] {
            if let click = NSEvent.mouseEvent(
                with: type, location: centre, modifierFlags: [],
                timestamp: ProcessInfo.processInfo.systemUptime, windowNumber: host.windowNumber,
                context: nil, eventNumber: 0, clickCount: 1, pressure: 1
            ) {
                host.sendEvent(click)
            }
        }
        // Where the click returns at once, the menu is open now.
        try? await Task.sleep(for: .seconds(2.5))
        host.close()
    }

    /// The one window of this process that appeared with the click: the
    /// menu, drawn over the window it opened from.
    private static func captureMenuWindow(_ url: URL, known: Set<CGWindowID>, over host: CGWindowID?) async {
        do {
            let content = try await SCShareableContent.currentProcess
            let pid = ProcessInfo.processInfo.processIdentifier
            let mine = content.windows.filter { $0.owningApplication?.processID == pid && $0.isOnScreen }
            let new = mine.filter {
                !known.contains($0.windowID) && $0.frame.width > 80 && $0.frame.height > 40
            }
            guard let menu = new.max(by: { $0.frame.height < $1.frame.height }) else {
                print("snapshot: the menu's window was not offered for \(url.lastPathComponent)")
                return
            }
            guard let backdrop = mine.first(where: { $0.windowID == host }),
                  let display = content.displays.first(where: { $0.frame.intersects(menu.frame) })
            else {
                try await write(SCContentFilter(desktopIndependentWindow: menu), to: url)
                return
            }
            // The menu and the status item's label above it, nothing else.
            let filter = SCContentFilter(display: display, including: [backdrop, menu])
            let area = menu.frame.insetBy(dx: -8, dy: -8).union(
                CGRect(x: menu.frame.minX - 8, y: backdrop.frame.minY, width: menu.frame.width + 16, height: 1)
            ).intersection(backdrop.frame)
            try await write(filter, to: url, area: area.offsetBy(dx: -display.frame.minX, dy: -display.frame.minY))
        } catch {
            print("snapshot: \(url.lastPathComponent): \(error.localizedDescription)")
        }
    }

    /// ScreenCaptureKit's view of this process's own windows needs no
    /// screen-recording permission, and draws what the screen shows (Liquid
    /// Glass included), which `cacheDisplay` does not.
    private static func capture(_ url: URL, _ which: @MainActor (NSWindow) -> Bool) async {
        do {
            // Exactly one of the app's own windows, by its window number:
            // never anything else on the screen.
            guard let mine = NSApplication.shared.windows.first(where: { $0.isVisible && which($0) }),
                  let id = CGWindowID(exactly: mine.windowNumber)
            else {
                print("snapshot: no window for \(url.lastPathComponent)")
                return
            }
            let content = try await SCShareableContent.currentProcess
            guard let window = content.windows.first(where: {
                $0.windowID == id
                    && $0.owningApplication?.processID == ProcessInfo.processInfo.processIdentifier
            }) else {
                print("snapshot: window \(id) not offered for \(url.lastPathComponent)")
                return
            }
            try await write(SCContentFilter(desktopIndependentWindow: window), to: url)
        } catch {
            print("snapshot: \(url.lastPathComponent): \(error.localizedDescription)")
        }
    }

    /// Write what `filter` shows (the part `area` of it, when given, in the
    /// filter's own coordinates) as a PNG.
    private static func write(_ filter: SCContentFilter, to url: URL, area: CGRect? = nil) async throws {
        let config = SCStreamConfiguration()
        let scale = CGFloat(filter.pointPixelScale)
        let rect = area ?? CGRect(origin: .zero, size: filter.contentRect.size)
        if let area { config.sourceRect = area }
        config.width = Int(rect.width * scale)
        config.height = Int(rect.height * scale)
        config.showsCursor = false
        config.ignoreShadowsSingleWindow = true
        let image = try await SCScreenshotManager.captureImage(contentFilter: filter, configuration: config)
        let rep = NSBitmapImageRep(cgImage: image)
        try rep.representation(using: .png, properties: [:])?.write(to: url)
        print("snapshot: wrote \(url.path)")
    }
}
#endif
