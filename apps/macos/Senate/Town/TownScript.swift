import Foundation

/// A made-up town that plays the same way every time: for snapshots, for
/// trying the scene without a busy Senate, and for the tests. Its names are
/// invented; nothing in it comes from the daemon.
enum TownScript {
    /// `SENATE_APP_TOWN_DEMO=1` (or `large`): the page shows this town, not
    /// the store's.
    static let env = "SENATE_APP_TOWN_DEMO"

    static var requested: TownInput? {
        switch ProcessInfo.processInfo.environment[env] {
        case "1", "small": quiet
        case "large": large
        default: nil
        }
    }

    private static func s(
        _ id: String, _ name: String, _ runtime: TownRuntime, _ mood: TownMood, chair: Bool = false
    ) -> TownSenator {
        TownSenator(id: id, name: name, runtime: runtime, mood: mood, chair: chair, sessionId: "demo-\(id)")
    }

    /// Five committees and a hall: someone at work, someone idle, a
    /// senator asleep, one waiting on a person, two threads waiting on you.
    static let quiet = TownInput(state: TownState(
        committees: [
            TownCommittee(name: "senate", senators: [
                s("claude-chair", "chair", .claude, .working, chair: true),
                s("claude-town", "town", .claude, .working),
                s("codex-flaky", "flaky", .codex, .idle),
                s("claude-app", "app", .claude, .waiting),
            ], waiting: [TownThreadRef(committee: "senate", root: 2577, title: "Unknown committee in ask", asker: "app")]),
            TownCommittee(name: "ledger", senators: [
                s("codex-ledger", "ledger-chair", .codex, .idle, chair: true),
                s("claude-migrate", "migrate", .claude, .working),
                s("agy-schema", "schema", .agy, .paused),
            ]),
            TownCommittee(name: "web", senators: [
                s("claude-web", "web-chair", .claude, .idle, chair: true),
                s("agy-css", "css", .agy, .working),
            ]),
            TownCommittee(name: "ios-duo", senators: [
                s("codex-ios", "ios-chair", .codex, .working, chair: true),
                s("claude-modal", "modal", .claude, .idle),
                s("codex-sheet", "sheet", .codex, .stopped),
            ], waiting: [TownThreadRef(committee: "ios-duo", root: 412, title: "Ship the modal?")]),
            TownCommittee(name: "docs", senators: [
                s("claude-docs", "docs", .claude, .idle, chair: true),
            ]),
        ],
        hall: TownHall(secretary: s("claude-secretary", "secretary", .claude, .working), docket: 1),
        person: "wendell"
    ))

    /// What happens in the busy town: visits within a house, across the
    /// street, and to the hall.
    static let busy: [TownEvent] = [
        .message(from: "claude-chair", to: "claude-app", thread: ref("senate", 2585, "Town page"),
                 line: "Can you take the Town page next? SpriteKit, original art"),
        .message(from: "codex-ledger", to: "claude-web", thread: ref("ledger", 88, "Shared schema"),
                 line: "The schema moved to v3, please rebase onto it"),
        .message(from: "agy-css", to: "codex-ios", thread: ref("web", 17, "Tokens"),
                 line: "Are the colour tokens final for iOS too?"),
        .message(from: "claude-docs", to: "claude-secretary", thread: ref("docs", 9, "Release notes"),
                 line: "Release notes are ready for the Speaker"),
        .mood(senator: "codex-flaky", .working),
        .answered(thread: ref("ledger", 77, "Migration order"), a: "claude-migrate", b: "codex-ledger"),
        .arrived(senator: "claude-new"),
        .left(senator: "claude-modal"),
    ]

    private static func ref(_ committee: String, _ root: UInt64, _ title: String) -> TownThreadRef {
        TownThreadRef(committee: committee, root: root, title: title)
    }

    /// The quiet town after the busy events' mood changes.
    static var busyState: TownState {
        var state = quiet.state
        for (i, committee) in state.committees.enumerated() {
            for (j, senator) in committee.senators.enumerated() {
                if senator.id == "codex-flaky" { state.committees[i].senators[j].mood = .working }
                if senator.id == "claude-modal" { state.committees[i].senators[j].mood = .stopped }
            }
            if committee.name == "web" {
                state.committees[i].senators.append(s("claude-new", "new", .claude, .idle))
            }
        }
        return state
    }

    /// Thirty committees and sixty senators: the stress test.
    static let large: TownInput = {
        let moods: [TownMood] = [.working, .idle, .idle, .working, .paused, .waiting, .idle, .stopped]
        let runtimes: [TownRuntime] = [.claude, .codex, .agy, .claude]
        var committees: [TownCommittee] = []
        var n = 0
        for c in 0..<30 {
            var senators: [TownSenator] = []
            for k in 0..<2 {
                senators.append(s("s\(n)", "senator-\(n)", runtimes[n % 4], moods[(n * 3 + c) % moods.count], chair: k == 0))
                n += 1
            }
            committees.append(TownCommittee(
                name: "committee-\(c)", senators: senators,
                waiting: c % 7 == 0 ? [TownThreadRef(committee: "committee-\(c)", root: UInt64(c + 1), title: "Question")] : []
            ))
        }
        return TownInput(state: TownState(committees: committees, hall: TownHall()))
    }()

    /// Visits for the large town: a dozen at once, more than are let out.
    static var largeEvents: [TownEvent] {
        (0..<14).map { i in
            .message(from: "s\(i * 4)", to: "s\(i * 4 + 9)", thread: ref("committee-\(i * 2)", UInt64(100 + i), "Hello"),
                     line: "Hello from senator \(i * 4)")
        }
    }
}
