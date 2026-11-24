import AppKit
import SenateCore

@MainActor
enum PersonApprovalPresenter {
    static func watch(store: CoreStore) async {
        var shown = Set<String>()
        while !Task.isCancelled {
            if let home = store.senateHome, NSApp.modalWindow == nil {
                let prompts = await Task.detached(priority: .utility) {
                    personApprovalPrompts(home: home).map { (id: $0.id, display: $0.display) }
                }.value
                shown.formIntersection(prompts.map(\.id))
                if !Task.isCancelled, let prompt = prompts.first(where: { !shown.contains($0.id) }) {
                    shown.insert(prompt.id)
                    let allowed = confirm(details: prompt.display)
                    _ = await Task.detached(priority: .utility) {
                        resolvePersonApproval(home: home, id: prompt.id, approved: allowed)
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
