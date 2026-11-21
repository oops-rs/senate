import AppKit
import ServiceManagement
import SwiftUI
import SenateCore

/// Settings (⌘,): launch at login and notifications; the Senate home and
/// the logs under Advanced.
struct SettingsView: View {
    let store: CoreStore

    var body: some View {
        TabView {
            Tab(L("settings.general"), systemImage: "gearshape") {
                GeneralSettings(store: store)
            }
            Tab(L("settings.advanced"), systemImage: "wrench.and.screwdriver") {
                AdvancedSettings(store: store)
            }
        }
        .scenePadding()
        .frame(width: 520)
    }
}

private struct GeneralSettings: View {
    let store: CoreStore
    @State private var login = LoginItem()
    @State private var notificationsAllowed: Bool?
    @AppStorage(Notifier.Kind.thread.defaultsKey) private var notifyThreads = true
    @AppStorage(Notifier.Kind.approval.defaultsKey) private var notifyApprovals = true
    @AppStorage(Notifier.Kind.prompt.defaultsKey) private var notifyPrompts = true

    var body: some View {
        Form {
            Section {
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
        }
        .formStyle(.grouped)
        .task { notificationsAllowed = await store.notificationsAuthorized() }
        .onAppear { login.refresh() }
    }

    static func openNotificationSettings() {
        if let url = URL(string: "x-apple.systempreferences:com.apple.Notifications-Settings.extension") {
            NSWorkspace.shared.open(url)
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

private struct AdvancedSettings: View {
    let store: CoreStore
    @AppStorage(CoreStore.homeKey) private var home = ""
    @State private var draft = ""

    var body: some View {
        Form {
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
        .onAppear { draft = home }
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
