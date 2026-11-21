import Foundation
import Testing
@testable import Senate
import SenateCore

/// The store against a fake core: what it shows, what it asks the core to
/// do, and what it keeps of its own.
@MainActor
@Suite struct CoreStoreTests {
    let core = FakeCore()
    let defaults: UserDefaults

    init() {
        let suite = "senate-tests-\(UUID().uuidString)"
        defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
    }

    func store() -> CoreStore {
        let core = self.core
        return CoreStore(makeCore: { core }, defaults: defaults, notifications: false)
    }

    /// Wait (briefly) for the store to have heard what the core pushed.
    func until(_ store: CoreStore, _ ok: @escaping (CoreStore) -> Bool) async throws {
        for _ in 0..<200 where !ok(store) {
            try await Task.sleep(for: .milliseconds(5))
        }
        #expect(ok(store))
    }

    @Test func statesArriveInTheOrderTheCoreSentThem() async throws {
        let store = store()
        #expect(core.calls == ["start"])
        for n in 1...20 {
            core.push(online((0..<n).map { thread(UInt64($0), "t\($0)") }))
        }
        try await until(store) { $0.state.inbox.count == 20 }
        #expect(store.state.online)
    }

    @Test func theInboxNarrowsToAnAgentButKeepsWhatWasPutToYou() async throws {
        let store = store()
        core.push(online([
            thread(1, "for claude", for: ["claude"]),
            thread(2, "for codex", for: ["codex"]),
            thread(3, "put to alice", for: ["codex"], escalated: true),
        ], agents: [agent("claude"), agent("codex")]))
        try await until(store) { $0.state.inbox.count == 3 }
        #expect(store.inboxAgents.map(\.path) == ["claude", "codex"])
        store.agentFilter = "claude"
        #expect(store.inbox.map(\.root) == [1, 3])
        store.agentFilter = nil
        store.search = "codex"
        #expect(store.inbox.map(\.root) == [2])
        store.search = "#3"
        #expect(store.inbox.isEmpty)
        store.search = "3"
        #expect(store.inbox.map(\.root) == [3])
    }

    @Test func aFilterForAnAgentThatLeftIsDropped() async throws {
        let store = store()
        core.push(online([thread(1, "x")], agents: [agent("claude"), agent("codex")]))
        try await until(store) { $0.state.inbox.count == 1 }
        store.agentFilter = "codex"
        core.push(online([thread(1, "x")], agents: [agent("claude")]))
        try await until(store) { $0.agentFilter == nil }
    }

    @Test func anEmptyReplyNeverReachesTheCore() async throws {
        let store = store()
        let t = thread(7, "which env?")
        core.push(online([t]))
        try await until(store) { !$0.state.inbox.isEmpty }
        store.drafts[t.key] = "  \n"
        await store.sendReply(t)
        #expect(store.actionError != nil)
        #expect(!core.calls.contains { $0.hasPrefix("reply") })
    }

    @Test func aReplyIsSentAsWrittenAndItsDraftCleared() async throws {
        let store = store()
        let t = thread(7, "which env?")
        core.push(online([t]))
        try await until(store) { !$0.state.inbox.isEmpty }
        store.drafts[t.key] = "staging"
        await store.sendReply(t)
        #expect(core.calls.contains("reply refund-v2 7 staging"))
        #expect(store.drafts[t.key] == nil)
        #expect(store.actionError == nil)
    }

    @Test func aRowIsBusyWhileItsActionIsInFlight() async throws {
        let store = store()
        let t = thread(7, "which env?")
        core.push(online([t]))
        try await until(store) { !$0.state.inbox.isEmpty }
        core.holdActions = true
        store.drafts[t.key] = "staging"
        let sending = Task { await store.sendReply(t) }
        try await until(store) { $0.isBusy(t.key) }
        core.release()
        await sending.value
        #expect(!store.isBusy(t.key))
    }

    @Test func theCoresRefusalIsShownAndTheDraftKept() async throws {
        let store = store()
        let t = thread(7, "which env?")
        core.push(online([t]))
        try await until(store) { !$0.state.inbox.isEmpty }
        store.drafts[t.key] = "staging"
        core.failNext = .Failed(message: "thread 7 is archived")
        await store.sendReply(t)
        #expect(store.actionError == "thread 7 is archived")
        #expect(store.drafts[t.key] == "staging")
    }

    @Test func draftsOutliveTheStore() async throws {
        let first = store()
        first.drafts["refund-v2:7"] = "half a thought"
        let second = store()
        #expect(second.drafts["refund-v2:7"] == "half a thought")
    }

    @Test func aCoreThatCannotStartCanBeTriedAgain() async throws {
        var attempts = 0
        let core = self.core
        let store = CoreStore(
            makeCore: {
                attempts += 1
                if attempts == 1 { throw CoreError.Setup(message: "no home") }
                return core
            },
            defaults: defaults,
            notifications: false
        )
        #expect(store.setupError == "no home")
        store.connect()
        #expect(store.setupError == nil)
        #expect(core.calls == ["start"])
    }

