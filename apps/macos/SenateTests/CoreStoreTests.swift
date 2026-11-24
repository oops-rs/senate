import AppKit
import Foundation
import ImageIO
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

    @Test func deskDecisionKeepsTheActionIdOnRetry() async throws {
        let store = store()
        let item = DocketEntry(committee: "refund-v2", seq: 60, kindLabel: "Request",
                               by: "tester", body: "Preview", awaiting: "addressed", why: "",
                               agents: [], since: Date())
        store.place = .desk(item)
        let detail = try await store.deskItem(item)
        #expect(detail.item.body == "Full request")
        let id = try store.newDeskActionId()
        core.failNext = .Failed(message: "The destination is temporarily unavailable.")
        await #expect(throws: CoreError.self) {
            _ = try await store.decideDeskItem(item, itemId: detail.eventId, id: id, choice: .forward)
        }
        let receipt = try await store.decideDeskItem(item, itemId: detail.eventId, id: id, choice: .forward)
        #expect(receipt.delivered && receipt.seq == 75)
        #expect(core.calls.filter { $0 == "desk-decide refund-v2 60 desk-test-action forward" }.count == 2)
        #expect(store.place == .desk(item))
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

    @Test func senateIsInTheDockUnlessTurnedOff() async throws {
        // Nothing stored (a new install, or one upgrading): in the Dock.
        #expect(Dock.showsInDock(defaults))
        #expect(Dock.policy(showInDock: true) == .regular)
        #expect(Dock.policy(showInDock: false) == .accessory)
        let store = store()
        #expect(store.showInDock)
        // Turned off, it is remembered, as a launch argument would say it.
        store.showInDock = false
        #expect(!Dock.showsInDock(defaults))
        defaults.set("NO", forKey: Dock.key)
        #expect(!Dock.showsInDock(defaults))
        defaults.set("YES", forKey: Dock.key)
        #expect(Dock.showsInDock(defaults))
    }

    @Test func theSidebarHeadsWithInboxApprovalsAndHistory() async throws {
        let store = store()
        #expect(store.sections == [.inbox, .approvals, .history])
    }

    @Test func openingAThreadFromANotificationSelectsIt() async throws {
        let store = store()
        store.place = .section(.senates)
        await store.perform(.openThread(committee: "refund-v2", root: 7))
        #expect(store.place == .thread("refund-v2:7"))
        #expect(store.selectedThread == "refund-v2:7")
    }

    @Test func thePlaceIsAPageAThreadASessionOrACommittee() async throws {
        let store = store()
        core.push(online([thread(7, "which env?")]))
        try await until(store) { $0.state.inbox.count == 1 }
        #expect(store.place == .section(.inbox))
        store.place = .thread("refund-v2:7")
        #expect(store.selectedThread == "refund-v2:7")
        #expect(store.selectedRow?.title == "which env?")
        store.place = .committee("payments")
        #expect(store.selectedThread == nil)
        store.place = .section(.inbox)
        #expect(store.place == .section(.inbox))
    }

    @Test func aThreadNotInTheInboxOpensFromWhatIsRead() async throws {
        let store = store()
        core.threadView = ThreadView(
            committee: "refund-v2", root: 140, title: "Reviewed; fine by me.", state: nil, stateLabel: nil,
            kindLabel: "note", events: [])
        core.push(online())
        try await until(store) { $0.state.online }
        store.openThread(committee: "refund-v2", root: 140)
        try await until(store) { $0.selectedRow != nil }
        #expect(core.calls.contains("thread refund-v2 140"))
        #expect(store.selectedRow?.title == "Reviewed; fine by me.")
        #expect(store.selectedRow?.forPerson == false)
    }

    // MARK: The menu bar

    @Test func theMenuListsWhatWaitsWithPermissionsFirst() async throws {
        let store = store()
        var state = online([thread(7, "which env?"), thread(8, "ship it?")])
        state.sessions = [prompting("lead", waited: 60), prompting("tester", tool: "rm -rf target", waited: 300)]
        state.approvals = [approval("a1", "run: make release", command: "make release")]
        core.push(state)
        try await until(store) { $0.state.inbox.count == 2 }
        #expect(store.waitingCount == 5)
        let menu = store.waitingMenu()
        #expect(menu.total == 5)
        #expect(!menu.truncated)
        // What holds an agent first, the longest wait first; then the threads.
        #expect(menu.rows.map(\.item.id) == [
            "prompt:refund-v2::tester", "prompt:refund-v2::lead", "approval:a1",
            "thread:refund-v2:7", "thread:refund-v2:8",
        ])
        #expect(menu.rows[0].subtitle == "rm -rf target · refund-v2 · 5m")
        #expect(menu.rows[2].title == "run: make release")
        #expect(menu.rows[2].subtitle.hasPrefix("make release · "))
        #expect(menu.rows[3].title == "which env?")
        #expect(menu.rows[3].subtitle.hasPrefix("refund-v2 · "))
        if !appLanguage().hasPrefix("zh") {
            #expect(menu.header == "5 waiting on you")
        }
    }

    @Test func requestsEscalationsAndDocketItemsWaitOnYouByCommitteeButAgentsWorkIsApart() async throws {
        let store = store()
        var state = online([
            thread(1, "agents at work", committee: "infra", forPerson: false),
            thread(2, "approve the window?", committee: "payments"),
            thread(3, "publish the docs?", committee: "docs", escalated: true),
        ])
        state.docket = docket([docketEntry(40, "who signs off?\nabove 10k", committee: "payments")])
        core.push(state)
        try await until(store) { $0.state.inbox.count == 3 }
        #expect(store.agentThreads.map(\.root) == [1])
        // The agents' own work is not counted and not in the menu.
        #expect(store.waitingCount == 3)
        #expect(Set(store.waitingMenu().rows.map(\.item.id)) == [
            "thread:payments:2", "docket:payments:40", "thread:docs:3",
        ])
        // (Each committee oldest first; these were made in the same instant.)
        #expect(Set(store.waitingByCommittee.map(\.committee)) == ["payments", "docs"])
        let payments = try #require(store.waitingByCommittee.first { $0.committee == "payments" })
        let item = try #require(payments.items.first { $0.id == "docket:payments:40" })
        #expect(item.title == "who signs off?")
        #expect(item.asker == "[senator::auditor]")
        // An item for the Speaker opens the approvals; from the menu too.
        store.place = .item(item.id)
        #expect(store.place == .section(.approvals))
        // Escalated to alice: the Speaker's desk, not the inbox.
        #expect(store.deskThreads.map(\.root) == [3])
        #expect(store.inboxThreads.map(\.root) == [2])
        #expect(store.approvalsCount == 2)
    }

    @Test func inactiveAgentsAreHiddenUntilAskedFor() async throws {
        let store = store()
        core.push(online([thread(1, "x")], agents: [
            agent("claude"), agent("claude-0a1b2c3d", active: false),
            agent("codex-01a1e8ff", pending: 1, active: false),
        ]))
        try await until(store) { $0.state.inbox.count == 1 }
        // An inactive agent with something waiting stays in the filter.
        #expect(store.inboxAgents.map(\.path) == ["claude", "codex-01a1e8ff"])
        store.showInactive = true
        #expect(store.inboxAgents.count == 3)
        // The choice is remembered.
        #expect(defaults.bool(forKey: "showInactive"))
    }

    @Test func committeesComeFromTheRosterAndHideWhenNothingIsActive() async throws {
        let store = store()
        var state = online([thread(2, "approve?", committee: "payments")])
        state.senates = [senate(roster: [
            committee("docs", chair: "scribe", members: [("scribe", false)]),
            committee("payments", chair: "ledger", members: [("ledger", false)]),
            committee("infra", members: [("oncall", true)]),
        ])]
        core.push(state)
        try await until(store) { $0.state.inbox.count == 1 }
        // docs: no one active, nothing waiting; payments: something waits.
        #expect(store.roster.filter(store.isActive).map(\.committee) == ["payments", "infra"])
    }

    // MARK: Sessions

    @Test func theSidebarListsActiveSessionsByCommitteeAndMovesBetweenThem() async throws {
        let store = store()
        var state = online()
        state.groups = groups([
            sessionLine("lead", status: .needsYou),
            sessionLine("codex", status: .working),
            sessionLine("reviewer", status: .stopped, active: false),
            sessionLine("ledger", committee: "payments"),
            sessionLine("scratch", committee: nil),
        ])
        core.push(state)
        try await until(store) { !$0.state.groups.isEmpty }
        #expect(store.shownGroups.map(\.title) == ["payments", "refund-v2", "No committee"])
        #expect(store.shownSessions.map(\.displayName) == ["ledger", "lead", "codex", "scratch"])
        store.showInactive = true
        #expect(store.shownSessions.count == 5)
        store.showInactive = false
        // The search narrows sessions too.
        store.search = "led"
        #expect(store.shownSessions.map(\.displayName) == ["ledger"])
        store.search = ""
        // ⌘⌥↓ from elsewhere: the first; then the next; ⌘⌥↑ back.
        store.moveSession(1)
        #expect(store.place == .session("s-ledger"))
        store.moveSession(1)
        #expect(store.place == .session("s-lead"))
        store.moveSession(-1)
        #expect(store.place == .session("s-ledger"))
        // Folded committees are remembered.
        store.folded.insert("payments")
        #expect(defaults.stringArray(forKey: "foldedCommittees") == ["payments"])
    }

    @Test func aSessionPageReadsItsTailFollowsItAndLetsGo() async throws {
        let store = store()
        var state = online()
        state.groups = groups([sessionLine("lead"), sessionLine("codex")])
        core.timelines["s-lead"] = TimelinePage(
            sessionId: "s-lead", available: true, unavailable: nil,
            items: [entry(1, "plan", user: true), entry(2, "done")], hasOlder: true,
            firstSeq: 1, lastSeq: 2, working: true)
        core.push(state)
        try await until(store) { !$0.state.groups.isEmpty }
        store.place = .session("s-lead")
        try await until(store) { $0.timeline?.loading == false }
        #expect(store.timeline?.items.map(\.seq) == [1, 2])
        #expect(core.calls.contains("timeline s-lead tail"))
        #expect(core.calls.contains("watch s-lead 2"))
        // A watch update replaces an item in place and appends the new.
        core.timelineListener?.onDelta(delta: TimelineDelta(
            items: [entry(2, "done, really"), entry(3, "next")], working: false, reset: false))
        try await until(store) { $0.timeline?.items.count == 3 }
        if case let .assistant(text) = store.timeline?.items[1].body { #expect(text == "done, really") }
        #expect(store.timeline?.working == false)
        // Older pages go before.
        core.timelines["s-lead"] = TimelinePage(
            sessionId: "s-lead", available: true, unavailable: nil, items: [entry(0, "older")],
            hasOlder: false, firstSeq: 0, lastSeq: 0, working: false)
        await store.loadOlder()
        #expect(store.timeline?.items.map(\.seq) == [0, 1, 2, 3])
        #expect(store.timeline?.hasOlder == false)
        // Leaving the session lets go of its watch.
        store.place = .session("s-codex")
        #expect(core.calls.contains("unwatch s-lead"))
        #expect(store.timeline?.sessionId == "s-codex")
    }

    @Test func theComposerSaysANoteToTheSessionAndClearsItsDraft() async throws {
        let store = store()
        let lead = sessionLine("lead")
        core.push(online())
        try await until(store) { $0.state.online }
        await store.say(to: lead)
        #expect(store.actionError != nil, "nothing to say")
        store.sessionDrafts[lead.sessionId] = "Ship it."
        await store.say(to: lead)
        #expect(core.calls.contains("say s-lead Ship it."))
        #expect(store.sessionDrafts[lead.sessionId] == nil)
    }

    @Test func anOlderDaemonsChairsStandInForTheRoster() async throws {
        let store = store()
        var state = online()
        state.senates = [senate(roster: [], chairs: [
            ChairLine(committee: "refund-v2", chair: "[senator::claude]"),
            ChairLine(committee: "infra", chair: nil),
        ])]
        core.push(state)
        try await until(store) { !$0.state.senates.isEmpty }
        #expect(store.roster.map(\.committee) == ["refund-v2", "infra"])
        #expect(store.roster[0].chairId == "claude")
        #expect(store.roster[1].chairId == nil)
    }

    @Test func theMenuShowsTheFirstEightAndSaysHowManyThereAre() async throws {
        let store = store()
        core.push(online((1...12).map { thread(UInt64($0), "t\($0)") }))
        try await until(store) { $0.state.inbox.count == 12 }
        let menu = store.waitingMenu()
        #expect(menu.rows.count == WaitingMenu.limit)
        #expect(menu.total == 12)
        #expect(menu.truncated)
    }

    @Test func theMenuIsOneLineWhenNothingWaitsOrTheDaemonIsDown() async throws {
        let store = store()
        // Before the daemon answers: offline, and nothing listed.
        #expect(store.waitingMenu().rows.isEmpty)
        #expect(store.waitingMenu().header == L("menu.offline_short"))
        core.push(online())
        try await until(store) { $0.state.online }
        #expect(store.waitingMenu().rows.isEmpty)
        #expect(store.waitingMenu().header == L("menu.clear"))
        #expect(store.waitingCount == 0)
    }

    @Test func aLongTitleOrCommandIsCutToOneShortLine() async throws {
        let store = store()
        var state = online([thread(7, String(repeating: "long ", count: 40))])
        state.approvals = [approval("a1", "run", command: "echo one\necho two")]
        core.push(state)
        try await until(store) { $0.state.inbox.count == 1 }
        let menu = store.waitingMenu()
        #expect(menu.rows[0].subtitle.hasPrefix("echo one · "))
        #expect(menu.rows[1].title.count == 64)
        #expect(menu.rows[1].title.hasSuffix("…"))
    }

    @Test func openingAMenuItemGoesToWhatItNames() async throws {
        let store = store()
        var state = online([thread(7, "which env?")])
        state.approvals = [approval("a1", "run: make release")]
        core.push(state)
        try await until(store) { $0.state.inbox.count == 1 }
        let menu = store.waitingMenu()
        store.open(menu.rows[1].item)
        #expect(store.place == .thread("refund-v2:7"))
        store.open(menu.rows[0].item)
        #expect(store.place == .section(.approvals))
        // "Show All" is home itself, whatever thread was open.
        store.showHome()
        #expect(store.place == .section(.inbox))
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

    @Test func aPromptBannerAttachesToItsOwnSessionNotItsAliasesNextHolder() async throws {
        let store = store()
        // A prompt waits in `reviewer`; its banner carries the session's id.
        let original = prompting("reviewer")
        var state = online()
        state.sessions = [original]
        core.push(state)
        try await until(store) { $0.state.sessions.count == 1 }
        let tap = Notifier.action(
            category: "HOSTED_PROMPT", action: "ATTACH",
            info: ["session": original.sessionId], text: nil)
        #expect(tap == .attach(sessionId: "s-reviewer"))

        // While it waits, the tap attaches to it.
        await store.perform(tap!)
        #expect(core.calls.filter { $0.hasPrefix("attach") } == ["attach-session \(original.sessionId)"])

        // `senate session replace`: the alias passes to a new session,
        // with a prompt of its own waiting.
        var replacement = prompting("reviewer")
        replacement.sessionId = "s-reviewer-2"
        state.sessions = [replacement]
        core.push(state)
        try await until(store) { $0.state.sessions.first?.sessionId == "s-reviewer-2" }

        // The old banner does not open the replacement: Approvals, and why.
        await store.perform(tap!)
        #expect(core.calls.filter { $0.hasPrefix("attach") } == ["attach-session \(original.sessionId)"])
        #expect(store.place == .section(.approvals))
        #expect(store.actionError == L("error.prompt_gone"))
    }

    @Test func aPromptThatWasAnsweredNoLongerAttaches() async throws {
        let store = store()
        var session = prompting("reviewer")
        var state = online()
        session.promptTool = nil
        session.promptSince = nil
        state.sessions = [session]
        core.push(state)
        try await until(store) { $0.state.sessions.count == 1 }
        await store.perform(.attach(sessionId: session.sessionId))
        #expect(!core.calls.contains { $0.hasPrefix("attach") })
        #expect(store.place == .section(.approvals))
    }

    @Test func answeringAHostedPromptAsksTheDaemonForThatSession() async throws {
        let store = store()
        let lead = HostedSession(
            displayName: "lead", active: true, label: "refund-v2::lead", sessionId: "s-1", mention: "senator::claude",
            committee: "refund-v2", runtime: "claude", model: nil, dir: "/work",
            running: true, startedAt: nil, attached: false, typedSecsAgo: nil,
            promptTool: "cargo publish", promptSince: .now, promptAnswerable: true, paused: false)
        await store.decide(lead, .allow)
        await store.decide(lead, .allowForSession)
        await store.decide(lead, .deny)
        #expect(core.calls.contains("approve-session refund-v2::lead allow"))
        #expect(core.calls.contains("approve-session refund-v2::lead allowForSession"))
        #expect(core.calls.contains("approve-session refund-v2::lead deny"))
        // The daemon's refusal (nothing waits, or not from here) is shown.
        core.failNext = .Failed(message: "senator::claude has no permission prompt waiting.")
        await store.decide(lead, .allow)
        #expect(store.actionError?.contains("no permission prompt waiting") == true)
        #expect(!store.isBusy("session:refund-v2::lead"))
    }

    @Test func readingTheOpenThreadKeepsOnlyTheSelectedOne() async throws {
        let store = store()
        let a = thread(1, "a"), b = thread(2, "b")
        core.push(online([a, b]))
        try await until(store) { $0.state.inbox.count == 2 }
        store.place = .thread(b.key)
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
        let prompt: [AnyHashable: Any] = ["session": "s-lead"]
        #expect(Notifier.action(category: "HOSTED_PROMPT", action: "ATTACH", info: prompt, text: nil)
            == .attach(sessionId: "s-lead"))
        #expect(Notifier.action(category: "HOSTED_PROMPT", action: "x", info: prompt, text: nil)
            == .openSession(sessionId: "s-lead"))
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

@Suite struct ConversationRowsTests {
    private func think(_ seq: UInt64, _ text: String) -> TimelineEntry {
        TimelineEntry(seq: seq, at: nil, truncated: false, body: .reasoning(text: text))
    }

    private func tool(_ seq: UInt64, _ title: String) -> TimelineEntry {
        TimelineEntry(seq: seq, at: nil, truncated: false, body: .tool(call: ToolCall(
            callId: "c\(seq)", name: "Bash", kind: "shell", status: "completed", statusLabel: "done",
            title: title, cwd: nil, output: nil, exitCode: nil, diff: nil, mcp: nil)))
    }

    @Test func reasoningGoesIntoTheCallOrReplyItLedTo() {
        let rows = ConversationRows.rows([
            entry(1, "go", user: true),
            think(2, "read the policy first"),
            tool(3, "cat policy.rs"),
            think(4, "now test"),
            think(5, "  "),                       // empty: dropped
            tool(6, "cargo test"),
            think(7, "it passes; say so"),
            entry(8, "Done."),
        ])
        #expect(rows.map(\.id) == [1, 3, 8], "no row of reasoning's own; the two calls one group")
        guard case let .tools(members) = rows[1].kind else { Issue.record("a group"); return }
        #expect(members.map(\.id) == [3, 6])
        #expect(rows[1].thinking == ["read the policy first"], "what led into the run rides on the group")
        #expect(members[0].thinking.isEmpty)
        #expect(members[1].thinking == ["now test"])
        #expect(rows[2].thinking == ["it passes; say so"])
    }

    @Test func aTodoOrSenateMessageBetweenKeepsItAPersonsMessageEndsIt() {
        let todo = TimelineEntry(seq: 3, at: nil, truncated: false, body: .todo(items: []))
        let rows = ConversationRows.rows([
            think(1, "plan"),
            entry(2, "ok?", user: true),       // ends it: "plan" is not shown
            think(4, "first step"),
            todo,                              // does not end it
            tool(5, "ls"),
            think(6, "still thinking"),        // trailing: not shown yet
        ])
        #expect(rows.map(\.id) == [2, 3, 5])
        #expect(rows[0].thinking.isEmpty)
        #expect(rows[2].thinking == ["first step"])
    }

    @Test func aRunOfCallsIsOneGroupFromTwoCallsUp() {
        let todo = TimelineEntry(seq: 5, at: nil, truncated: false, body: .todo(items: []))
        let rows = ConversationRows.rows([
            entry(1, "go", user: true),
            tool(2, "ls"),                      // alone: not wrapped
            entry(3, "Listed.", user: false),
            tool(4, "cat a"),
            todo,                               // ends the run: 4 alone
            tool(6, "cat b"),
            think(7, "and c"),
            tool(8, "cat c"),
            tool(9, "cat d"),                   // 6, 8, 9: one group
            entry(10, "next", user: true),
        ])
        #expect(rows.map(\.id) == [1, 2, 3, 4, 5, 6, 10])
        if case .tool = rows[1].kind {} else { Issue.record("a single call is not wrapped") }
        if case .tool = rows[3].kind {} else { Issue.record("a to-do ends a run") }
        guard case let .tools(members) = rows[5].kind else { Issue.record("a group"); return }
        #expect(members.map(\.id) == [6, 8, 9])
        #expect(ConversationRows.rowId(for: 7, in: rows) == 8, "reasoning: the call it led to, in its group")
        #expect(ConversationRows.rowId(for: 9, in: rows) == 9)
        #expect(ConversationRows.rowId(for: 10, in: rows) == 10)
    }

    @Test func aGroupKeepsItsIdentityAsItsRunGrows() {
        let two = ConversationRows.rows([think(1, "x"), tool(2, "a"), tool(3, "b")])
        let three = ConversationRows.rows([think(1, "x"), tool(2, "a"), tool(3, "b"), tool(4, "c")])
        #expect(two.map(\.id) == [2] && three.map(\.id) == [2])
        #expect(three[0].thinking == ["x"])
    }

    @Test func aRowKeepsItsIdentityWhenItsCallCompletes() {
        let running = ConversationRows.rows([think(1, "x"), tool(2, "make")])
        let done = ConversationRows.rows([think(1, "x"), tool(2, "make")])
        #expect(running.map(\.id) == done.map(\.id))
    }
}


/// History: read a page at a time, narrowed, searched, forgotten, and a
/// match opened where it was said.
@MainActor
@Suite struct HistoryTests {
    let core = FakeCore()

    func store() -> CoreStore {
        let suite = "senate-tests-\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        let core = self.core
        return CoreStore(makeCore: { core }, defaults: defaults, notifications: false)
    }

    @Test func pagesNarrowsAndForgets() async throws {
        let store = store()
        let h = store.history
        await h.reload()
        #expect(h.rows.count == 12)
        #expect(h.next == nil)
        h.runtime = "codex"
        #expect(h.shown.count == 6 && h.shown.allSatisfy { $0.runtime == "codex" })
        h.runtime = nil
        h.committee = "c1"
        for _ in 0..<100 where h.rows.count == 12 || h.loading { try await Task.sleep(for: .milliseconds(5)) }
        #expect(h.rows.allSatisfy { $0.committee == "c1" } && h.rows.count == 4)
        let row = try #require(h.rows.first)
        #expect(await h.forget(row))
        #expect(!h.rows.contains { $0.sessionId == row.sessionId })
        #expect(core.calls.contains("forget \(row.sessionId)"))
        // By day, newest first.
        let days = HistoryView.days(h.rows)
        #expect(days.flatMap(\.rows).map(\.sessionId) == h.rows.map(\.sessionId))
    }

    @Test func searchAsksAgainWhileTheDaemonIndexes() async throws {
        let store = store()
        core.indexingFor = 1
        store.history.query = "ledger"
        for _ in 0..<400 where store.history.hits.isEmpty { try await Task.sleep(for: .milliseconds(10)) }
        #expect(store.history.hits.first?.sessionId == "past-3")
        #expect(store.history.indexing == nil)
        #expect(core.calls.filter { $0 == "search ledger" }.count == 2)
    }

    @Test func anEndedMatchOpensInHistory() async throws {
        let store = store()
        await store.history.reload()
        let hit = SearchHit(sessionId: "past-3", seq: 2, kind: .reply, snippet: "x", at: nil, live: false)
        store.open(hit, query: "x")
        #expect(store.place == .past("past-3"))
        #expect(store.reveal == Reveal(sessionId: "past-3", seq: 2, query: "x"))
    }
}

extension CoreStoreTests {
    /// "Show archived" lists the archived committees, remembers itself,
    /// and Unarchive asks the core and takes the committee off the list.
    @Test func showArchivedListsAndUnarchives() async throws {
        core.archived = ["api-test", "old-docs"]
        let store = store()
        #expect(store.archived.isEmpty, "off by default: nothing read")
        store.showArchived = true
        await store.loadArchived()
        #expect(store.archived == ["api-test", "old-docs"])
        #expect(defaults.bool(forKey: "showArchived"))

        #expect(await store.unarchive("api-test"))
        #expect(core.calls.contains("unarchive:api-test"))
        #expect(store.archived == ["old-docs"])

        store.showArchived = false
        #expect(store.archived.isEmpty)
    }

    /// A refusal (not the Speaker in person) is shown as the daemon said,
    /// and the committee stays archived.
    @Test func aRefusedUnarchiveShowsItsMessage() async throws {
        core.archived = ["api-test"]
        let store = store()
        store.showArchived = true
        await store.loadArchived()
        core.failNext = .Failed(message: "Only the Speaker in person can unarchive a committee.")
        #expect(await store.unarchive("api-test") == false)
        #expect(store.actionError?.contains("Only the Speaker in person") == true)
        #expect(store.archived == ["api-test"])
    }
}

extension CoreStoreTests {
    /// ⌘↩ sends only something to send, once, online, and never while an
    /// input method is composing.
    @Test func theComposerSendsOnlyWhenItShould() {
        #expect(MessageComposer.canSend("hello", busy: false, online: true, composing: false))
        #expect(!MessageComposer.canSend("", busy: false, online: true, composing: false))
        #expect(!MessageComposer.canSend("  \n\t ", busy: false, online: true, composing: false))
        #expect(!MessageComposer.canSend("hello", busy: true, online: true, composing: false))
        #expect(!MessageComposer.canSend("hello", busy: false, online: false, composing: false))
        #expect(!MessageComposer.canSend("你好", busy: false, online: true, composing: true))
    }
}

extension CoreStoreTests {
    /// "Copy Senator Alias" shows only for an alias on a committee; "Copy
    /// Senator ID" copies the primary name the CLI takes.
    @Test func copyItemsTellAnAliasFromAName() {
        #expect(Mentions.isAlias("senate::app"))
        #expect(!Mentions.isAlias("claude-f6d10eaa"))
        #expect(!Mentions.isAlias(nil))
        #expect(!Mentions.isAlias("::app"))
        #expect(Mentions.id("[senator::claude-f6d10eaa-48f6]") == "claude-f6d10eaa-48f6")
    }
}

extension CoreStoreTests {
    /// The window's navigation: a page opened from another is pushed, Back
    /// pops it (past what no longer exists), the sidebar starts again.
    @Test func pagesArePushedAndBackPopsThem() async throws {
        let store = store()
        store.select(.section(.inbox))
        #expect(store.path.isEmpty && !store.canGoBack)

        store.place = .committee("refund-v2")
        store.openThread(committee: "refund-v2", root: 118)
        #expect(store.path == [.committee("refund-v2"), .thread("refund-v2:118")])
        #expect(store.place == .thread("refund-v2:118"))

        #expect(store.back())
        #expect(store.place == .committee("refund-v2"))
        #expect(store.path == [.committee("refund-v2")])

        // A new page after going back replaces what was ahead.
        store.place = .section(.approvals)
        #expect(store.path == [.committee("refund-v2"), .section(.approvals)])

        // A session no longer on this machine is passed over.
        store.place = .session("gone")
        store.place = .committee("docs")
        #expect(store.back())
        #expect(store.place == .section(.approvals), "past the session that is gone")

        // The system's Back sets the path: the page shown follows.
        store.path = [.committee("refund-v2")]
        #expect(store.place == .committee("refund-v2"))

        // The sidebar: a new root, nothing to go back to.
        store.select(.section(.history))
        #expect(store.path.isEmpty && store.root == .section(.history) && store.place == .section(.history))
        #expect(!store.back())
    }
}

extension CoreStoreTests {
    /// A note sent from a session's composer shows at once, until the
    /// session's own card for it (same committee and seq) arrives; a
    /// failed send leaves no row and keeps the draft.
    @Test func aSentNoteShowsUntilTheSessionCarriesIt() async throws {
        let store = store()
        let session = sessionLine("claude")

        store.sessionDrafts["s-claude"] = "Ship it."
        await store.say(to: session)
        #expect(store.sessionDrafts["s-claude"] == nil)
        #expect(store.unconfirmed("s-claude", in: []).map(\.seq) == [300])

        // The session's conversation carries it: no longer pending.
        let card = TimelineEntry(seq: 9, at: nil, truncated: false, body: .senate(message: SenateLine(
            committee: "refund-v2", seq: 300, thread: 300, from: "[senator::alice]", kind: "note",
            kindLabel: "note", text: "Ship it.")))
        #expect(store.unconfirmed("s-claude", in: [card]).isEmpty)
        try await until(store) { $0.sentNotes["s-claude"] == nil }

        // Not delivered: still shown, and after a while as queued.
        store.sessionDrafts["s-claude"] = "Later."
        core.nextSent = SentNote(committee: "refund-v2", seq: 301)
        await store.say(to: session)
        let left = store.unconfirmed("s-claude", in: [card])
        #expect(left.map(\.seq) == [301])
        #expect(left[0].queued(now: Date().addingTimeInterval(60)))
        #expect(!left[0].queued(now: Date()))

        // Refused: no row, the draft stays.
        store.sessionDrafts["s-claude"] = "No."
        core.failNext = .Failed(message: "The session is not hosted here.")
        await store.say(to: session)
        #expect(store.sessionDrafts["s-claude"] == "No.")
        #expect(store.unconfirmed("s-claude", in: [card]).map(\.seq) == [301])
    }
}

extension CoreStoreTests {
    /// The note names each staged file on a line of its own, quoted the
    /// shell's way when it has spaces; image paths are read back from it.
    @Test func aNoteCarriesItsFilesPaths() {
        let body = Attachment.noteBody("Look at these.", paths: ["/h/drops/a.png", "/h/drops/my shot.png", "/h/drops/it's.txt"])
        #expect(body == "Look at these.\n\n/h/drops/a.png\n'/h/drops/my shot.png'\n'/h/drops/it'\\''s.txt'")
        #expect(Attachment.noteBody("", paths: ["/h/drops/a.png"]) == "/h/drops/a.png")
        #expect(Attachment.imagePaths(in: body) == ["/h/drops/a.png", "/h/drops/my shot.png"])
        #expect(MessageComposer.canSend("", busy: false, online: true, composing: false, attachments: 1))
    }

    /// Sending with attachments stages each, in order, then says the note
    /// with their paths; a refusal keeps the text and the attachments.
    @Test func attachmentsAreStagedThenNamedInTheNote() async throws {
        let store = store()
        let session = sessionLine("claude")
        let a = Attachment(name: "shot.png", data: Data([1, 2, 3]), thumbnail: nil)
        let b = Attachment(name: "log.txt", data: Data([4]), thumbnail: nil)

        // Refused while staging: nothing said, everything kept.
        store.sessionDrafts["s-claude"] = "See attached."
        store.sessionAttachments["s-claude"] = [a, b]
        core.failNext = .Failed(message: "attachments are limited to 20 MiB")
        await store.say(to: session)
        #expect(store.actionError?.contains("20 MiB") == true)
        #expect(store.sessionDrafts["s-claude"] == "See attached.")
        #expect(store.sessionAttachments["s-claude"]?.count == 2)
        #expect(!core.calls.contains { $0.hasPrefix("say ") })

        await store.say(to: session)
        let calls = core.calls.filter { $0.hasPrefix("drop ") || $0.hasPrefix("say ") }
        #expect(calls.suffix(3) == [
            "drop s-claude shot.png 3", "drop s-claude log.txt 1",
            "say s-claude See attached.\n\n/tmp/senate-fake/drops/fixture/shot.png\n/tmp/senate-fake/drops/fixture/log.txt",
        ])
        #expect(store.sessionDrafts["s-claude"] == nil && store.sessionAttachments["s-claude"] == nil)
    }
}

