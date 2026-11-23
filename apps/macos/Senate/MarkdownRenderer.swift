import CoreText
import Foundation
import Markdown
import SenateCore
import SwiftUI
#if canImport(AppKit)
import AppKit
/// The platform's font and colour, as attributed strings take them.
typealias PlatformFont = NSFont
typealias PlatformColor = NSColor
#elseif canImport(UIKit)
import UIKit
typealias PlatformFont = UIFont
typealias PlatformColor = UIColor
#endif

/// A message's Markdown as one attributed string, from swift-markdown's
/// CommonMark/GFM tree: what `MessageText` lays out in one text view, so a
/// selection runs across its paragraphs, lists and code. Foundation and
/// Core Text only, no AppKit: the text view that shows it is the one
/// platform piece (`MessageText.swift` on macOS).
///
/// Built off the main thread (`MarkdownCache.prepare`), once per message
/// text; the colours are the Theme's own dynamic colours, so one string
/// serves light and dark. Mentions (`[senator::…]`) read as the names the
/// app shows. A table becomes an attachment that scrolls sideways (TextKit 2
/// lays out no text tables; the view makes it one, from `table`);
/// everything else is text.
nonisolated struct MarkdownRenderer: @unchecked Sendable {
    /// The Theme's colours and faces, as attributed strings take them.
    struct Style: @unchecked Sendable {
        var text: PlatformColor
        var secondary: PlatformColor
        var accent: PlatformColor
        var agent: PlatformColor
        var codeBackground: PlatformColor
        var font: PlatformFont = .systemFont(ofSize: 16)
        var code: PlatformFont = .monospacedSystemFont(ofSize: 13, weight: .regular)
        var lineSpacing: CGFloat = 5
        var paragraphSpacing: CGFloat = 10
    }

    let style: Style
    /// The committee the text belongs to: mentions are named for it.
    let committee: String?

    // MARK: Attributes `MessageTextView` draws from

    static let block = NSAttributedString.Key("SenateBlock")
    /// One code block's text, for its copy button.
    static let codeText = NSAttributedString.Key("SenateCodeText")
    /// A table (a `Table` value) on its one object-replacement character.
    static let table = NSAttributedString.Key("SenateTable")
    /// A mention's id (`[senator::claude]`), on the name shown for it.
    static let mention = NSAttributedString.Key("SenateMention")

    /// A table's cells, and each column's alignment as the Markdown
    /// declares it.
    final class Table: @unchecked Sendable {
        enum Alignment: Sendable { case left, center, right }
        let rows: [[String]]
        let alignments: [Alignment]
        init(rows: [[String]], alignments: [Alignment]) {
            self.rows = rows
            self.alignments = alignments
        }

        /// What a copy of the table gives: one line per row, cells
        /// separated by tabs.
        var plainText: String {
            rows.map { $0.joined(separator: "\t") }.joined(separator: "\n")
        }
    }
    enum Block: String { case code, quote, rule }

    func render(_ source: String) -> NSAttributedString {
        let (text, mentions) = Self.shielding(source, committee: committee)
        let document = Document(parsing: text, options: [])
        var builder = Builder(style: style, mentions: mentions)
        builder.blocks(document.children, context: Context())
        let out = builder.out
        // No trailing paragraph break: the view would draw an empty line.
        while out.string.hasSuffix("\n") {
            out.deleteCharacters(in: NSRange(location: out.length - 1, length: 1))
        }
        return out
    }

    /// Mentions swapped for private-use tokens before parsing (the parser
    /// would read `[senator::x]` as a link label), with the names to put
    /// back: `(display, id, written)`.
    static func shielding(_ text: String, committee: String?) -> (String, [(String, String, String)]) {
        guard text.contains("[") else { return (text, []) }
        var out = ""
        var mentions: [(String, String, String)] = []
        for part in splitMentions(text: text, committee: committee) {
            if let display = part.display, part.mention != nil {
                out += "\u{F0001}\(mentions.count)\u{F0002}"
                mentions.append((display, part.id ?? part.text, part.text))
            } else {
                out += part.text
            }
        }
        return (out, mentions)
    }

    struct Context {
        var indent: CGFloat = 0
        var quote = false
    }

    struct Builder {
        let style: Style
        let mentions: [(String, String, String)]
        let out = NSMutableAttributedString()
        private static let token = try! NSRegularExpression(pattern: "\u{F0001}(\\d+)\u{F0002}")

        init(style: Style, mentions: [(String, String, String)]) {
            self.style = style
            self.mentions = mentions
        }

        func paragraph(_ context: Context, spacingBefore: CGFloat = 0, after: CGFloat? = nil,
                       hanging: CGFloat = 0) -> NSParagraphStyle {
            let p = NSMutableParagraphStyle()
            p.lineSpacing = style.lineSpacing
            p.paragraphSpacing = after ?? style.paragraphSpacing
            p.paragraphSpacingBefore = spacingBefore
            p.firstLineHeadIndent = context.indent
            p.headIndent = context.indent + hanging
            p.lineBreakMode = .byWordWrapping
            if hanging > 0 {
                p.tabStops = [NSTextTab(textAlignment: .left, location: context.indent + hanging, options: [:])]
                p.defaultTabInterval = hanging
                // A marker from a fallback font (☐) must not make its line
                // taller than the list's other lines.
                let f = style.font
                let line = ceil(f.ascender - f.descender + f.leading)
                p.minimumLineHeight = line
                p.maximumLineHeight = line
            }
            return p
        }

        func base(_ context: Context) -> [NSAttributedString.Key: Any] {
            var a: [NSAttributedString.Key: Any] = [.font: style.font, .foregroundColor: style.text]
            if context.quote {
                a[.foregroundColor] = style.secondary
                a[MarkdownRenderer.block] = Block.quote.rawValue
            }
            return a
        }

        mutating func blocks(_ children: some Sequence<Markup>, context: Context) {
            for child in children { block(child, context: context) }
        }

        mutating func block(_ markup: Markup, context: Context) {
            switch markup {
            case let p as Paragraph:
                let start = out.length
                inlines(p.children, attributes: base(context))
                out.append(NSAttributedString(string: "\n", attributes: base(context)))
                out.addAttribute(.paragraphStyle, value: paragraph(context),
                                 range: NSRange(location: start, length: out.length - start))
            case let h as Heading:
                let size: CGFloat = h.level == 1 ? 22 : h.level == 2 ? 19 : 17
                var a = base(context)
                a[.font] = PlatformFont.systemFont(ofSize: size, weight: .semibold)
                let start = out.length
                inlines(h.children, attributes: a)
                out.append(NSAttributedString(string: "\n", attributes: a))
                out.addAttribute(.paragraphStyle,
                                 value: paragraph(context, spacingBefore: start == 0 ? 0 : 8, after: 6),
                                 range: NSRange(location: start, length: out.length - start))
            case let list as UnorderedList:
                items(Array(list.listItems), ordered: nil, context: context)
            case let list as OrderedList:
                items(Array(list.listItems), ordered: Int(list.startIndex), context: context)
            case let code as Markdown.CodeBlock:
                codeBlock(code.code, context: context)
            case let quote as BlockQuote:
                var inner = context
                inner.indent += 16
                inner.quote = true
                blocks(quote.children, context: inner)
            case is ThematicBreak:
                var a = base(context)
                a[MarkdownRenderer.block] = Block.rule.rawValue
                a[.font] = PlatformFont.systemFont(ofSize: 6)
                out.append(NSAttributedString(string: "\u{00A0}\n", attributes: a))
                let p = paragraph(context, spacingBefore: 6, after: 12)
                out.addAttribute(.paragraphStyle, value: p, range: NSRange(location: out.length - 2, length: 2))
            case let table as Markdown.Table:
                tableBlock(table, context: context)
            case let html as HTMLBlock:
                codeBlock(html.rawHTML, context: context)
            default:
                // Anything else this build does not know: its text.
                blocks(markup.children, context: context)
            }
        }

        mutating func items(_ items: [ListItem], ordered: Int?, context: Context) {
            let tasks = items.contains { $0.checkbox != nil }
            let hanging: CGFloat = ordered != nil || tasks ? 28 : 20
            for (n, item) in items.enumerated() {
                let marker: String
                if let box = item.checkbox {
                    marker = box == .checked ? "☑" : "☐"
                } else if let first = ordered {
                    marker = "\(first + n)."
                } else {
                    marker = context.indent > 0 ? "◦" : "•"
                }
                var first = true
                for child in item.children {
                    if let p = child as? Paragraph {
                        let start = out.length
                        let a = base(context)
                        out.append(NSAttributedString(string: (first ? marker : "") + "\t", attributes: a))
                        inlines(p.children, attributes: a)
                        out.append(NSAttributedString(string: "\n", attributes: a))
                        out.addAttribute(.paragraphStyle,
                                         value: paragraph(context, after: 4, hanging: hanging),
                                         range: NSRange(location: start, length: out.length - start))
                        first = false
                    } else {
                        var inner = context
                        inner.indent += hanging
                        block(child, context: inner)
                    }
                }
            }
            // Space after the list as after a paragraph.
            if out.length > 0, let p = out.attribute(.paragraphStyle, at: out.length - 1, effectiveRange: nil)
                as? NSParagraphStyle, context.indent == 0 {
                let q = p.mutableCopy() as! NSMutableParagraphStyle
                q.paragraphSpacing = style.paragraphSpacing
                var range = NSRange()
                _ = out.attribute(.paragraphStyle, at: out.length - 1, effectiveRange: &range)
                out.addAttribute(.paragraphStyle, value: q, range: range)
            }
        }

        mutating func codeBlock(_ code: String, context: Context) {
            var text = code
            while text.hasSuffix("\n") { text.removeLast() }
            let start = out.length
            let a: [NSAttributedString.Key: Any] = [
                .font: style.code, .foregroundColor: style.text,
                MarkdownRenderer.block: Block.code.rawValue,
                MarkdownRenderer.codeText: text,
            ]
            out.append(NSAttributedString(string: text + "\n", attributes: a))
            let p = NSMutableParagraphStyle()
            p.lineSpacing = 2
            p.firstLineHeadIndent = context.indent + 12
            p.headIndent = context.indent + 12
            p.tailIndent = -40
            p.lineBreakMode = .byCharWrapping
            out.addAttribute(.paragraphStyle, value: p, range: NSRange(location: start, length: out.length - start))
            // Room above and below inside the tinted block, and after it.
            let lines = (out.string as NSString).paragraphRange(for: NSRange(location: start, length: 0))
            let firstStyle = p.mutableCopy() as! NSMutableParagraphStyle
            firstStyle.paragraphSpacingBefore = 10
            out.addAttribute(.paragraphStyle, value: firstStyle, range: lines)
            let last = (out.string as NSString).paragraphRange(for: NSRange(location: out.length - 1, length: 0))
            let lastStyle = (out.attribute(.paragraphStyle, at: last.location, effectiveRange: nil)
                as! NSParagraphStyle).mutableCopy() as! NSMutableParagraphStyle
            lastStyle.paragraphSpacing = 20
            out.addAttribute(.paragraphStyle, value: lastStyle, range: last)
        }

        mutating func tableBlock(_ table: Markdown.Table, context: Context) {
            var rows: [[String]] = []
            rows.append(table.head.cells.map { plain($0) })
            for row in table.body.rows { rows.append(row.cells.map { plain($0) }) }
            let alignments = table.columnAlignments.map { a -> Table.Alignment in
                switch a {
                case .center: .center
                case .right: .right
                default: .left
                }
            }
            var a = base(context)
            a[MarkdownRenderer.table] = Table(rows: rows, alignments: alignments)
            out.append(NSAttributedString(string: "\u{FFFC}", attributes: a))
            out.append(NSAttributedString(string: "\n", attributes: base(context)))
        }

        /// A table cell's text, mentions named.
        func plain(_ cell: Markdown.Table.Cell) -> String {
            unshield(cell.plainText.trimmingCharacters(in: .whitespaces))
        }

        func unshield(_ s: String) -> String {
            guard s.contains("\u{F0001}") else { return s }
            var out = s
            for m in Self.token.matches(in: s, range: NSRange(s.startIndex..., in: s)).reversed() {
                guard let r = Range(m.range, in: s), let i = Int(s[Range(m.range(at: 1), in: s)!]),
                      i < mentions.count else { continue }
                out.replaceSubrange(r, with: mentions[i].0)
            }
            return out
        }

        mutating func inlines(_ children: some Sequence<Markup>, attributes: [NSAttributedString.Key: Any]) {
            for child in children { inline(child, attributes: attributes) }
        }

        mutating func inline(_ markup: Markup, attributes a: [NSAttributedString.Key: Any]) {
            switch markup {
            case let t as Markdown.Text:
                text(t.string, attributes: a)
            case let e as Emphasis:
                inlines(e.children, attributes: traits(a, .traitItalic))
            case let s as Strong:
                inlines(s.children, attributes: traits(a, .traitBold))
            case let s as Strikethrough:
                var b = a
                b[.strikethroughStyle] = NSUnderlineStyle.single.rawValue
                inlines(s.children, attributes: b)
            case let c as InlineCode:
                var b = a
                b[.font] = style.code
                b[.backgroundColor] = style.codeBackground
                out.append(NSAttributedString(string: unshieldRaw(c.code), attributes: b))
            case let l as Markdown.Link:
                var b = a
                if let d = l.destination, let url = URL(string: d) {
                    b[.link] = url
                    b[.foregroundColor] = style.accent
                }
                inlines(l.children, attributes: b)
            case is SoftBreak:
                out.append(NSAttributedString(string: " ", attributes: a))
            case is LineBreak:
                out.append(NSAttributedString(string: "\u{2028}", attributes: a))
            case let i as Markdown.Image:
                inlines(i.children, attributes: a)
            case let h as InlineHTML:
                text(h.rawHTML, attributes: a)
            default:
                inlines(markup.children, attributes: a)
            }
        }

        private static let links = try? NSDataDetector(types: NSTextCheckingResult.CheckingType.link.rawValue)

        /// Plain text: a bare URL as a link (GFM's autolinks), its mention
        /// tokens as names in the agent's colour.
        mutating func text(_ s: String, attributes a: [NSAttributedString.Key: Any]) {
            if a[.link] == nil, s.contains("://") || s.contains("www."), let detector = Self.links {
                let ns = s as NSString
                var at = 0
                for m in detector.matches(in: s, range: NSRange(location: 0, length: ns.length)) {
                    guard let url = m.url else { continue }
                    if m.range.location > at {
                        text(ns.substring(with: NSRange(location: at, length: m.range.location - at)), attributes: a)
                    }
                    var b = a
                    b[.link] = url
                    b[.foregroundColor] = style.accent
                    out.append(NSAttributedString(string: ns.substring(with: m.range), attributes: b))
                    at = m.range.location + m.range.length
                }
                if at > 0 {
                    if at < ns.length { text(ns.substring(from: at), attributes: a) }
                    return
                }
            }
            guard s.contains("\u{F0001}") else {
                out.append(NSAttributedString(string: s, attributes: a))
                return
            }
            var at = s.startIndex
            for m in Self.token.matches(in: s, range: NSRange(s.startIndex..., in: s)) {
                guard let r = Range(m.range, in: s), let i = Int(s[Range(m.range(at: 1), in: s)!]),
                      i < mentions.count else { continue }
                out.append(NSAttributedString(string: String(s[at..<r.lowerBound]), attributes: a))
                var b = a
                b[.foregroundColor] = style.agent
                b[MarkdownRenderer.mention] = "[\(mentions[i].1)]"
                if let f = b[.font] as? PlatformFont { b[.font] = Self.with(f, .traitBold) }
                out.append(NSAttributedString(string: mentions[i].0, attributes: b))
                at = r.upperBound
            }
            out.append(NSAttributedString(string: String(s[at...]), attributes: a))
        }

        /// In code, a mention stays as written.
        func unshieldRaw(_ s: String) -> String {
            guard s.contains("\u{F0001}") else { return s }
            var out = s
            for m in Self.token.matches(in: s, range: NSRange(s.startIndex..., in: s)).reversed() {
                guard let r = Range(m.range, in: s), let i = Int(s[Range(m.range(at: 1), in: s)!]),
                      i < mentions.count else { continue }
                out.replaceSubrange(r, with: mentions[i].2)
            }
            return out
        }

        func traits(_ a: [NSAttributedString.Key: Any], _ trait: CTFontSymbolicTraits) -> [NSAttributedString.Key: Any] {
            var b = a
            if let f = a[.font] as? PlatformFont { b[.font] = Self.with(f, trait) }
            return b
        }

        /// `font` with `trait` added (Core Text: the same on either platform,
        /// whose fonts it bridges).
        static func with(_ font: PlatformFont, _ trait: CTFontSymbolicTraits) -> PlatformFont {
            let ct = font as CTFont
            let traits = CTFontGetSymbolicTraits(ct).union(trait)
            guard let out = CTFontCreateCopyWithSymbolicTraits(ct, 0, nil, traits, traits) else { return font }
            return out as PlatformFont
        }
    }
}

