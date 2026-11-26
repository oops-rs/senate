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
                        (id: $0.id, reason: $0.reason)
                    }
                }.value
                shown.formIntersection(prompts.map(\.id))
                if !Task.isCancelled, let prompt = prompts.first(where: { !shown.contains($0.id) }) {
                    shown.insert(prompt.id)
                    let context = LAContext()
                    context.touchIDAuthenticationAllowableReuseDuration = 0
                    let allowed = (try? await context.evaluatePolicy(
                        .deviceOwnerAuthentication, localizedReason: prompt.reason
                    )) == true
                    let approved = allowed && !Task.isCancelled
                    _ = await Task.detached(priority: .utility) {
                        resolvePersonApproval(home: home, id: prompt.id, approved: approved)
                    }.value
                }
            }
            try? await Task.sleep(for: .seconds(1))
        }
    }
}
