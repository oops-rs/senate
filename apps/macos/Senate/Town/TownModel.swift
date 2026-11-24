import Foundation

// The Town's model: what the scene draws is a pure function of a small
// `TownState`, and what it animates is a stream of `TownEvent`s. Neither
// knows the daemon, AppKit or SpriteKit, so the same model runs the scene
// on iOS later; `TownFeed` makes both from the app's store.

/// The agent runtime a senator runs on: the glyph on its shirt.
enum TownRuntime: String, Sendable, CaseIterable {
    case claude, codex, agy, other

    init(_ name: String) {
        self = TownRuntime(rawValue: name.lowercased()) ?? .other
    }
}

/// What a senator is doing, as the town shows it.
enum TownMood: String, Sendable, CaseIterable {
    /// Mid-turn: busy at its desk.
    case working
    /// Nothing to do: it wanders about its room.
    case idle
    /// A prompt or a question waits on a person: it raises its hand.
    case waiting
    /// Recorded paused: asleep at its desk.
    case paused
    /// Its terminal is not running: not in the room.
    case stopped
}

/// One senator: a little person in its committee's house.
struct TownSenator: Hashable, Sendable, Identifiable {
    /// The senator's id, bare (`claude-14ed…`): what messages name.
    var id: String
    var name: String
    var runtime: TownRuntime
    var mood: TownMood
    var chair: Bool = false
    /// This machine's session for it, which a click opens.
    var sessionId: String?
    /// What it is working on, when known (its latest status title).
    var doing: String?
}

/// A thread, as far as the town needs it: where it is and what it is called.
struct TownThreadRef: Hashable, Sendable {
    var committee: String
    var root: UInt64
    var title: String
    /// Who asked, by name, for a thread waiting on the person.
    var asker: String?

    var key: String { "\(committee):\(root)" }
}

/// A committee: a house, its senators living there (the chair first), and
/// the threads waiting on the person there (a visitor at its door).
struct TownCommittee: Hashable, Sendable, Identifiable {
    var name: String
    var senators: [TownSenator]
    var waiting: [TownThreadRef] = []

    var id: String { name }
}

/// The town hall: the Speaker's desk, where the Secretary works.
struct TownHall: Hashable, Sendable {
    var secretary: TownSenator?
    /// Items awaiting the Speaker.
    var docket: Int = 0
}

/// Everything the scene draws.
struct TownState: Hashable, Sendable {
    var committees: [TownCommittee] = []
    var hall = TownHall()
    /// The person things wait on (`alice`), for the visitors' labels.
    var person: String?

    /// Every senator by id, the Secretary included.
    var senators: [String: TownSenator] {
        var all: [String: TownSenator] = [:]
        for committee in committees {
            for senator in committee.senators where all[senator.id] == nil { all[senator.id] = senator }
        }
        if let secretary = hall.secretary, all[secretary.id] == nil { all[secretary.id] = secretary }
        return all
    }

    /// The committee a senator lives in; `nil` for the Secretary (the hall).
    func home(of id: String) -> String? {
        committees.first { $0.senators.contains { $0.id == id } }?.name
    }
}

/// Something that happened, for the scene to act out.
enum TownEvent: Hashable, Sendable {
    /// `from` said something in `thread` to `to` (a senator, or `nil` when
    /// the thread names no one in town: it goes to the thread's house).
    case message(from: String, to: String?, thread: TownThreadRef, line: String)
    case mood(senator: String, TownMood)
    /// Came into town: a new senator, or one whose session started again.
    case arrived(senator: String)
    /// Left town: gone from the state, or its session stopped.
    case left(senator: String)
    /// A thread between `a` and `b` was answered or closed.
    case answered(thread: TownThreadRef, a: String?, b: String?)
}

