import SwiftUI
import SenateCore

/// The pieces every section is built from, so a chip, a button or a card is
/// the same shape wherever it appears.

/// A small capsule that names a state or a place.
struct Chip: View {
    enum Tone { case neutral, accent, attention, danger, success }

    let text: String
    var tone: Tone = .neutral

    var body: some View {
        Text(verbatim: text)
            .font(Theme.Fonts.chip)
            .lineLimit(1)
            .padding(.horizontal, Theme.Space.s)
            .padding(.vertical, 3)
            .foregroundStyle(foreground)
            .background(background, in: Capsule())
    }

    private var foreground: Color {
        switch tone {
        case .neutral: Theme.onSurfaceVariant
        case .accent: Theme.onPrimaryContainer
        case .attention: Theme.attention
        case .danger: Theme.danger
        case .success: Theme.success
        }
    }

    private var background: Color {
        switch tone {
        case .neutral: Theme.containerHigh
        case .accent: Theme.primaryContainer
        case .attention: Theme.attentionContainer
        case .danger: Theme.dangerContainer
        case .success: Theme.successContainer
        }
    }
}

/// The app's buttons: capsules. One `filled` button at most in a view (what
/// you most likely came to do); the rest are `tonal`, and what refuses or
/// closes is `danger`.
struct PillButtonStyle: ButtonStyle {
    enum Kind { case filled, tonal, danger }

    var kind: Kind = .tonal
    @Environment(\.isEnabled) private var enabled

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(Theme.Fonts.label)
            .lineLimit(1)
            .padding(.horizontal, 14)
            .frame(minHeight: 30)
            .foregroundStyle(foreground)
            .background(background, in: Capsule())
            .contentShape(Capsule())
            .opacity(enabled ? (configuration.isPressed ? 0.7 : 1) : 0.45)
    }

    private var foreground: Color {
        switch kind {
        case .filled: Theme.onPrimary
        case .tonal: Theme.onSurface
        case .danger: Theme.danger
        }
    }

    private var background: Color {
        switch kind {
        case .filled: Theme.primary
        case .tonal, .danger: Theme.containerHigh
        }
    }
}

extension ButtonStyle where Self == PillButtonStyle {
    static var pill: PillButtonStyle { PillButtonStyle() }
    static func pill(_ kind: PillButtonStyle.Kind) -> PillButtonStyle { PillButtonStyle(kind: kind) }
}

extension View {
    /// Set apart as one object on the surface.
    func card(padding: CGFloat = Theme.Space.l) -> some View {
        self
            .padding(padding)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.container, in: RoundedRectangle(cornerRadius: Theme.Radius.card))
    }
}

/// A section's content: scrolled, at the width text is read at, centred in
/// the content column.
struct ReadingColumn<Content: View>: View {
    var spacing: CGFloat = Theme.Space.m
    @ViewBuilder let content: Content

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: spacing) {
                content
            }
            .frame(maxWidth: Theme.readingWidth, alignment: .leading)
            .padding(.horizontal, Theme.Space.xl)
            .padding(.top, Theme.Space.s)
            .padding(.bottom, Theme.Space.xxl)
            .frame(maxWidth: .infinity)
        }
    }
}

/// What a group of rows or cards is, in plain secondary text.
struct SectionHeader: View {
    let text: String

    init(_ text: String) {
        self.text = text
    }

    var body: some View {
        Text(verbatim: text)
            .font(Theme.Fonts.row)
            .foregroundStyle(Theme.onSurfaceVariant)
            .padding(.top, Theme.Space.m)
            .padding(.leading, Theme.Space.xs)
            .accessibilityAddTraits(.isHeader)
    }
}

/// A command or a path, as typed.
struct CodeBlock: View {
    let text: String
    var lines: Int? = nil

    var body: some View {
        Text(verbatim: text)
            .font(Theme.Fonts.code)
            .foregroundStyle(Theme.onSurface)
            .lineLimit(lines)
            .textSelection(.enabled)
            .padding(.horizontal, Theme.Space.m)
            .padding(.vertical, Theme.Space.s)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.containerHigh, in: RoundedRectangle(cornerRadius: Theme.Radius.code))
    }
}

/// Online or not, with a label for VoiceOver.
struct StatusDot: View {
    let online: Bool
    var size: CGFloat = 8

    var body: some View {
        Circle()
            .fill(online ? Theme.success : Theme.onSurfaceVariant.opacity(0.6))
            .frame(width: size, height: size)
            .accessibilityLabel(online ? L("a11y.online") : L("a11y.offline"))
    }
}

/// What a section shows when it has nothing: one quiet line, centred.
struct EmptyState: View {
    let text: String
    var detail: String? = nil

    var body: some View {
        VStack(spacing: Theme.Space.s) {
            Text(verbatim: text)
                .font(Theme.Fonts.display)
                .foregroundStyle(Theme.onSurface)
                .multilineTextAlignment(.center)
            if let detail {
                Text(verbatim: detail)
                    .font(Theme.Fonts.text)
                    .foregroundStyle(Theme.onSurfaceVariant)
                    .multilineTextAlignment(.center)
            }
        }
        .padding(Theme.Space.xxl)
        .frame(maxWidth: 560)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

/// `needs_human` → `needs human`.
func words(_ s: String) -> String {
    s.replacingOccurrences(of: "_", with: " ")
}

/// A section's name at the head of its column, and a note about it when
/// there is one to give.
struct PageHeading: View {
    let text: String
    var note: String? = nil

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.xs) {
            Text(verbatim: text)
                .font(Theme.Fonts.title)
                .foregroundStyle(Theme.onSurface)
                .accessibilityAddTraits(.isHeader)
            if let note {
                Text(verbatim: note)
                    .font(Theme.Fonts.row)
                    .foregroundStyle(Theme.onSurfaceVariant)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(.top, Theme.Space.xl)
        .padding(.leading, Theme.Space.xs)
    }
}

/// A quiet line of facts under a name: a path, an age.
struct MetaLine<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        HStack(spacing: Theme.Space.xs) {
            content
        }
        .font(Theme.Fonts.row)
        .foregroundStyle(Theme.onSurfaceVariant)
        .lineLimit(1)
    }
}