extension CoreStoreTests {
    /// A tiny PNG of the given size.
    private func png(_ w: Int, _ h: Int) -> Data {
        let image = NSImage(size: NSSize(width: w, height: h))
        image.lockFocus()
        NSColor.systemOrange.setFill()
        NSRect(x: 0, y: 0, width: w, height: h).fill()
        image.unlockFocus()
        let rep = NSBitmapImageRep(data: image.tiffRepresentation!)!
        return rep.representation(using: .png, properties: [:])!
    }

    /// Thumbnails read only a session's drop, in the daemon's exact layout
    /// `<home>/hosts/<host>/drops/<drop>/<file>`; one level off is refused.
    @Test func thumbnailsReadOnlyASessionsDrops() throws {
        let home = FileManager.default.temporaryDirectory.appendingPathComponent("senate-thumbs-\(UUID().uuidString)")
        let drop = home.appendingPathComponent("hosts/01HOST/drops/01DROP")
        try FileManager.default.createDirectory(at: drop, withIntermediateDirectories: true)
        let file = drop.appendingPathComponent("Screenshot 2026-11-24.png")
        try png(800, 500).write(to: file)
        #expect(Thumbnails.drop(file.path, home: home.path) != nil)
        #expect(Thumbnails.url(for: file.path, home: home.path)?.hasPrefix("senate-app://thumb/") == true)

        // One level shallower (drops/<file>) and one deeper: refused.
        let shallow = home.appendingPathComponent("hosts/01HOST/drops/flat.png")
        try png(10, 10).write(to: shallow)
        #expect(Thumbnails.drop(shallow.path, home: home.path) == nil)
        let deep = drop.appendingPathComponent("more")
        try FileManager.default.createDirectory(at: deep, withIntermediateDirectories: true)
        try png(10, 10).write(to: deep.appendingPathComponent("x.png"))
        #expect(Thumbnails.drop(deep.appendingPathComponent("x.png").path, home: home.path) == nil)
        // Outside the Senate home, or a symlink out of it: refused.
        let outside = home.appendingPathComponent("elsewhere.png")
        try png(10, 10).write(to: outside)
        let link = drop.appendingPathComponent("link.png")
        try FileManager.default.createSymbolicLink(at: link, withDestinationURL: outside)
        #expect(Thumbnails.drop(link.path, home: home.path) == nil)
        #expect(Thumbnails.drop(outside.path, home: home.path) == nil)
        // A directory where the file would be: refused.
        let dir = drop.appendingPathComponent("folder.png")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        #expect(Thumbnails.drop(dir.path, home: home.path) == nil)
    }

