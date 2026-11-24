import CoreGraphics
import Foundation
import Testing
@testable import Senate

/// The Town's pure parts: the layout from names, the events from two
/// readings, the director's choreography, and the scene's clicks.
@MainActor
@Suite struct TownTests {
    private func senator(_ id: String, _ mood: TownMood = .idle, chair: Bool = false) -> TownSenator {
        TownSenator(id: id, name: id, runtime: .claude, mood: mood, chair: chair, sessionId: "s-\(id)")
    }

    private func ref(_ committee: String = "c", _ root: UInt64 = 1) -> TownThreadRef {
        TownThreadRef(committee: committee, root: root, title: "t")
    }

    // MARK: Layout

    @Test func layoutIsTheSameWhateverTheOrder() {
        let names = (0..<30).map { "committee-\($0)" }
        let a = TownLayout(committees: names)
        let b = TownLayout(committees: names.reversed())
        #expect(a == b)
        #expect(a.cells.count == 30)
        // One lot each, never the hall's.
        #expect(Set(a.cells.values).count == 30)
        #expect(!a.cells.values.contains(TownLayout.GridCell(x: 0, y: 0)))
    }

    @Test func aSmallTownSitsAroundItsHall() {
        let layout = TownLayout(committees: ["only"])
        let cell = try! #require(layout.cells["only"])
        // The first lot is beside the hall, not at a corner.
        #expect(abs(cell.x) + abs(cell.y) == 1)
        let rooms = layout.room("only")!
        #expect(!rooms.intersects(layout.hall))
    }

    @Test func spiralFillsRingsNearestFirst() {
        let cells = TownLayout.spiral(count: 24)
        #expect(cells.prefix(8).allSatisfy { max(abs($0.x), abs($0.y)) == 1 })
        #expect(cells.prefix(4).allSatisfy { abs($0.x) + abs($0.y) == 1 })
        #expect(cells.dropFirst(8).allSatisfy { max(abs($0.x), abs($0.y)) == 2 })
    }

