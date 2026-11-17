import Foundation
import UserNotifications

/// macOS notifications for Codex approval requests. Clicking one opens the
/// Approvals tab.
@MainActor
final class Notifier: NSObject, UNUserNotificationCenterDelegate {
    /// Set to `0` to keep the app from asking for (and posting) notifications,
    /// e.g. against the fake daemon.
    static let env = "SENATE_APP_NOTIFICATIONS"

    var onOpen: (() -> Void)?
    private let enabled = ProcessInfo.processInfo.environment[Notifier.env] != "0"

    func setUp() {
        guard enabled else { return }
        let center = UNUserNotificationCenter.current()
        center.delegate = self
        center.requestAuthorization(options: [.alert, .sound]) { _, error in
            if let error {
                NSLog("Senate: notifications unavailable: \(error.localizedDescription)")
            }
        }
    }

    func approvalRequested(_ approval: ApprovalRequest) {
        guard enabled else { return }
        let content = UNMutableNotificationContent()
        content.title = L("notify.codex_approval")
        content.body = [approval.summary, approval.reason]
            .compactMap { $0 }
            .joined(separator: "\n")
        content.sound = .default
        let request = UNNotificationRequest(identifier: approval.id, content: content, trigger: nil)
        UNUserNotificationCenter.current().add(request) { error in
            if let error {
                NSLog("Senate: notification: \(error.localizedDescription)")
            }
        }
    }

    /// Something came back to the person. The daemon posts no notification
    /// while this app is connected, so this is the one they get.
    func forPerson(_ note: PersonNote) {
        guard enabled else { return }
        let content = UNMutableNotificationContent()
        content.title = note.title
        content.body = note.body
        content.sound = .default
        let request = UNNotificationRequest(
            identifier: "person:\(note.committee):\(note.thread):\(UUID().uuidString)",
            content: content,
            trigger: nil
        )
        UNUserNotificationCenter.current().add(request) { error in
            if let error {
                NSLog("Senate: notification: \(error.localizedDescription)")
            }
        }
    }

    // Shown even while the app is frontmost.
    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification
    ) async -> UNNotificationPresentationOptions {
        [.banner, .list, .sound]
    }

    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse
    ) async {
        await MainActor.run { self.onOpen?() }
    }
}