    /// Only small pictures are kept, and only so many.
    @Test func thumbnailsAreSmallAndBounded() throws {
        let big = png(3000, 1000)
        let source = try #require(CGImageSourceCreateWithData(big as CFData, nil))
        let small = try #require(Thumbnails.small(source))
        let props = CGImageSourceCopyPropertiesAtIndex(CGImageSourceCreateWithData(small as CFData, nil)!, 0, nil)
            as? [CFString: Any]
        #expect((props?[kCGImagePropertyPixelWidth] as? Int).map { $0 <= Thumbnails.maxPixels } == true)
        let tiny = png(4, 4)
        for i in 0..<(Thumbnails.capacity + 20) {
            Thumbnails.remember(path: "/bounded/\(i).png", data: tiny)
        }
        #expect(Thumbnails.count <= Thumbnails.capacity)
        #expect(Thumbnails.url(for: "/bounded/0.png", home: nil) == nil, "the oldest went")
        #expect(Thumbnails.url(for: "/bounded/\(Thumbnails.capacity + 19).png", home: nil) != nil)
    }
}

extension CoreStoreTests {
    private func committeeThread(_ committee: String, _ root: UInt64, last: UInt64, state: String = "processing",
                        other: String? = nil, waits: String? = nil) -> CommitteeThread {
        CommitteeThread(committee: committee, root: root, title: "Thread \(root)", kind: "question",
                        opener: "senator::codex", addressees: ["senator::lead"], owner: nil, state: state,
                        waitsOn: waits, escalatedTo: nil, takeover: nil, lastSeq: last, lastAt: Date(),
                        lastAuthor: nil, messageCount: nil, otherCommittee: other)
    }