    @Test func roomsDoNotOverlap() {
        let layout = TownLayout(committees: (0..<30).map { "c\($0)" })
        let rooms = layout.cells.keys.compactMap(layout.room) + [layout.hall]
        for (i, a) in rooms.enumerated() {
            for b in rooms.dropFirst(i + 1) { #expect(!a.intersects(b)) }
        }
    }

    @Test func seatsAreInsideTheRoomAndDistinct() {
        let room = CGRect(x: 0, y: 0, width: TownLayout.room.width, height: TownLayout.room.height)
        let seats = (0..<10).map { TownLayout.seat($0, in: room) }
        #expect(Set(seats.map { "\($0.x),\($0.y)" }).count == 10)
        #expect(seats.allSatisfy { room.contains($0) })
    }

    @Test func aWalkAcrossTownGoesOutOneDoorAndInTheOther() {
        let layout = TownLayout(committees: (0..<12).map { "c\($0)" })
        let names = layout.cells.keys.sorted()
        // Two houses in different rows.
        let a = names.first { layout.cells[$0]!.y == 1 }!
        let b = names.first { layout.cells[$0]!.y == -1 }!
        let ra = layout.room(a)!, rb = layout.room(b)!
        let from = CGPoint(x: ra.midX, y: ra.midY), to = CGPoint(x: rb.midX, y: rb.midY)
        let path = TownLayout.path(from: from, in: ra, to: to, in: rb)
        #expect(path.first == from)
        #expect(path.last == to)
        #expect(path.contains(TownLayout.door(of: ra)))
        #expect(path.contains(TownLayout.door(of: rb)))
        // Along the streets: every leg between the doors is straight across or up.
        let inner = Array(path.dropFirst().dropLast())
        for (p, q) in zip(inner, inner.dropFirst()) {
            #expect(abs(p.x - q.x) < 0.01 || abs(p.y - q.y) < 0.01)
        }
        // And never through a third house.
        let others = layout.cells.keys.filter { $0 != a && $0 != b }.compactMap(layout.room) + [layout.hall]
        for (p, q) in zip(inner.dropFirst(), inner.dropFirst().dropLast()) {
            let leg = CGRect(x: min(p.x, q.x), y: min(p.y, q.y), width: abs(p.x - q.x) + 0.1, height: abs(p.y - q.y) + 0.1)
            #expect(others.allSatisfy { !$0.insetBy(dx: 2, dy: 2).intersects(leg) })
        }
    }

    @Test func aWalkInsideARoomIsStraight() {
        let room = CGRect(x: 0, y: 0, width: 200, height: 100)
        let path = TownLayout.path(from: CGPoint(x: 10, y: 10), in: room, to: CGPoint(x: 90, y: 50), in: room)
        #expect(path.count == 2)
    }

    // MARK: Events from two readings

    private func input(_ senators: [TownSenator]) -> TownState {
        TownState(committees: [TownCommittee(name: "c", senators: senators)])
    }

    @Test func theFirstReadingIsQuiet() {
        #expect(TownDiff.events(from: nil, to: input([senator("a"), senator("b", .working)])).isEmpty)
    }

    @Test func moodsArrivalsAndDepartures() {
        let old = input([senator("a", .idle), senator("b", .working)])
        let new = input([senator("a", .working), senator("c")])
        #expect(TownDiff.events(from: old, to: new) == [
            .mood(senator: "a", .working), .arrived(senator: "c"), .left(senator: "b"),
        ])
    }

    @Test func aSessionStoppingLeavesTownAndStartingComesBack() {
        let old = input([senator("a", .working), senator("b", .stopped)])
        let new = input([senator("a", .stopped), senator("b", .idle), senator("c", .stopped)])
        // A new senator already stopped does not arrive.
        #expect(TownDiff.events(from: old, to: new) == [.left(senator: "a"), .arrived(senator: "b")])
    }

    private func entry(
        _ kind: String, from: String = "/s::c::senator::a", to: [String] = ["/s::c::senator::b"],
        behalf: String? = nil, thread: UInt64? = 7, seq: UInt64 = 9, state: String? = nil
    ) -> TownActivityEntry {
        TownActivityEntry(committee: "c", seq: seq, kind: kind, from: from, to: to, onBehalfOf: behalf,
                          thread: thread, line: "hello there", state: state)
    }

    @Test func whatIsSaidIsAVisitFromItsRealAuthor() {
        let t = TownThreadRef(committee: "c", root: 7, title: "hello there")
        #expect(TownActivity.events([entry("answer")]) == [.message(from: "a", to: "b", thread: t, line: "hello there")])
        // The Speaker speaking for a senator: the senator walks.
        #expect(TownActivity.events([entry("question", from: "speaker", behalf: "senator::z")])
            == [.message(from: "z", to: "b", thread: t, line: "hello there")])
        // A question opens its own thread; nobody addressed goes to the house.
        let own = TownThreadRef(committee: "c", root: 9, title: "hello there")
        #expect(TownActivity.events([entry("question", to: [], thread: nil)])
            == [.message(from: "a", to: nil, thread: own, line: "hello there")])
    }

    @Test func aSettledStatusIsACheckAndOthersAreNothing() {
        let t = TownThreadRef(committee: "c", root: 7, title: "hello there")
        #expect(TownActivity.events([entry("status", state: "active")]) == [.answered(thread: t, a: "a", b: "b")])
        #expect(TownActivity.events([entry("status", state: "processing")]).isEmpty)
        // Comings and goings are the roster's, not repeated from here.
        #expect(TownActivity.events([entry("joined", state: "unknown"), entry("left", state: "away")]).isEmpty)
        // To oneself is not a visit.
        #expect(TownActivity.events([entry("note", to: ["senator::a"])]).isEmpty)
    }

    @Test func entriesAreActedInLogOrder() {
        let late = entry("note", seq: 20)
        let early = entry("answer", from: "senator::b", to: ["senator::a"], seq: 10)
        let events = TownActivity.events([late, early])
        guard case let .message(first, _, _, _) = events.first else { Issue.record("no visit"); return }
        #expect(first == "b")
    }

    @Test func aRemoteMembersStateIsItsMood() {
        #expect(TownMood(session: "working") == .working)
        #expect(TownMood(session: "paused") == .paused)
        #expect(TownMood(session: "away") == .stopped)
        #expect(TownMood(session: "unknown") == .idle)
        #expect(TownMood(session: nil) == .idle)
    }

    // MARK: Director

    private func message(_ from: String, _ to: String? = nil, root: UInt64 = 1) -> TownEvent {
        .message(from: from, to: to, thread: ref("c", root), line: "one two three four five six seven")
    }

    @Test func visitsAreCappedOnePerSenator() {
        var director = TownDirector()
        director.maxWalkers = 2
        let present: Set<String> = ["a", "b", "c", "d"]
        let actions = director.accept(
            [message("a", "b", root: 1), message("a", "c", root: 2), message("b", "a", root: 3), message("c", "d", root: 4)],
            present: present, reduceMotion: false
        )
        // a and b go; a's second visit and c's wait.
        #expect(actions.count == 2)
        #expect(director.out == ["a", "b"])
        #expect(director.queue.count == 2)
        // a comes home: one slot frees (b is still out), and a's next
        // visit is first in the queue.
        let next = director.cameHome("a")
        #expect(next.count == 1)
        if case let .visit(from, to, _, line, walk) = next.first {
            #expect(from == "a")
            #expect(to == "c")
            #expect(walk)
            #expect(line == "one two three four five six…")
        } else {
            Issue.record("expected a visit")
        }
    }

    @Test func theQueueDropsTheOldest() {
        var director = TownDirector()
        director.maxWalkers = 1
        director.maxQueue = 3
        let events = (0..<6).map { message("a", root: UInt64($0)) }
        _ = director.accept(events, present: ["a"], reduceMotion: false)
        #expect(director.queue.count == 3)
        if case let .visit(_, _, thread, _, _) = director.queue.first {
            #expect(thread.root == 3)
        }
    }

    @Test func aMoodWaitsUntilTheWalkerIsHome() {
        var director = TownDirector()
        _ = director.accept([message("a", "b")], present: ["a", "b"], reduceMotion: false)
        let now = director.accept([.mood(senator: "a", .working), .mood(senator: "b", .paused)], present: ["a", "b"], reduceMotion: false)
        #expect(now == [.mood(senator: "b", .paused)])
        #expect(director.cameHome("a") == [.mood(senator: "a", .working)])
    }

    @Test func reduceMotionFadesAtOnce() {
        var director = TownDirector()
        director.maxWalkers = 1
        let actions = director.accept([message("a", "b"), message("c", "b")], present: ["a", "b", "c"], reduceMotion: true)
        #expect(actions.count == 2)
        #expect(actions.allSatisfy { if case let .visit(_, _, _, _, walk) = $0 { !walk } else { false } })
        #expect(director.out.isEmpty)
    }

    @Test func strangersComeAsGuests() {
        var director = TownDirector()
        // Someone from elsewhere visits; it counts as out like anyone.
        #expect(director.accept([message("ghost", "a")], present: ["a"], reduceMotion: false).count == 1)
        #expect(director.out == ["ghost"])
        _ = director.cameHome("ghost")
        // An unknown listener: the visit goes to the thread's house.
        let actions = director.accept([message("a", "ghost")], present: ["a"], reduceMotion: false)
        if case let .visit(_, to, _, _, _) = actions.first { #expect(to == nil) } else { Issue.record("no visit") }
    }

    @Test func aSenatorWhoLeftSaysNothing() {
        var director = TownDirector()
        director.maxWalkers = 1
        _ = director.accept([message("a", root: 1), message("b", root: 2)], present: ["a", "b"], reduceMotion: false)
        _ = director.accept([.left(senator: "b")], present: ["a"], reduceMotion: false)
        #expect(director.queue.isEmpty)
    }

    @Test func arrivalsWalkWhileThereIsRoomElseAppear() {
        var director = TownDirector()
        director.maxWalkers = 1
        let present: Set<String> = ["a", "b"]
        #expect(director.accept([.arrived(senator: "a"), .left(senator: "b")], present: present, reduceMotion: false) == [
            .arrive(senator: "a", walk: true), .depart(senator: "b", walk: false),
        ])
        // The walker in counts against the limit until home.
        #expect(director.out == ["a"])
        _ = director.cameHome("a")
        #expect(director.accept([.left(senator: "a")], present: present, reduceMotion: true) == [.depart(senator: "a", walk: false)])
        #expect(director.accept([.arrived(senator: "ghost")], present: present, reduceMotion: false).isEmpty)
    }

    @Test func aSettledThreadChecksThoseInTown() {
        var director = TownDirector()
        #expect(director.accept([.answered(thread: ref(), a: "a", b: "ghost")], present: ["a"], reduceMotion: false)
            == [.check(thread: ref(), a: "a", b: nil)])
        #expect(director.accept([.answered(thread: ref(), a: "x", b: "y")], present: ["a"], reduceMotion: false).isEmpty)
    }

    @Test func theWayInFromTheEdge() {
        let layout = TownLayout(committees: ["a", "b", "c"])
        let room = layout.room("a")!
        let edge = layout.edgeX(for: room)
        #expect(edge < layout.bounds.minX || edge > layout.bounds.maxX)
        let path = TownLayout.pathFromEdge(edge, to: CGPoint(x: room.midX, y: room.midY), in: room)
        #expect(path.first?.x == edge)
        #expect(path.contains(TownLayout.door(of: room)))
        #expect(path.last == CGPoint(x: room.midX, y: room.midY))
    }

    // MARK: Placement

    @Test func aBubbleGoesAboveTheHeadWhenNothingIsThere() {
        let view = CGRect(x: -500, y: -500, width: 1000, height: 1000)
        let b = TownPlacement.bubble(size: CGSize(width: 100, height: 22), head: .zero, obstacles: [], viewport: view)
        #expect(b.centre.x == 0)
        #expect(b.centre.y > 11)
        #expect(b.tail == .zero)
    }

    @Test func aBubbleAvoidsAFaceAndStaysInView() {
        let view = CGRect(x: -500, y: -500, width: 1000, height: 1000)
        let size = CGSize(width: 100, height: 22)
        // Someone stands right above the speaker.
        let face = CGRect(x: -20, y: 15, width: 40, height: 30)
        let b = TownPlacement.bubble(size: size, head: .zero, obstacles: [face], viewport: view)
        let rect = CGRect(x: b.centre.x - 50, y: b.centre.y - 11, width: 100, height: 22)
        #expect(!rect.intersects(face))
        // At the top edge of the view it is kept inside.
        let edge = TownPlacement.bubble(size: size, head: CGPoint(x: 480, y: 490), obstacles: [], viewport: view)
        #expect(edge.centre.x + 50 <= 500)
        #expect(edge.centre.y + 11 <= 500)
    }

    @Test func aFaceCountsMoreThanASign() {
        let view = CGRect(x: -500, y: -500, width: 1000, height: 1000)
        let size = CGSize(width: 100, height: 22)
        // Faces on both sides and above; a sign higher up: the bubble may
        // cover the sign, never a face.
        let faces = [CGRect(x: -60, y: 10, width: 120, height: 40), CGRect(x: 40, y: -10, width: 200, height: 30),
                     CGRect(x: -240, y: -10, width: 200, height: 30)]
        let sign = CGRect(x: -200, y: 60, width: 400, height: 100)
        let b = TownPlacement.bubble(size: size, head: .zero, obstacles: faces, soft: [sign], viewport: view)
        let rect = CGRect(x: b.centre.x - 50, y: b.centre.y - 11, width: 100, height: 22)
        #expect(faces.allSatisfy { !$0.intersects(rect) })
        #expect(rect.intersects(sign))
    }

    @Test func namesNeverOverlap() {
        let a = TownPlacement.Label(id: "a", rect: CGRect(x: 0, y: 0, width: 40, height: 10), priority: 0)
        let chair = TownPlacement.Label(id: "chair", rect: CGRect(x: 20, y: 0, width: 40, height: 10), priority: 3)
        let c = TownPlacement.Label(id: "c", rect: CGRect(x: 10, y: -11, width: 40, height: 10), priority: 0)
        let far = TownPlacement.Label(id: "far", rect: CGRect(x: 300, y: 0, width: 40, height: 10), priority: 0)
        let out = TownPlacement.labels([a, chair, c, far], step: 11)
        #expect(out["chair"] == .some(0))
        #expect(out["far"] == .some(0))
        #expect(out["a"] == .some(11))
        // Whatever is shown, nothing overlaps.
        let shown = [a, chair, c, far].compactMap { l in out[l.id]!.map { l.rect.offsetBy(dx: 0, dy: -$0) } }
        for (i, r) in shown.enumerated() {
            for q in shown.dropFirst(i + 1) { #expect(!r.intersects(q)) }
        }
        // With no room left a name hides.
        let crowd = (0..<4).map { TownPlacement.Label(id: "\($0)", rect: CGRect(x: 0, y: 0, width: 40, height: 10), priority: 0) }
        #expect(TownPlacement.labels(crowd, step: 11).values.filter { $0 == nil }.count == 2)
    }

    // MARK: Text

    @Test func firstWordsAndIds() {
        #expect(TownText.firstWords("Hello there\nsecond line") == "Hello there")
        #expect(TownText.firstWords("a b c d e f g h") == "a b c d e f…")
        #expect(TownText.firstWords(String(repeating: "x", count: 50)).count == 34)
        #expect(TownText.senatorId("[senator::claude-1]") == "claude-1")
        #expect(TownText.senatorId("office::secretary") == "secretary")
        #expect(TownText.senatorId("codex-2") == "codex-2")
        #expect(TownRuntime(guessing: "agy-01a2") == .agy)
        #expect(TownRuntime(guessing: "copilot") == .other)
    }

    // MARK: Scene

    @Test func aClickFindsWhatIsUnderIt() {
        let scene = TownScene(size: CGSize(width: 900, height: 700))
        scene.apply(TownScript.quiet)
        scene.fit()
        let chair = try! #require(scene.position(of: "claude-chair"))
        #expect(scene.hit(at: scene.viewPoint(CGPoint(x: chair.x, y: chair.y + 14))) == .senator(id: "claude-chair", sessionId: "demo-claude-chair"))
        let hp = CGPoint(x: scene.layout.hall.minX + 20, y: scene.layout.hall.minY + 20)
        #expect(scene.hit(at: scene.viewPoint(hp)) == .hall)
        let docs = try! #require(scene.layout.room("docs"))
        #expect(scene.hit(at: scene.viewPoint(CGPoint(x: docs.minX + 14, y: docs.minY + 14))) == .committee("docs"))
        // A stopped senator is not there to be clicked.
        let sheet = try! #require(scene.position(of: "codex-sheet"))
        #expect(scene.hit(at: scene.viewPoint(CGPoint(x: sheet.x, y: sheet.y + 14))) != .senator(id: "codex-sheet", sessionId: "demo-codex-sheet"))
    }

    @Test func aLargeTownBuilds() {
        let scene = TownScene(size: CGSize(width: 900, height: 700))
        scene.apply(TownScript.large)
        scene.apply(TownScript.large, events: TownScript.largeEvents)
        #expect(scene.layout.cells.count == 30)
    }
}
