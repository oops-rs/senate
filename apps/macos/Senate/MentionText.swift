import AppKit
import SwiftUI
import SenateCore

/// A text with its mentions shown as names: `[senator::claude-a956…]` reads
/// `senate-chair`, `[speaker::copilot]` reads `copilot (Speaker)`, in the
/// font around it with a quiet tint for what it names. The id stays one
/// step away: in the tooltip, in the context menu (Copy), and for VoiceOver.
/// The core splits the text and names each mention for the committee the
/// text belongs to (`\.mentionCommittee`); this view never resolves a name.
struct MentionText: View {
    let text: String
    @Environment(\.mentionCommittee) private var committee

    init(_ text: String) {
        self.text = text
    }

    var body: some View {
        let parts = Mentions.shared.parts(text, committee: committee)
        let ids = parts.compactMap(\.id)
        let line = parts.reduce(Text(verbatim: "")) { sum, part in
            Text("\(sum)\(piece(part))")
        }
        if ids.isEmpty {
            line
        } else {
            line
                .help(ids.map { "[\($0)]" }.joined(separator: "\n"))
                .contextMenu {
                    ForEach(ids, id: \.self) { id in
                        Button(L("mention.copy", "[\(id)]")) { Mentions.copy(id) }
                    }
                }
                .accessibilityCustomContent(Text(verbatim: L("a11y.ids")), Text(verbatim: ids.joined(separator: ", ")))
        }
    }

    private func piece(_ part: TextPart) -> Text {
        guard let kind = part.mention else { return Text(verbatim: part.text) }
        return Text(verbatim: part.display ?? part.text)
            .fontWeight(.medium)
            .foregroundStyle(MentionText.color(kind))
    }

    static func color(_ kind: MentionKind) -> Color {
        switch kind {
        case .senior: .primary
        case .junior, .anyAgent: .indigo
        case .speaker, .speakerFor: .orange
        case .committee: .teal
        case .guest: .secondary
        }
    }
}

extension EnvironmentValues {
    /// The committee the texts below belong to: names are aliases there.
    @Entry var mentionCommittee: String?
}

/// Split texts, kept until the core's names change (`mentionNamesVersion`):
/// drawing a list of a thousand rows splits nothing twice.
@Observable
final class Mentions {
    static let shared = Mentions()

    /// Read by every `MentionText`, so they redraw when names change.
    private(set) var version: UInt64 = 0
    @ObservationIgnored private var cache: [Key: [TextPart]] = [:]

    private struct Key: Hashable {
        let text: String
        let committee: String?
    }

    func parts(_ text: String, committee: String?) -> [TextPart] {
        _ = version
        let key = Key(text: text, committee: committee)
        if let hit = cache[key] { return hit }
        // Plain text needs no trip to the core.
        let parts = text.contains("[")
            ? splitMentions(text: text, committee: committee)
            : [TextPart(text: text, mention: nil, display: nil, id: nil)]
        if cache.count > 10_000 { cache.removeAll(keepingCapacity: true) }
        cache[key] = parts
        return parts
    }

    /// Called with each state: drop what was split under the old names.
    func update() {
        let now = mentionNamesVersion()
        if now != version {
            cache.removeAll(keepingCapacity: true)
            version = now
        }
    }

    /// Copy a mention's id, bracketed as it is written in running text.
    static func copy(_ id: String) {
        copyText("[\(id)]")
    }

    static func copyText(_ text: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
    }
}

/// One clock for every "how long ago" in the app, ticking every half
/// minute: a thousand rows share it instead of a timer each.
@Observable
final class Clock {
    static let shared = Clock()
    private(set) var now = Date.now

    private init() {
        Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(30))
                self?.now = .now
            }
        }
    }
}

/// A `MentionText` rewritten as the clock ticks with how long ago `date`
/// was: `build` gets that short age (`3m`) and returns the text.
struct TimedMention: View {
    let date: Date
    let build: (String) -> String

    var body: some View {
        MentionText(build(Ago.text(date, now: Clock.shared.now)))
    }
}

/// How long ago `date` was, short (`3m`), kept current.
struct Ago: View {
    let date: Date

    var body: some View {
        Text(verbatim: Ago.text(date, now: Clock.shared.now))
            .monospacedDigit()
            .accessibilityLabel(date.formatted(.relative(presentation: .named)))
    }

    /// `42s`, `3m`, `5h`, `2d`.
    nonisolated static func text(_ date: Date, now: Date) -> String {
        let s = max(0, Int(now.timeIntervalSince(date)))
        switch s {
        case ..<60: return "\(s)s"
        case ..<3600: return "\(s / 60)m"
        case ..<86400: return "\(s / 3600)h"
        default: return "\(s / 86400)d"
        }
    }
}
