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

    private func input(_ senators: [TownSenator], threads: [TownThread] = []) -> TownInput {
        TownInput(state: TownState(committees: [TownCommittee(name: "c", senators: senators)]), threads: threads)
    }

    private func thread(_ updated: UInt64, from: String = "a", to: String? = "b") -> TownThread {
        TownThread(ref: ref(), updated: updated, speaker: from, listener: to, line: "hello there")
    }

    @Test func theFirstReadingIsQuiet() {
        #expect(TownDiff.events(from: nil, to: input([senator("a")], threads: [thread(5)])).isEmpty)
    }

    @Test func moodsArrivalsAndDepartures() {
        let old = input([senator("a", .idle), senator("b", .working)])
        let new = input([senator("a", .working), senator("c")])
        #expect(TownDiff.events(from: old, to: new) == [
            .mood(senator: "a", .working), .arrived(senator: "c"), .left(senator: "b"),
        ])
    }

    @Test func aThreadThatGrewIsAMessage() {
        let people = [senator("a"), senator("b")]
        let old = input(people, threads: [thread(5)])
        #expect(TownDiff.events(from: old, to: input(people, threads: [thread(5)])).isEmpty)
        #expect(TownDiff.events(from: old, to: input(people, threads: [thread(6)])) == [
            .message(from: "a", to: "b", thread: ref(), line: "hello there"),
        ])
        // A new thread is a message too; one to oneself is not.
        let fresh = TownThread(ref: ref("c", 9), updated: 9, speaker: "b", listener: "a", line: "x")
        let selfish = TownThread(ref: ref("c", 10), updated: 10, speaker: "a", listener: "a", line: "y")
        #expect(TownDiff.events(from: old, to: input(people, threads: [thread(5), fresh, selfish])) == [
            .message(from: "b", to: "a", thread: ref("c", 9), line: "x"),
        ])
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

    @Test func strangersAreNotActedOut() {
        var director = TownDirector()
        #expect(director.accept([message("ghost", "a")], present: ["a"], reduceMotion: false).isEmpty)
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
        scene.apply(TownScript.quiet.state)
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
        scene.apply(TownScript.large.state)
        scene.apply(TownScript.large.state, events: TownScript.largeEvents)
        #expect(scene.layout.cells.count == 30)
    }
}