    /// The sidebar's threads: newest first, each committee its own (its
    /// side of a crossing), archived ones only with Show inactive, news since the
    /// thread was last opened, kept live from activity.
    @Test func committeeThreadsAreListedAndFollowed() async throws {
        let store = store()
        core.committeeThreadRows = [
            committeeThread("refund-v2", 10, last: 12),
            committeeThread("refund-v2", 20, last: 25, other: "payments"),
            committeeThread("payments", 77, last: 26, other: "refund-v2"),
            committeeThread("refund-v2", 30, last: 31, state: "archived"),
        ]
        await store.threads.start(committees: ["refund-v2", "payments"], archived: false)
        #expect(store.threads.threads("refund-v2").map(\.root) == [20, 10])
        #expect(store.threads.threads("payments").map(\.root) == [77], "each committee lists its side of a crossing")
        #expect(store.threads.threads("refund-v2").allSatisfy { !store.threads.unread($0) }, "what was there is not news")
        #expect(core.calls.contains("watch-activity"))

        // Something is said in thread 10: read again, and it is news.
        core.committeeThreadRows[0] = committeeThread("refund-v2", 10, last: 40)
        core.pushActivity(ActivityBatch(events: [ActivityEntry(
            committee: "refund-v2", seq: 40, at: Date(), kind: "answer", from: "senator::lead",
            to: ["senator::codex"], onBehalfOf: nil, thread: 10, line: "done", state: nil)], cursors: [:], reset: false))
        try await until(store) { $0.threads.threads("refund-v2").first?.root == 10 }
        let ten = try #require(store.threads.threads("refund-v2").first)
        #expect(store.threads.unread(ten))

        // Opened: seen.
        store.openThread(committee: "refund-v2", root: 10)
        #expect(!store.threads.unread(ten))

        // Show inactive: archived ones too.
        await store.threads.start(committees: ["refund-v2"], archived: true)
        #expect(store.threads.threads("refund-v2").map(\.root) == [10, 30, 20])
        store.threads.stop()
    }
}

