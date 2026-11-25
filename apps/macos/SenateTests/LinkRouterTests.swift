import Foundation
import Testing
@testable import Senate
import SenateCore

/// `senate://open/v1/…` links: the shared fixtures through the core's
/// parser, every destination and every way a target is not there, bursts,
/// bounded activation, the confirmation before another Senate, and that
/// opening a link never acts.
@MainActor
@Suite struct LinkRouterTests {
    let core = FakeCore()
    let defaults: UserDefaults

    init() {
        let suite = "senate-link-tests-\(UUID().uuidString)"
        defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
    }

    /// A store online as alice: a thread waiting, two committees, a
    /// session hosted as `refund-v2::lead`, an item on the desk, a second
    /// Senate served here, and the archived committee `old-api`.
    func store() async throws -> CoreStore {
        let core = self.core
        let store = CoreStore(makeCore: { core }, defaults: defaults, notifications: false)
        var state = online([thread(7, "which env?")])
        let primary = senate(roster: [
            committee("refund-v2", chair: "claude", live: true, members: [("claude", true), ("ledger", false)]),
            committee("payments", members: [("auditor", false)]),
        ])
        var other = primary
        other.key = "team/local"
        other.senate = "team/local"
        other.title = "Senate team/local"
        other.primary = false
        state.senates = [primary, other]
        state.groups = groups([sessionLine("lead", hosted: prompting("lead"))])
        state.docket = docket([docketEntry(40, "who signs off?", committee: "payments")])
        core.archived = ["old-api"]
        core.missingThreads = [404]
        core.historyIds = [Self.obs("1"): "past-2"]
        // `lead` on refund-v2, and `auditor` (no session here) on payments.
        core.seats = ["refund-v2::\(Self.obs("2"))": "lead", "payments::\(Self.obs("3"))": "auditor"]
        core.push(state)
        for _ in 0..<200 where store.state.senates.count < 2 {
            try await Task.sleep(for: .milliseconds(5))
        }
        #expect(store.state.online)
        store.links.windowVisible = { true }
        store.links.activate = {}
        return store
    }

    /// An Observe selector, by the grammar: `obs-<12 hex>-<32 hex>`.
    static func obs(_ digit: String) -> String {
        "obs-0123456789ab-" + String(repeating: digit, count: 32)
    }

    func resolve(_ store: CoreStore, _ route: String) async -> LinkRouter.Outcome {
        guard let link = parseAppLink(url: "senate://open/v1/\(route)") else { return .rejected }
        return await store.links.resolve(link)
    }

    // MARK: The grammar

    struct Fixtures: Decodable {
        let valid: [[String]]
        let invalid: [String]
    }

    @Test func everySharedFixtureParsesThroughTheCore() throws {
        let file = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .appendingPathComponent("../../../docs/observe/v1/fixtures/links.json")
        let fixtures = try JSONDecoder().decode(Fixtures.self, from: Data(contentsOf: file))
        #expect(fixtures.valid.count >= 12 && fixtures.invalid.count >= 26)
        for pair in fixtures.valid {
            let parsed = parseAppLink(url: "senate://open/v1/\(pair[0])")
            #expect(parsed?.canonical == "senate://open/v1/\(pair[1])", "\(pair[0])")
        }
        for input in fixtures.invalid {
            #expect(parseAppLink(url: input) == nil, "\(input)")
        }
        for input in [
            "senate://intent/v1/reply/senate/42",
            "senate://intent/v1/approve/abc",
            "senate://OPEN/v1/inbox",
            "SENATE://open/v1/inbox",
            "senate:open/v1/inbox",
            "https://open/v1/inbox",
            "senate://open/v1/Inbox",
        ] {
            #expect(parseAppLink(url: input) == nil, "\(input)")
        }
    }

    @Test func rejectedLinksDoNothing() async throws {
        let store = try await store()
        let before = store.place
        #expect(!store.links.open("senate://intent/v1/reply/senate/7", provenance: .gesture))
        #expect(!store.links.open("senate://evil/v1/inbox", provenance: .system))
        await store.links.flush()
        #expect(store.place == before)
        #expect(store.links.activations == 0)
    }

    // MARK: Destinations