enum TownDiff {
    /// What happened between two readings of the town: moods, arrivals and
    /// departures. Nothing for the first reading: a town that opens is
    /// quiet, not a replay.
    static func events(from old: TownState?, to new: TownState) -> [TownEvent] {
        guard let old else { return [] }
        var events: [TownEvent] = []
        let before = old.senators
        let after = new.senators
        for id in after.keys.sorted() {
            guard let now = after[id] else { continue }
            if let was = before[id] {
                if was.mood == now.mood { continue }
                // A session stopping leaves town; one starting again comes back.
                if now.mood == .stopped {
                    events.append(.left(senator: id))
                } else if was.mood == .stopped {
                    events.append(.arrived(senator: id))
                } else {
                    events.append(.mood(senator: id, now.mood))
                }
            } else if now.mood != .stopped {
                events.append(.arrived(senator: id))
            }
        }
        for id in before.keys.sorted() where after[id] == nil {
            events.append(.left(senator: id))
        }
        return events
    }
}

/// One entry of a committee's activity, as the town needs it (the core's
/// `ActivityEntry`, without the daemon's types).
struct TownActivityEntry: Hashable, Sendable {
    var committee: String
    var seq: UInt64
    /// `question`, `request`, `answer`, `note`, `status`, `escalation`,
    /// `decision`, `joined`, `left`, `state`, `session_started`, …
    var kind: String
    var from: String
    var to: [String]
    var onBehalfOf: String?
    var thread: UInt64?
    var line: String
    /// A status entry's committed thread state; a member's for the others.
    var state: String?
}

enum TownActivity {
    /// What is said in a thread: acted out as a visit.
    static let spoken: Set<String> = ["question", "request", "answer", "note", "escalation", "decision"]
    /// A thread's state that means it is settled.
    static let settled: Set<String> = ["active", "answered", "closed", "archived", "done"]

    /// The town's events in a batch of activity: what was said (a visit
    /// from its real author to the first it was said to) and threads
    /// settled (a ✓). Comings and goings come from the roster's own
    /// readings, which the store keeps current, so they are not repeated
    /// here.
    static func events(_ entries: [TownActivityEntry]) -> [TownEvent] {
        var events: [TownEvent] = []
        for entry in entries.sorted(by: { ($0.seq, $0.committee) < ($1.seq, $1.committee) }) {
            let author = TownText.senatorId(entry.onBehalfOf ?? entry.from)
            let listener = entry.to.first.map(TownText.senatorId)
            let ref = TownThreadRef(committee: entry.committee, root: entry.thread ?? entry.seq,
                                    title: TownText.firstWords(entry.line, words: 12, characters: 80))
            if spoken.contains(entry.kind) {
                guard author != listener else { continue }
                events.append(.message(from: author, to: listener, thread: ref, line: entry.line))
            } else if entry.kind == "status", let state = entry.state, settled.contains(state), entry.thread != nil {
                events.append(.answered(thread: ref, a: author, b: listener))
            }
        }
        return events
    }
}

enum TownText {
    /// A senator id from a mention or an address: `[senator::claude-1]`,
    /// `senator::claude-1`, `office::secretary` → `claude-1`, `secretary`.
    static func senatorId(_ mention: String) -> String {
        var text = mention.trimmingCharacters(in: .whitespaces)
        if text.hasPrefix("["), text.hasSuffix("]") { text = String(text.dropFirst().dropLast()) }
        if let range = text.range(of: "::", options: .backwards) { text = String(text[range.upperBound...]) }
        return text
    }

    /// The first words of a message, for a speech bubble: at most `words`
    /// words and `characters` characters, on one line, `…` when cut.
    static func firstWords(_ text: String, words: Int = 6, characters: Int = 34) -> String {
        let line = text.split(whereSeparator: \.isNewline).first.map(String.init) ?? ""
        let parts = line.split(whereSeparator: \.isWhitespace)
        var out = parts.prefix(words).joined(separator: " ")
        var cut = parts.count > words
        if out.count > characters {
            out = String(out.prefix(characters - 1)).trimmingCharacters(in: .whitespaces)
            cut = true
        }
        return cut ? out + "…" : out
    }
}