extension CoreStoreTests {
    /// An item awaiting an answer is answered as the Speaker, or closed;
    /// what can be done comes from the daemon, and an item nobody here
    /// can act on says whom it waits on.
    @Test func aDeskItemIsAnsweredOrClosedAsTheSpeaker() async throws {
        let store = store()
        let question = DocketEntry(committee: "commons-demo", seq: 63, kindLabel: "Question", by: "tester",
                                   body: "Which env?", awaiting: "addressed", why: "", agents: [], since: Date())
        let detail = try await store.deskItem(question)
        #expect(detail.actions == [.answer, .close])
        let answered = try await store.decideDeskItem(question, itemId: detail.eventId, id: "a1",
                                                      choice: .answer(body: "staging"))
        #expect(answered.kind == .answered && answered.seq == 77)
        let closed = try await store.decideDeskItem(question, itemId: detail.eventId, id: "c1",
                                                    choice: .close(reason: "stale"))
        #expect(closed.kind == .closed && closed.seq == 78)
        #expect(core.calls.contains("desk-decide commons-demo 63 a1 answer"))
        #expect(core.calls.contains("desk-decide commons-demo 63 c1 close"))

        let away = DocketEntry(committee: "refund-v2", seq: 62, kindLabel: "Request", by: "tester",
                               body: "x", awaiting: "away", why: "", agents: [], since: Date())
        let held = try await store.deskItem(away)
        #expect(held.actions.isEmpty && held.waitsOn != nil)
    }
}