    @Test func everyDestinationResolvesToItsPage() async throws {
        let store = try await store()
        let entry = store.docketEntries.first!
        let cases: [(String, LinkRouter.Outcome)] = [
            ("inbox", .shown(.section(.inbox))),
            ("approvals", .shown(.section(.approvals))),
            ("desk", .shown(.section(.approvals))),
            ("history", .shown(.section(.history))),
            ("profile", .shown(.section(.senates))),
            ("committee/payments", .shown(.committee("payments"))),
            // Archived, and readable: its own page.
            ("committee/old-api", .shown(.committee("old-api"))),
            ("thread/refund-v2/7", .shown(.thread("refund-v2:7"))),
            // Not in the inbox, but readable.
            ("thread/payments/12", .shown(.thread("payments:12"))),
            ("session/s-lead", .shown(.session("s-lead"))),
            ("session/refund-v2%3A%3Alead", .shown(.session("s-lead"))),
            ("senator/lead", .shown(.session("s-lead"))),
            ("senator/refund-v2%3A%3Alead", .shown(.session("s-lead"))),
            // No session here: the seat's committee.
            ("senator/auditor", .shown(.committee("payments"))),
            ("senator/refund-v2%3A%3Aledger", .shown(.committee("refund-v2"))),
            ("desk/payments/40", .shown(.desk(entry))),
            ("history/past-3", .shown(.past("past-3"))),
            ("history/\(Self.obs("1"))", .shown(.past("past-2"))),
            // Observe's opaque seat selectors, resolved by the daemon.
            ("senator/refund-v2%3A%3A\(Self.obs("2"))", .shown(.session("s-lead"))),
            ("session/refund-v2%3A%3A\(Self.obs("2"))", .shown(.session("s-lead"))),
            ("senator/payments%3A%3A\(Self.obs("3"))", .shown(.committee("payments"))),
            ("inbox?senate=id%3Athis-mac", .shown(.section(.inbox))),
            ("committee/payments?senate=%2Falice-desk", .shown(.committee("payments"))),
        ]
        for (route, expected) in cases {
            #expect(await resolve(store, route) == expected, "\(route)")
        }
    }

    @Test func whatIsNotHereIsQuietlyUnavailable() async throws {
        let store = try await store()
        for route in [
            "committee/gone",
            "thread/gone/7",
            "thread/refund-v2/404",
            "session/s-nobody",
            "session/refund-v2%3A%3Anobody",
            "senator/nobody",
            "senator/payments%3A%3Anobody",
            "desk/payments/41",
            "history/\(Self.obs("9"))",
            // A selector the daemon does not know, one for another
            // committee, and a seat with no session. (A selector with no
            // committee is not a link at all: the grammar rejects it.)
            "senator/refund-v2%3A%3A\(Self.obs("9"))",
            "senator/payments%3A%3A\(Self.obs("2"))",
            "session/payments%3A%3A\(Self.obs("3"))",
            "history/never-was",
            // Another machine's Senate, or one not configured here.
            "inbox?senate=id%3Asomeone-else",
            "inbox?senate=%2Fnowhere",
        ] {
            #expect(await resolve(store, route) == .unavailable, "\(route)")
        }
        // Shown in the current window as a page, not an alert.
        store.links.open("senate://open/v1/committee/gone", provenance: .system)
        await store.links.flush()
        #expect(store.place == .unavailable)
    }

    @Test func aDaemonWithoutObserveKnowsNoSenateIds() async throws {
        core.installationId = nil
        let store = try await store()
        #expect(await resolve(store, "inbox?senate=id%3Athis-mac") == .unavailable)
    }

    // MARK: Another Senate

    @Test func anotherSenateIsNamedAndConfirmedBeforeItIsShown() async throws {
        let store = try await store()
        store.select(.section(.inbox))
        let link = "senate://open/v1/committee/payments?senate=%2Fteam%2Flocal"
        store.links.open(link, provenance: .gesture)
        await store.links.flush()
        let other = LinkRouter.SenateSwitch(key: "team/local", title: "Senate team/local")
        #expect(store.place == .confirmSenate(other, link))
        // No: back where the person was, nothing else shown.
        #expect(store.back())
        #expect(store.place == .section(.inbox))
        // Yes: that Senate, where the app lists it.
        store.links.open(link, provenance: .gesture)
        await store.links.flush()
        store.links.confirmed(other)
        #expect(store.place == .section(.senates))
    }

    // MARK: Bursts and activation

