import SwiftUI
import SenateCore

/// A text with its bracketed ids set apart: `[senator::alice/claude]` is shown
/// in a monospaced face and a colour for what it names, so it reads as an id
/// in the middle of a Chinese or an English sentence. The brackets stay: they
/// are part of the id's spelling and what a reader copies back into a tool.
/// The core finds the mentions (`splitMentions`), never this view.
struct MentionText: View {
    let text: String

    init(_ text: String) {
        self.text = text
    }

    var body: some View {
        parts.reduce(Text(verbatim: "")) { sum, part in
            Text("\(sum)\(piece(part))")
        }
    }

    private var parts: [TextPart] { splitMentions(text: text) }

    private func piece(_ part: TextPart) -> Text {
        guard let kind = part.mention else { return Text(verbatim: part.text) }
        // Monospaced in whatever font surrounds it, so it keeps the size of
        // a caption or a title (and Dynamic Type).
        return Text(verbatim: part.text)
            .monospaced()
            .fontWeight(.medium)
            .foregroundStyle(MentionText.color(kind))
    }

    static func color(_ kind: MentionKind) -> Color {
        switch kind {
        case .senior: .blue
        case .junior, .anyAgent: .purple
        case .speaker, .speakerFor: .orange
        case .committee: .teal
        case .guest: .gray
        }
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
