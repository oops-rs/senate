import Foundation

/// What the scene does next.
enum TownAction: Hashable, Sendable {
    /// `from` visits `to` (or the thread's house when `to` is nil) and says
    /// `line`; with `walk` false (Reduce Motion) the bubble fades from one
    /// to the other and nobody moves.
    case visit(from: String, to: String?, thread: TownThreadRef, line: String, walk: Bool)
    /// Show a senator's mood (after its walk, when it is out).
    case mood(senator: String, TownMood)
    /// Walk in from the edge of town (or, without `walk`, fade in).
    case arrive(senator: String, walk: Bool)
    /// Walk out to the edge of town (or fade out).
    case depart(senator: String, walk: Bool)
    /// A brief ✓ over the two who settled a thread.
    case check(thread: TownThreadRef, a: String?, b: String?)
}

/// Turns events into the scene's actions, keeping the town calm: at most
/// `maxWalkers` senators out at once, one visit per senator at a time, a
/// short queue behind them (the oldest dropped when it overflows), and a
/// senator's mood changes held while it is out. Pure: the scene tells it
/// when a visit ends.
struct TownDirector {
    var maxWalkers = 6
    var maxQueue = 12

    /// Senators out on a visit.
    private(set) var out: Set<String> = []
    /// Visits waiting for a walker to come home or a free slot.
    private(set) var queue: [TownAction] = []
    /// Moods to show when their senator is back.
    private(set) var heldMoods: [String: TownMood] = [:]

    /// Take events. `present` is who is in town (a message from anyone else
    /// is not acted out); `reduceMotion` makes every visit a fade.
    mutating func accept(_ events: [TownEvent], present: Set<String>, reduceMotion: Bool) -> [TownAction] {
        var actions: [TownAction] = []
        for event in events {
            switch event {
            case let .message(from, to, thread, line):
                guard present.contains(from) else { continue }
                let target = to.flatMap { present.contains($0) ? $0 : nil }
                let visit = TownAction.visit(
                    from: from, to: target, thread: thread, line: TownText.firstWords(line), walk: !reduceMotion
                )
                if reduceMotion {
                    actions.append(visit)
                } else {
                    queue.append(visit)
                }
            case let .mood(senator, mood):
                if out.contains(senator) {
                    heldMoods[senator] = mood
                } else {
                    actions.append(.mood(senator: senator, mood))
                }
            case let .left(senator):
                // Gone: whatever it was going to say is not said.
                queue.removeAll { if case let .visit(from, _, _, _, _) = $0 { return from == senator } else { return false } }
                heldMoods[senator] = nil
                guard present.contains(senator), !out.contains(senator) else { continue }
                actions.append(.depart(senator: senator, walk: letOut(senator, reduceMotion)))
            case let .arrived(senator):
                guard present.contains(senator), !out.contains(senator) else { continue }
                actions.append(.arrive(senator: senator, walk: letOut(senator, reduceMotion)))
            case let .answered(thread, a, b):
                let a = a.flatMap { present.contains($0) ? $0 : nil }
                let b = b.flatMap { present.contains($0) ? $0 : nil }
                guard a != nil || b != nil else { continue }
                actions.append(.check(thread: thread, a: a, b: b))
            }
        }
        let started = dispatch()
        if queue.count > maxQueue { queue.removeFirst(queue.count - maxQueue) }
        return actions + started
    }

    /// A senator is home again: its held mood, and whoever can go next.
    mutating func cameHome(_ senator: String) -> [TownAction] {
        out.remove(senator)
        var actions: [TownAction] = []
        if let mood = heldMoods.removeValue(forKey: senator) { actions.append(.mood(senator: senator, mood)) }
        return actions + dispatch()
    }

    /// Whether an arrival or a departure walks: when motion is allowed and
    /// there is room among the walkers (it then counts as out until
    /// `cameHome`); otherwise it happens at once.
    private mutating func letOut(_ senator: String, _ reduceMotion: Bool) -> Bool {
        guard !reduceMotion, out.count < maxWalkers else { return false }
        out.insert(senator)
        return true
    }

    /// Forget everything in flight (the scene was rebuilt).
    mutating func reset() {
        out = []
        queue = []
        heldMoods = [:]
    }

    /// Start the queued visits whose senator is home, while there is room.
    private mutating func dispatch() -> [TownAction] {
        var started: [TownAction] = []
        var waiting: [TownAction] = []
        for action in queue {
            guard case let .visit(from, _, _, _, _) = action else { continue }
            if out.count < maxWalkers && !out.contains(from) {
                out.insert(from)
                started.append(action)
            } else {
                waiting.append(action)
            }
        }
        queue = waiting
        return started
    }
}
