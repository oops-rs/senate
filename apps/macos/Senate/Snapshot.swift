#if DEBUG
import AppKit
import ScreenCaptureKit
import SenateCore

/// Development only: with `SENATE_APP_SNAPSHOT=<dir>`, open the window, show
/// each section in turn, write the window as `<dir>/<section>.png`, and quit.
/// The app draws its own window, so no screen-recording permission is
/// needed. `SENATE_APP_SNAPSHOT_PREFIX` names the files (`zh-inbox.png`).
enum Snapshot {
    static let env = "SENATE_APP_SNAPSHOT"

    static var directory: URL? {
        ProcessInfo.processInfo.environment[env].flatMap { $0.isEmpty ? nil : URL(fileURLWithPath: $0) }
    }

    static func run(_ store: CoreStore, into dir: URL) {
        let prefix = ProcessInfo.processInfo.environment["SENATE_APP_SNAPSHOT_PREFIX"] ?? ""
        Task {
            try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            for _ in 0..<50 where !store.state.online {
                try? await Task.sleep(for: .milliseconds(200))
            }
            try? await Task.sleep(for: .seconds(1))
            for section in store.sections {
                store.section = section
                if section == .inbox {
                    store.selectedThread = store.state.inbox.first?.key
                }
                try? await Task.sleep(for: .seconds(1.5))
                await capture(dir.appendingPathComponent("\(prefix)\(section.rawValue).png"))
            }
            NSApplication.shared.terminate(nil)
        }
    }

    /// ScreenCaptureKit's view of this process's own windows needs no
    /// screen-recording permission, and draws what the screen shows (Liquid
    /// Glass included), which `cacheDisplay` does not.
    private static func capture(_ url: URL) async {
        do {
            // Exactly the app's main window, by its window number: never
            // anything else on the screen.
            guard let mine = NSApplication.shared.windows.first(where: {
                $0.isVisible && $0.identifier?.rawValue.contains(CoreStore.windowID) == true
            }) ?? NSApplication.shared.windows.first(where: { $0.isVisible && $0.frame.width > 400 })
            else {
                print("snapshot: no window for \(url.lastPathComponent)")
                return
            }
            let content = try await SCShareableContent.currentProcess
            guard let window = content.windows.first(where: {
                $0.windowID == CGWindowID(mine.windowNumber)
                    && $0.owningApplication?.processID == ProcessInfo.processInfo.processIdentifier
            }) else {
                print("snapshot: window \(mine.windowNumber) not offered for \(url.lastPathComponent)")
                return
            }
            let filter = SCContentFilter(desktopIndependentWindow: window)
            let config = SCStreamConfiguration()
            let scale = CGFloat(filter.pointPixelScale)
            config.width = Int(filter.contentRect.width * scale)
            config.height = Int(filter.contentRect.height * scale)
            config.showsCursor = false
            config.ignoreShadowsSingleWindow = true
            let image = try await SCScreenshotManager.captureImage(contentFilter: filter, configuration: config)
            let rep = NSBitmapImageRep(cgImage: image)
            try rep.representation(using: .png, properties: [:])?.write(to: url)
            print("snapshot: wrote \(url.path)")
        } catch {
            print("snapshot: \(url.lastPathComponent): \(error.localizedDescription)")
        }
    }
}
#endif
