import Foundation
import os
import SenateCore
import UserNotifications

/// What a person did with a notification.
enum NotificationAction: Equatable, Sendable {
    case openThread(committee: String, root: UInt64)
    case reply(committee: String, root: UInt64, body: String)
    case take(committee: String, root: UInt64)
    case openApprovals
    case approve(id: String, allow: Bool)
    case openSessions
    /// Attach to the hosted session with this exact id (resolved when
    /// tapped: an alias may since have passed to a replacement).
    case attach(sessionId: String)
}

/// macOS notifications: something came back to you (Reply, Take), a Codex
/// approval request (Allow, Deny; both need the Mac unlocked), and a
/// permission prompt waiting in a hosted terminal (Attach; it is decided
/// there). Clicking one opens what it is about. Each kind can be turned
/// off in Settings; permission is asked the first time one is posted.
final class Notifier: NSObject, UNUserNotificationCenterDelegate {
    /// Set to `0` to keep the app from asking for (and posting) notifications,
    /// e.g. against the fake daemon.
    static let env = "SENATE_APP_NOTIFICATIONS"

    enum Kind: String, CaseIterable {
        case thread, approval, prompt

        /// The Settings toggle for this kind (on unless turned off).
        var defaultsKey: String { "notify.\(rawValue)" }
    }

    /// Where actions go; set by the store.
    var handle: ((NotificationAction) async -> Void)?

    private let enabled = ProcessInfo.processInfo.environment[Notifier.env] != "0"
    private let log = Logger(subsystem: Log.subsystem, category: "notifications")
    private var asked = false

    private enum Category {
        static let thread = "THREAD"
        static let approval = "CODEX_APPROVAL"
        static let prompt = "HOSTED_PROMPT"
    }

    private enum Action {
        static let reply = "REPLY"
        static let take = "TAKE"
        static let allow = "ALLOW"
        static let deny = "DENY"
        static let attach = "ATTACH"
    }

    func setUp() {
        guard enabled else { return }
        let center = UNUserNotificationCenter.current()
        center.delegate = self
        center.setNotificationCategories([
            UNNotificationCategory(
                identifier: Category.thread,
                actions: [
                    UNTextInputNotificationAction(
                        identifier: Action.reply,
                        title: L("notify.reply"),
                        options: [],
                        textInputButtonTitle: L("inbox.send"),
                        textInputPlaceholder: L("inbox.reply_placeholder")
                    ),
                    UNNotificationAction(identifier: Action.take, title: L("inbox.take"), options: []),
                ],
                intentIdentifiers: []
            ),
            UNNotificationCategory(
                identifier: Category.approval,
                actions: [
                    UNNotificationAction(
                        identifier: Action.allow,
                        title: L("approvals.allow"),
                        options: [.authenticationRequired]
                    ),
                    UNNotificationAction(
                        identifier: Action.deny,
                        title: L("approvals.deny"),
                        options: [.authenticationRequired, .destructive]
                    ),
                ],
                intentIdentifiers: []
            ),
            UNNotificationCategory(
                identifier: Category.prompt,
                actions: [
                    UNNotificationAction(
                        identifier: Action.attach,
                        title: L("prompt.attach"),
                        options: [.foreground]
                    ),
                ],
                intentIdentifiers: []
            ),
        ])
    }

    /// Ask for permission (Settings' button, or the first notification).
    @discardableResult
    func requestAuthorization() async -> Bool {
        do {
            return try await UNUserNotificationCenter.current()
                .requestAuthorization(options: [.alert, .sound, .badge])
        } catch {
            log.error("notifications unavailable: \(error.localizedDescription, privacy: .public)")
            return false
        }
    }

    /// Whether notifications are allowed for the app, for Settings.
    func authorized() async -> Bool {
        let settings = await UNUserNotificationCenter.current().notificationSettings()
        return settings.authorizationStatus == .authorized
            || settings.authorizationStatus == .provisional
    }