extension CoreStoreTests {
    /// The heatmap has every day of the year, Monday first, days without
    /// activity at zero; its colour has four steps; numbers read as time.
    @Test func statsHeatmapAndNumbers() {
        let today = DateComponents(calendar: Calendar(identifier: .gregorian), year: 2026, month: 11, day: 24).date!
        let day = { (d: String, secs: UInt64) in
            StatsDay(date: d, sessions: 1, sessionsStarted: 1, activeSecs: secs, prompts: 1, agentTurns: 1,
                     toolCalls: 1, commands: 0, edits: 0, mcpCalls: 0, messages: 0, threadsOpened: 0,
                     threadsClosed: 0, threadsTaken: 0, threadsAnswered: 0, approvalsDecided: 0)
        }
        let cells = Heatmap.cells([day("2026-11-24", 3600), day("2026-11-20", 600)], through: today)
        #expect(cells.count == 365)
        #expect(cells.last?.id == "2026-11-24" && cells.last?.secs == 3600)
        #expect(cells.first(where: { $0.id == "2026-11-20" })?.secs == 600)
        #expect(cells.filter { $0.secs > 0 }.count == 2)
        // 24 November 2026 is a Tuesday: the second row.
        #expect(cells.last?.weekday == 1)
        #expect(Heatmap.level(3600, 3600) == 1 && Heatmap.level(600, 3600) == 0.3 && Heatmap.level(0, 0) == 0)
        #expect(StatsRange.day(today, minus: 6) == "2026-11-18")
        #expect(Headline.duration(2340) == "39m" && Headline.duration(7260) == "2h 1m")
    }
}
