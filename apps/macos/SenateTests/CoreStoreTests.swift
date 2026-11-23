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

    @Test func deskDecisionKeepsTheActionIdOnRetry() async throws {
        let store = store()
        let item = DocketEntry(committee: "refund-v2", seq: 60, kindLabel: "Request",
                               by: "tester", body: "Preview", awaiting: "addressed", why: "",
                               agents: [], since: Date())
        store.deskSelection = item
        let detail = try await store.deskItem(item)
        #expect(detail.item.body == "Full request")
        let id = try store.newDeskActionId()
        core.failNext = .Failed(message: "The destination is temporarily unavailable.")
        await #expect(throws: CoreError.self) {
            _ = try await store.decideDeskItem(item, itemId: detail.eventId, id: id, reason: nil)
        }
        let receipt = try await store.decideDeskItem(item, itemId: detail.eventId, id: id, reason: nil)
        #expect(receipt.delivered && receipt.seq == 75)
        #expect(core.calls.filter { $0 == "desk-decide refund-v2 60 desk-test-action forward" }.count == 2)
        #expect(store.deskSelection == item)
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

    @Test func theSidebarHeadsWithInboxAndApprovals() async throws {
        let store = store()
        #expect(store.sections == [.inbox, .approvals])
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
        #expect(core.calls.filter { $0.hasPrefix("attach") } == ["attach refund-v2::reviewer"])

        // `senate session replace`: the alias passes to a new session,
        // with a prompt of its own waiting.
        var replacement = prompting("reviewer")
        replacement.sessionId = "s-reviewer-2"
        state.sessions = [replacement]
        core.push(state)
        try await until(store) { $0.state.sessions.first?.sessionId == "s-reviewer-2" }

        // The old banner does not open the replacement: Approvals, and why.
        await store.perform(tap!)
        #expect(core.calls.filter { $0.hasPrefix("attach") } == ["attach refund-v2::reviewer"])
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