    func forPerson(_ note: PersonNote) {
        let content = UNMutableNotificationContent()
        content.title = note.title
        content.body = note.body
        content.sound = .default
        content.categoryIdentifier = Category.thread
        content.threadIdentifier = "\(note.committee):\(note.thread)"
        content.userInfo = ["committee": note.committee, "root": note.thread]
        post(.thread, id: "person:\(note.committee):\(note.thread):\(UUID().uuidString)", content)
    }

    func approvalRequested(_ approval: ApprovalRequest) {
        let content = UNMutableNotificationContent()
        content.title = L("notify.codex_approval")
        content.body = [approval.summary, approval.reason]
            .compactMap { $0 }
            .joined(separator: "\n")
        content.sound = .default
        content.categoryIdentifier = Category.approval
        content.threadIdentifier = "approvals"
        content.interruptionLevel = .timeSensitive
        content.userInfo = ["approval": approval.id]
        post(.approval, id: "approval:\(approval.id)", content)
    }

    func promptWaiting(_ session: HostedSession) {
        let content = UNMutableNotificationContent()
        content.title = L("notify.prompt", session.label)
        content.body = session.promptTool ?? L("notify.prompt_body")
        content.sound = .default
        content.categoryIdentifier = Category.prompt
        content.threadIdentifier = "prompts"
        content.interruptionLevel = .timeSensitive
        // The exact session, never its alias: `senate session replace` hands
        // the alias to a new session, and this banner is about this one.
        content.userInfo = ["session": session.sessionId]
        post(.prompt, id: "prompt:\(session.sessionId)", content)
    }

    /// The approval was answered (here or elsewhere): take its banner down.
    func withdrawApproval(_ id: String) {
        guard enabled else { return }
        UNUserNotificationCenter.current().removeDeliveredNotifications(withIdentifiers: ["approval:\(id)"])
    }

    private func post(_ kind: Kind, id: String, _ content: UNMutableNotificationContent) {
        guard enabled, UserDefaults.standard.object(forKey: kind.defaultsKey) as? Bool ?? true else {
            return
        }
        Task {
            if !asked {
                asked = true
                await requestAuthorization()
            }
            do {
                try await UNUserNotificationCenter.current()
                    .add(UNNotificationRequest(identifier: id, content: content, trigger: nil))
            } catch {
                log.error("posting a notification: \(error.localizedDescription, privacy: .public)")
            }
        }
    }

    // MARK: UNUserNotificationCenterDelegate

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
        guard let action = Self.action(of: response) else { return }
        await dispatch(action)
    }

    private func dispatch(_ action: NotificationAction) async {
        await handle?(action)
    }

    /// What the person did, from the notification's payload.
    nonisolated static func action(of response: UNNotificationResponse) -> NotificationAction? {
        let info = response.notification.request.content.userInfo
        let text = (response as? UNTextInputNotificationResponse)?.userText
        return action(
            category: response.notification.request.content.categoryIdentifier,
            action: response.actionIdentifier,
            info: info,
            text: text
        )
    }

    /// The mapping itself, apart from UserNotifications' types (tests).
    nonisolated static func action(
        category: String,
        action: String,
        info: [AnyHashable: Any],
        text: String?
    ) -> NotificationAction? {
        switch category {
        case Category.thread:
            guard let committee = info["committee"] as? String,
                  let root = (info["root"] as? NSNumber)?.uint64Value
            else { return nil }
            switch action {
            case Action.reply:
                guard let text, !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
                    return .openThread(committee: committee, root: root)
                }
                return .reply(committee: committee, root: root, body: text)
            case Action.take: return .take(committee: committee, root: root)
            case UNNotificationDismissActionIdentifier: return nil
            default: return .openThread(committee: committee, root: root)
            }
        case Category.approval:
            guard let id = info["approval"] as? String else { return .openApprovals }
            switch action {
            case Action.allow: return .approve(id: id, allow: true)
            case Action.deny: return .approve(id: id, allow: false)
            case UNNotificationDismissActionIdentifier: return nil
            default: return .openApprovals
            }
        case Category.prompt:
            guard let id = info["session"] as? String else { return .openSessions }
            switch action {
            case Action.attach: return .attach(sessionId: id)
            case UNNotificationDismissActionIdentifier: return nil
            default: return .openSessions
            }
        default:
            return nil
        }
    }
}
