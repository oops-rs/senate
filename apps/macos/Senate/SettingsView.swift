import AppKit
import ServiceManagement
import LocalAuthentication
import SwiftUI
import SenateCore

/// Settings (⌘,, or the sidebar's account row), one pane: the Dock, launch at login,
/// notifications, the Senate home and the logs.
struct SettingsView: View {
    let store: CoreStore
    @State private var login = LoginItem()
    @State private var notificationsAllowed: Bool?
    @AppStorage(Notifier.Kind.thread.defaultsKey) private var notifyThreads = true
    @AppStorage(Notifier.Kind.approval.defaultsKey) private var notifyApprovals = true
    @AppStorage(Notifier.Kind.prompt.defaultsKey) private var notifyPrompts = true
    @AppStorage(CoreStore.homeKey) private var home = ""
    @State private var draft = ""
    @State private var requireApprovalTouch = false
    @State private var updatingSecurity = false
    @State private var securityNotice: String?

    var body: some View {
        Form {
            Section {
                Toggle(isOn: Bindable(store).showInDock) {
                    Text(verbatim: L("settings.show_in_dock"))
                    Text(verbatim: L("settings.show_in_dock_help"))
                }
                Toggle(L("settings.login"), isOn: Binding(
                    get: { login.enabled },
                    set: { login.set($0) }
                ))
                if let note = login.note {
                    Text(verbatim: note).font(.callout).foregroundStyle(.secondary)
                }
            }
            Section {
                Toggle(L("settings.notify_threads"), isOn: $notifyThreads)
                Toggle(L("settings.notify_approvals"), isOn: $notifyApprovals)
                Toggle(L("settings.notify_prompts"), isOn: $notifyPrompts)
                if notificationsAllowed == false {
                    HStack {
                        Text(verbatim: L("settings.notify_off"))
                            .font(.callout).foregroundStyle(.secondary)
                        Spacer()
                        Button(L("settings.notify_allow")) {
                            Task {
                                if await store.requestNotifications() {
                                    notificationsAllowed = true
                                } else {
                                    Self.openNotificationSettings()
                                }
                            }
                        }
                    }
                }
            } header: {
                Text(verbatim: L("settings.notifications"))
            }
            Section("Security") {
                Toggle("Require Touch ID for approvals in Senate", isOn: Binding(
                    get: { requireApprovalTouch },
                    set: { value in Task { await updateApprovalTouch(value) } }
                ))
                .disabled(store.senateHome == nil || updatingSecurity)
                Toggle(isOn: Binding(
                    get: { store.state.unsafeIgnoreExecutionAuth },
                    set: { value in Task { _ = await store.setUnsafeIgnoreExecutionAuth(value) } }
                )) {
                    Text(verbatim: L("settings.unsafe_ignore_auth"))
                    Text(verbatim: L("settings.unsafe_ignore_auth_help"))
                }
                .disabled(!store.state.online || store.isBusy("unsafe-mode"))
                if let securityNotice {
                    Text(verbatim: securityNotice).font(.callout).foregroundStyle(.secondary)
                }
            }
            Section {
                LabeledContent(L("settings.home_in_use")) {
                    Text(verbatim: store.senateHome ?? "—")
                        .textSelection(.enabled)
                        .foregroundStyle(.secondary)
                }
                HStack {
                    TextField(L("settings.home"), text: $draft, prompt: Text(verbatim: L("settings.home_default")))
                    Button(L("settings.choose")) { choose() }
                }
                HStack {
                    Spacer()
                    Button(L("settings.home_reset")) {
                        draft = ""
                        apply()
                    }
                    .disabled(home.isEmpty)
                    Button(L("settings.home_apply")) { apply() }
                        .disabled(draft == home)
                        .keyboardShortcut(.defaultAction)
                }
            } header: {
                Text(verbatim: L("settings.home_section"))
            } footer: {
                Text(verbatim: L("settings.home_note"))
            }
            Section {
                LabeledContent(L("settings.logs")) {
                    Button(L("settings.logs_open")) { store.openLogStream() }
                }
                Text(verbatim: L("settings.logs_note"))
                    .font(.callout).foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .tint(Theme.primary)
        .frame(width: 520)
        .fixedSize(horizontal: false, vertical: true)
        .task { notificationsAllowed = await store.notificationsAuthorized() }
        .task(id: store.senateHome) {
            if let home = store.senateHome {
                let security = await Task.detached { personSecurityStatus(home: home) }.value
                requireApprovalTouch = security.approvalTouchRequired
                securityNotice = security.notice
            }
        }
        .onAppear {
            login.refresh()
            draft = home
        }
    }

    @MainActor
    private func updateApprovalTouch(_ required: Bool) async {
        guard let home = store.senateHome, !updatingSecurity else { return }
        updatingSecurity = true
        defer { updatingSecurity = false }
        if !required {
            let context = LAContext()
            context.touchIDAuthenticationAllowableReuseDuration = 0
            guard (try? await context.evaluatePolicy(
                .deviceOwnerAuthentication,
                localizedReason: "Turn off Touch ID for approvals in Senate"
            )) == true else { return }
        }
        let changed = await Task.detached {
            setPersonApprovalTouch(home: home, required: required)
        }.value
        if changed {
            requireApprovalTouch = required
            securityNotice = nil
        } else {
            securityNotice = "Senate could not verify the installed app or approval policy."
        }
    }

    static func openNotificationSettings() {
        if let url = URL(string: "x-apple.systempreferences:com.apple.Notifications-Settings.extension") {
            NSWorkspace.shared.open(url)
        }
    }

    private func apply() {
        home = draft.trimmingCharacters(in: .whitespaces)
        store.restart()
    }

    private func choose() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.showsHiddenFiles = true
        panel.directoryURL = URL(fileURLWithPath: draft.isEmpty ? NSHomeDirectory() + "/.senate" : draft)
        if panel.runModal() == .OK, let url = panel.url {
            draft = url.path
        }
    }
}

/// Launch at login, through the app's own login item (SMAppService).
@Observable
final class LoginItem {
    private(set) var enabled = false
    private(set) var note: String?

    init() { refresh() }

    func refresh() {
        let status = SMAppService.mainApp.status
        enabled = status == .enabled || status == .requiresApproval
        note = status == .requiresApproval ? L("settings.login_approve") : nil
    }

    func set(_ on: Bool) {
        do {
            if on {
                try SMAppService.mainApp.register()
            } else {
                try SMAppService.mainApp.unregister()
            }
            refresh()
        } catch {
            Log.app.error("login item: \(error.localizedDescription, privacy: .public)")
            refresh()
            note = L("settings.login_failed", error.localizedDescription)
        }
    }
}
