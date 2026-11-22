import AppKit
import SwiftUI

/// The app's design tokens: every colour, type size, gap and corner the
/// views use comes from here, so the window reads as one thing in both
/// appearances.
///
/// Surfaces are near-neutral greys that step in tone instead of being
/// separated by lines. The accent (the blue of Senate's icon) is spent
/// sparingly: the one filled button in a view, the unread dot, the caret.
/// What a state means (something waits, something failed, something is
/// live) has its own colours, apart from the accent.
enum Theme {
    // MARK: Surfaces

    /// The content column.
    static let surface = dynamic(0xFBFBFA, 0x151516)
    /// The sidebar: one step from the content.
    static let sidebar = dynamic(0xF1F1F0, 0x1C1C1E)
    /// What is set apart on the surface: your own messages, cards.
    static let container = dynamic(0xF3F3F2, 0x222224)
    /// What is set apart on a container: chips, code, a hovered row.
    static let containerHigh = dynamic(0xE8E8E7, 0x2E2E31)
    /// The selected sidebar row.
    static let selection = dynamic(0xDBDBDA, 0x38383C)
    /// What floats over the content: the reply box.
    static let bright = dynamic(0xFFFFFF, 0x242426)

    // MARK: Text and lines

    static let onSurface = dynamic(0x161617, 0xEDEDEE, contrast: (0x000000, 0xFFFFFF))
    static let onSurfaceVariant = dynamic(0x6B6B6E, 0xA3A3A8, contrast: (0x3F3F43, 0xD0D0D5))
    /// The rare hairline, where tone alone would not show an edge.
    static let outline = dynamic(0xDFDFDE, 0x343437, contrast: (0x85858A, 0x8A8A90))

    // MARK: Accent

    static let primary = dynamic(0x2F52C0, 0xA5B8FF, contrast: (0x1C3691, 0xCDD8FF))
    static let onPrimary = dynamic(0xFFFFFF, 0x0F1E57)
    static let primaryContainer = dynamic(0xE1E8FF, 0x25346E)
    static let onPrimaryContainer = dynamic(0x1C3080, 0xDDE4FF)

    // MARK: Meaning

    /// Something waits on you.
    static let attention = dynamic(0x8A4F00, 0xFFC56E, contrast: (0x5C3400, 0xFFDFA8))
    static let attentionContainer = dynamic(0xFFEFD2, 0x3A2A0C)
    /// Refusing, closing, or something that failed.
    static let danger = dynamic(0xB42318, 0xFF9E91, contrast: (0x7F140C, 0xFFCDC5))
    static let dangerContainer = dynamic(0xFDE8E5, 0x47191A)
    /// Online, live.
    static let success = dynamic(0x1E7A3C, 0x6BD68D, contrast: (0x0F4F24, 0xB5F0C7))
    static let successContainer = dynamic(0xDDF3E3, 0x143522)

    // MARK: Who a mention names

    static let agent = dynamic(0x4A47C9, 0xADABFF, contrast: (0x2E2B94, 0xD3D2FF))
    static let speaker = dynamic(0x9A4F06, 0xFFB86B, contrast: (0x6B3500, 0xFFD9A8))
    static let committee = dynamic(0x0E6F73, 0x6FD3D6, contrast: (0x06494C, 0xB2ECEE))

    /// A colour for each appearance, and for each with Increase Contrast
    /// when that needs another value.
    private static func dynamic(_ light: UInt32, _ dark: UInt32, contrast: (UInt32, UInt32)? = nil) -> Color {
        Color(nsColor: NSColor(name: nil) { appearance in
            let names: [NSAppearance.Name] = [
                .aqua, .darkAqua, .accessibilityHighContrastAqua, .accessibilityHighContrastDarkAqua,
            ]
            switch appearance.bestMatch(from: names) {
            case .darkAqua: return rgb(dark)
            case .accessibilityHighContrastAqua: return rgb(contrast?.0 ?? light)
            case .accessibilityHighContrastDarkAqua: return rgb(contrast?.1 ?? dark)
            default: return rgb(light)
            }
        })
    }

    private nonisolated static func rgb(_ hex: UInt32) -> NSColor {
        NSColor(
            srgbRed: CGFloat((hex >> 16) & 0xFF) / 255,
            green: CGFloat((hex >> 8) & 0xFF) / 255,
            blue: CGFloat(hex & 0xFF) / 255,
            alpha: 1
        )
    }

    // MARK: Type

    /// The system face throughout; the monospaced one for commands and paths.
    enum Fonts {
        /// The one large line of the home view.
        static let display = Font.system(size: 28)
        /// A thread's title.
        static let title = Font.system(size: 20, weight: .semibold)
        /// A card's first line.
        static let heading = Font.system(size: 15, weight: .semibold)
        /// What is read at length: a thread's messages, the reply being written.
        static let reading = Font.system(size: 16)
        /// Text in cards and rows of the content column.
        static let text = Font.system(size: 14)
        /// Sidebar rows.
        static let row = Font.system(size: 13)
        /// Buttons and names.
        static let label = Font.system(size: 13, weight: .medium)
        /// Ages, paths, notes.
        static let meta = Font.system(size: 12)
        static let chip = Font.system(size: 11, weight: .medium)
        static let code = Font.system(size: 12.5, design: .monospaced)
    }

    /// Added between the lines of reading text (16 pt on a 24 pt line).
    static let readingLineSpacing: CGFloat = 5

    // MARK: Space and shape

    /// Gaps, on a 4 pt grid.
    enum Space {
        static let xs: CGFloat = 4
        static let s: CGFloat = 8
        static let m: CGFloat = 12
        static let l: CGFloat = 16
        static let xl: CGFloat = 24
        static let xxl: CGFloat = 32
        static let xxxl: CGFloat = 48
    }

    enum Radius {
        static let code: CGFloat = 10
        static let card: CGFloat = 16
        static let bubble: CGFloat = 20
        static let composer: CGFloat = 24
    }

    /// The width text is read at, centred in the content column.
    static let readingWidth: CGFloat = 700
    /// The reply box is a little wider than the text above it.
    static let composerWidth: CGFloat = 732
    static let sidebarWidth: CGFloat = 232
    /// The height of every sidebar row.
    static let rowHeight: CGFloat = 32
}