    @Test func theDocketSectionIsThereOnlyWhileAnAgentPresides() async throws {
        let store = store()
        core.push(online())
        try await until(store) { $0.state.online }
        #expect(!store.sections.contains(.docket))
        core.push(online(docket: true))
        try await until(store) { $0.sections.contains(.docket) }
    }

    @Test func openingAThreadFromANotificationSelectsIt() async throws {
        let store = store()
        store.section = .senates
        await store.perform(.openThread(committee: "refund-v2", root: 7))
        #expect(store.section == .inbox)
        #expect(store.selectedThread == "refund-v2:7")
    }

    @Test func notificationActionsReachTheCore() async throws {
        let store = store()
        await store.perform(.reply(committee: "refund-v2", root: 7, body: "yes"))
        await store.perform(.take(committee: "refund-v2", root: 8))
        await store.perform(.approve(id: "019a:3", allow: false))
        #expect(core.calls.contains("reply refund-v2 7 yes"))
        #expect(core.calls.contains("take refund-v2 8"))
        #expect(core.calls.contains("approve 019a:3 deny"))
    }

    @Test func readingTheOpenThreadKeepsOnlyTheSelectedOne() async throws {
        let store = store()
        let a = thread(1, "a"), b = thread(2, "b")
        core.push(online([a, b]))
        try await until(store) { $0.state.inbox.count == 2 }
        store.selectedThread = b.key
        await store.loadThread(a)
        #expect(store.threadView(for: a) == nil, "a read for a thread no longer selected is dropped")
        await store.loadThread(b)
        #expect(store.threadView(for: b)?.root == 2)
    }
}

@Suite struct NotificationTests {
    @Test func aReplyWithTextRepliesAndWithoutOpensTheThread() {
        let info: [AnyHashable: Any] = ["committee": "refund-v2", "root": NSNumber(value: 7)]
        #expect(Notifier.action(category: "THREAD", action: "REPLY", info: info, text: "ok")
            == .reply(committee: "refund-v2", root: 7, body: "ok"))
        #expect(Notifier.action(category: "THREAD", action: "REPLY", info: info, text: "  ")
            == .openThread(committee: "refund-v2", root: 7))
        #expect(Notifier.action(category: "THREAD", action: "TAKE", info: info, text: nil)
            == .take(committee: "refund-v2", root: 7))
        #expect(Notifier.action(category: "THREAD", action: "com.apple.UNNotificationDefaultActionIdentifier", info: info, text: nil)
            == .openThread(committee: "refund-v2", root: 7))
        #expect(Notifier.action(category: "THREAD", action: "com.apple.UNNotificationDismissActionIdentifier", info: info, text: nil)
            == nil)
    }

    @Test func approvalsAndPromptsMapToTheirActions() {
        let approval: [AnyHashable: Any] = ["approval": "019a:3"]
        #expect(Notifier.action(category: "CODEX_APPROVAL", action: "ALLOW", info: approval, text: nil)
            == .approve(id: "019a:3", allow: true))
        #expect(Notifier.action(category: "CODEX_APPROVAL", action: "DENY", info: approval, text: nil)
            == .approve(id: "019a:3", allow: false))
        #expect(Notifier.action(category: "CODEX_APPROVAL", action: "x", info: approval, text: nil)
            == .openApprovals)
        let prompt: [AnyHashable: Any] = ["session": "refund-v2::lead"]
        #expect(Notifier.action(category: "HOSTED_PROMPT", action: "ATTACH", info: prompt, text: nil)
            == .attach(label: "refund-v2::lead"))
        #expect(Notifier.action(category: "HOSTED_PROMPT", action: "x", info: prompt, text: nil)
            == .openSessions)
        #expect(Notifier.action(category: "OTHER", action: "x", info: [:], text: nil) == nil)
    }
}

@Suite struct FormattingTests {
    @Test func agesAreShort() {
        let now = Date.now
        #expect(Ago.text(now.addingTimeInterval(-42), now: now) == "42s")
        #expect(Ago.text(now.addingTimeInterval(-180), now: now) == "3m")
        #expect(Ago.text(now.addingTimeInterval(-5 * 3600), now: now) == "5h")
        #expect(Ago.text(now.addingTimeInterval(-2 * 86400), now: now) == "2d")
        #expect(Ago.text(now.addingTimeInterval(60), now: now) == "0s")
    }

    @Test func everyLabelIsInBothLanguages() throws {
        // The source catalog, next to this file's directory.
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("Senate/Localizable.xcstrings")
        let catalog = try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as! [String: Any]
        let strings = catalog["strings"] as! [String: [String: Any]]
        #expect(strings.count > 100)
        for (key, entry) in strings {
            let localizations = entry["localizations"] as? [String: Any] ?? [:]
            #expect(localizations["en"] != nil, "\(key) has no English")
            #expect(localizations["zh-Hans"] != nil, "\(key) has no Chinese")
        }
    }

    @Test func countsUseRealPlurals() {
        if appLanguage().hasPrefix("zh") {
            #expect(L("senates.sessions", Int64(3)) == "3 个会话")
        } else {
            #expect(L("senates.sessions", Int64(1)) == "1 session")
            #expect(L("senates.sessions", Int64(3)) == "3 sessions")
        }
    }
}
