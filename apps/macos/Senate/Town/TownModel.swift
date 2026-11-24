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
}

/// A thread, as far as the town needs it: where it is and what it is called.
struct TownThreadRef: Hashable, Sendable {
    var committee: String
    var root: UInt64
    var title: String

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
    case arrived(senator: String)
    case left(senator: String)
}

/// A thread between agents as one state of the store shows it: enough to
/// tell, from two states, that someone said something in it.
struct TownThread: Hashable, Sendable {
    var ref: TownThreadRef
    /// Its last seq: it grows when someone writes in it.
    var updated: UInt64
    /// Who spoke last, as far as the store tells (the latest news's author,
    /// else who opened it).
    var speaker: String
    /// Whom it is spoken to.
    var listener: String?
    /// The first line of what was said, else the thread's title.
    var line: String
}

/// One reading of the store: the town and the threads in flight.
struct TownInput: Hashable, Sendable {
    var state: TownState
    var threads: [TownThread] = []
}

enum TownDiff {
    /// What happened between two readings. Nothing for the first one: a
    /// town that opens is quiet, not a replay.
    static func events(from old: TownInput?, to new: TownInput) -> [TownEvent] {
        guard let old else { return [] }
        var events: [TownEvent] = []
        let before = old.state.senators
        let after = new.state.senators
        for id in after.keys.sorted() {
            guard let now = after[id] else { continue }
            if let was = before[id] {
                if was.mood != now.mood { events.append(.mood(senator: id, now.mood)) }
            } else {
                events.append(.arrived(senator: id))
            }
        }
        for id in before.keys.sorted() where after[id] == nil {
            events.append(.left(senator: id))
        }
        let known = Dictionary(old.threads.map { ($0.ref.key, $0.updated) }, uniquingKeysWith: max)
        for thread in new.threads.sorted(by: { ($0.updated, $0.ref.key) < ($1.updated, $1.ref.key) }) {
            if let seen = known[thread.ref.key], seen >= thread.updated { continue }
            guard thread.speaker != thread.listener else { continue }
            events.append(.message(
                from: thread.speaker, to: thread.listener, thread: thread.ref, line: thread.line
            ))
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