    @Test func aBurstShowsOnlyItsLastLinkOnce() async throws {
        let store = try await store()
        var now = ContinuousClock.now
        store.links.clock = { now }
        var activated = 0
        store.links.activate = { activated += 1 }
        for (i, route) in ["inbox", "approvals", "committee/payments"].enumerated() {
            now = now + .milliseconds(100 * i)
            store.links.open("senate://open/v1/\(route)", provenance: .gesture)
        }
        try await Task.sleep(for: .milliseconds(450))
        #expect(store.links.routed == [.shown(.committee("payments"))])
        #expect(store.place == .committee("payments"))
        #expect(activated == 1)
    }

    @Test func aBurstIsShownAtTheLatestOneSecondAfterItBegan() async throws {
        let store = try await store()
        let start = ContinuousClock.now
        var now = start
        store.links.clock = { now }
        store.links.open("senate://open/v1/inbox", provenance: .system)
        // Still arriving 1.1 s on: shown now, not 300 ms later.
        now = start + .milliseconds(1100)
        store.links.open("senate://open/v1/approvals", provenance: .system)
        for _ in 0..<40 where store.links.routed.isEmpty {
            try await Task.sleep(for: .milliseconds(5))
        }
        #expect(store.links.routed == [.shown(.section(.approvals))])
    }

    @Test func systemLinksActivateAtMostOnceEveryTwoSecondsAndNotOverAVisibleWindow() async throws {
        let store = try await store()
        var now = ContinuousClock.now
        store.links.clock = { now }
        var visible = true
        var activated = 0
        store.links.windowVisible = { visible }
        store.links.activate = { activated += 1 }
        func deliver(_ route: String, _ provenance: LinkRouter.Provenance = .system) async {
            store.links.open("senate://open/v1/\(route)", provenance: provenance)
            await store.links.flush()
        }
        // On screen already: the page changes, the app is not activated.
        await deliver("approvals")
        #expect(store.place == .section(.approvals) && activated == 0)
        visible = false
        await deliver("inbox")
        #expect(activated == 1)
        now = now + .milliseconds(1500)
        await deliver("history")
        #expect(store.place == .section(.history) && activated == 1)
        now = now + .milliseconds(600)
        await deliver("approvals")
        #expect(activated == 2)
        // A click the app saw: once per accepted gesture.
        await deliver("inbox", .gesture)
        #expect(activated == 3)
    }

    // MARK: Navigation only

    /// A thread link probes the thread without reading it: the read marker
    /// stays where it was (review 2, L1).
    @Test func aThreadLinkDoesNotMarkItRead() async throws {
        let store = try await store()
        store.links.open("senate://open/v1/thread/payments/12", provenance: .system)
        await store.links.flush()
        #expect(core.calls.contains("peek payments 12"))
        // Neither the router nor the page it shows reads it (moves the marker).
        for _ in 0..<50 where !core.calls.contains(where: { $0.hasPrefix("peek payments 12") }) { try await Task.sleep(for: .milliseconds(5)) }
        try await Task.sleep(for: .milliseconds(50))
        #expect(!core.calls.contains { $0.hasPrefix("thread ") }, "a link read a thread")
        #expect(store.place == .thread("payments:12"))
    }

    @Test func noLinkSendsApprovesAttachesStopsClosesOrEdits() async throws {
        let store = try await store()
        let routes = [
            "inbox", "approvals", "desk", "desk/payments/40", "history", "history/past-3",
            "history/\(Self.obs("1"))", "senator/refund-v2%3A%3A\(Self.obs("2"))", "profile", "committee/payments", "committee/old-api",
            "thread/refund-v2/7", "thread/payments/12", "session/s-lead",
            "session/refund-v2%3A%3Alead", "senator/lead", "senator/auditor",
            "committee/payments?senate=%2Fteam%2Flocal", "inbox?senate=id%3Athis-mac",
        ]
        for route in routes {
            store.links.open("senate://open/v1/\(route)", provenance: .gesture)
            await store.links.flush()
        }
        let acting = ["reply", "take", "approve", "attach", "stop", "close", "say", "restore",
                      "resume", "pause", "forget", "unarchive", "desk-decide", "drop", "ensure",
                      "codex", "reconnect", "read "]
        for call in core.calls {
            #expect(!acting.contains { call.hasPrefix($0) }, "a link made the core \(call)")
        }
        #expect(store.links.routed.count == routes.count)
        #expect(!store.links.routed.contains(.unavailable))
    }
}
