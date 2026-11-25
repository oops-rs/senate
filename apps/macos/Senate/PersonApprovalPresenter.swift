import AppKit
import LocalAuthentication
import SenateCore

@MainActor
enum PersonApprovalPresenter {
    static func watch(store: CoreStore) async {
        var shown = Set<String>()
        var lastNotice: String?
        while !Task.isCancelled {
            if let home = store.senateHome, NSApp.modalWindow == nil {
                let policy = await Task.detached(priority: .utility) {
                    personSecurityStatus(home: home)
                }.value
                if let notice = policy.notice, notice != lastNotice {
                    lastNotice = notice
                    let alert = NSAlert()
                    alert.alertStyle = .warning
                    alert.messageText = notice
                    alert.informativeText = "A sensitive request will require macOS authentication before Senate accepts the change."
                    alert.runModal()
                } else if policy.notice == nil {
                    lastNotice = nil
                }
                let prompts = await Task.detached(priority: .utility) {
                    personApprovalPrompts(home: home).map {
                        (id: $0.id, display: $0.display, reason: $0.reason, age: $0.ageSeconds)
                    }
                }.value
                shown.formIntersection(prompts.map(\.id))
                if !Task.isCancelled, let prompt = prompts.first(where: { !shown.contains($0.id) }) {
                    shown.insert(prompt.id)
                    var allowed = false
                    let queue = "Requested \(prompt.age)s ago. \(max(0, prompts.count - 1)) other requests waiting."
                    let origin = "The foreground terminal could not be verified. Match the requester below to the command you ran."
                    if confirm(details: "\(queue)\n\(origin)\n\n\(prompt.display)") && !Task.isCancelled {
                        let context = LAContext()
                        context.touchIDAuthenticationAllowableReuseDuration = 0
                        allowed = (try? await context.evaluatePolicy(
                            .deviceOwnerAuthentication, localizedReason: prompt.reason
                        )) == true
                    }
                    let approved = allowed && !Task.isCancelled
                    _ = await Task.detached(priority: .utility) {
                        resolvePersonApproval(home: home, id: prompt.id, approved: approved)
                    }.value
                }
            }
            try? await Task.sleep(for: .seconds(1))
        }
    }

    private static func confirm(details: String) -> Bool {
        let alert = NSAlert()
        alert.messageText = L("approvals.person_confirmation")
        alert.alertStyle = .warning
        alert.addButton(withTitle: L("approvals.deny"))
        let allow = alert.addButton(withTitle: L("approvals.allow"))
        allow.keyEquivalent = ""

        let scroll = NSScrollView(frame: NSRect(x: 0, y: 0, width: 450, height: 240))
        let text = NSTextView(frame: scroll.bounds)
        text.isEditable = false
        text.isHorizontallyResizable = false
        text.isVerticallyResizable = true
        text.font = .monospacedSystemFont(ofSize: 12, weight: .regular)
        text.string = details
        scroll.documentView = text
        scroll.hasVerticalScroller = true
        alert.accessoryView = scroll
        NSApp.activate()
        return alert.runModal() == .alertSecondButtonReturn
    }
}
