import AppKit
import SwiftUI

// The one macOS-only piece of message text: the AppKit (TextKit 2) view
// that shows `MarkdownRenderer`'s attributed string, its block drawing,
// tables as attachments, and copy as plain text. An iOS app would replace
// this file with a `UITextView` equivalent behind the same `MessageText`.

/// A message's Markdown in one selectable, non-editable text view, sized
/// to its text: a drag selects across its paragraphs, lists and code, and
/// a copy is plain text with its line breaks and list markers.
struct MessageText: NSViewRepresentable {
    let text: String
    var committee: String?

    func makeNSView(context: Context) -> MessageTextView {
        let view = MessageTextView()
        view.show(MarkdownCache.shared.attributed(text, committee: committee))
        return view
    }

    func updateNSView(_ view: MessageTextView, context: Context) {
        let next = MarkdownCache.shared.attributed(text, committee: committee)
        if view.shown !== next { view.show(next) }
    }

    func sizeThatFits(_ proposal: ProposedViewSize, nsView view: MessageTextView, context: Context) -> CGSize? {
        let width = (proposal.width ?? Theme.readingWidth).rounded(.down)
        guard width > 0, width.isFinite else { return nil }
        if let h = MarkdownCache.shared.height(text, committee: committee, width: width) {
            return CGSize(width: width, height: h)
        }
        let start = ContinuousClock.now
        let h = view.height(forWidth: width)
        MarkdownCache.shared.counters.measures += 1
        MarkdownCache.shared.counters.measureTime += ContinuousClock.now - start
        MarkdownCache.shared.setHeight(h, text, committee: committee, width: width)
        return CGSize(width: width, height: h)
    }
}

/// The text view behind `MessageText`: TextKit 2, drawing code blocks on a
/// tinted ground with a copy button, block quotes with a bar, thematic
/// breaks as a line.
final class MessageTextView: NSTextView, NSTextLayoutManagerDelegate {
    private(set) var shown: NSAttributedString?
    private var copyButtons: [NSButton] = []
    /// Each code block: where it starts and ends in the text, and its code.
    private var codeBlocks: [(NSRange, String)] = []

    init() {
        let container = NSTextContainer(size: CGSize(width: Theme.readingWidth, height: .greatestFiniteMagnitude))
        container.lineFragmentPadding = 0
        container.widthTracksTextView = true
        let layout = NSTextLayoutManager()
        layout.textContainer = container
        let content = NSTextContentStorage()
        content.addTextLayoutManager(layout)
        super.init(frame: .zero, textContainer: container)
        layout.delegate = self
        isEditable = false
        isSelectable = true
        isRichText = true
        drawsBackground = false
        textContainerInset = .zero
        isVerticallyResizable = false
        isHorizontallyResizable = false
        linkTextAttributes = [.cursor: NSCursor.pointingHand]
        setAccessibilityRole(.staticText)
    }

    @available(*, unavailable) required init?(coder: NSCoder) { fatalError("not coded") }

    func show(_ text: NSAttributedString) {
        shown = text
        let shownText = Self.forAppKit(text)
        codeBlocks = []
        text.enumerateAttribute(MarkdownRenderer.codeText, in: NSRange(location: 0, length: text.length)) { v, r, _ in
            if let code = v as? String { codeBlocks.append((r, code)) }
        }
        textStorage?.setAttributedString(shownText)
        setAccessibilityValue(text.string)
        needsLayout = true
    }

    /// The renderer's string as AppKit shows it: a table value becomes a
    /// table attachment, a mention's id its name's tooltip.
    private static func forAppKit(_ text: NSAttributedString) -> NSAttributedString {
        let whole = NSRange(location: 0, length: text.length)
        var tables: [(NSRange, MarkdownRenderer.Table)] = []
        text.enumerateAttribute(MarkdownRenderer.table, in: whole) { v, r, _ in
            if let t = v as? MarkdownRenderer.Table { tables.append((r, t)) }
        }
        var mentions: [(NSRange, String)] = []
        text.enumerateAttribute(MarkdownRenderer.mention, in: whole) { v, r, _ in
            if let id = v as? String { mentions.append((r, id)) }
        }
        guard !tables.isEmpty || !mentions.isEmpty else { return text }
        let out = NSMutableAttributedString(attributedString: text)
        for (r, id) in mentions { out.addAttribute(.toolTip, value: id, range: r) }
        for (r, t) in tables { out.addAttribute(.attachment, value: TableAttachment(t), range: r) }
        return out
    }

    /// The height the text takes at `width`.
    func height(forWidth width: CGFloat) -> CGFloat {
        guard let layout = textLayoutManager, let container = textContainer else { return 0 }
        if frame.width != width { setFrameSize(CGSize(width: width, height: frame.height)) }
        container.size = CGSize(width: width, height: .greatestFiniteMagnitude)
        layout.ensureLayout(for: layout.documentRange)
        return ceil(layout.usageBoundsForTextContainer.height)
    }

    override func layout() {
        super.layout()
        if !codeBlocks.isEmpty || !copyButtons.isEmpty { placeCopyButtons() }
    }

    // MARK: Drawing the blocks

    func textLayoutManager(_ manager: NSTextLayoutManager, textLayoutFragmentFor location: any NSTextLocation,
                           in element: NSTextElement) -> NSTextLayoutFragment {
        if let paragraph = element as? NSTextParagraph,
           paragraph.attributedString.length > 0,
           let kind = paragraph.attributedString.attribute(MarkdownRenderer.block, at: 0, effectiveRange: nil) as? String,
           let block = MarkdownRenderer.Block(rawValue: kind) {
            // The colours for the appearance in force now: a change of
            // appearance lays the text out again.
            var tint = CGColor(gray: 0.5, alpha: 0.15), line = CGColor(gray: 0.5, alpha: 0.5)
            effectiveAppearance.performAsCurrentDrawingAppearance {
                tint = NSColor(Theme.containerHigh).cgColor
                line = NSColor(Theme.outline).cgColor
            }
            return BlockFragment(textElement: element, range: element.elementRange, block: block,
                                 tint: tint, line: line)
        }
        return NSTextLayoutFragment(textElement: element, range: element.elementRange)
    }

    // MARK: Copy buttons over code blocks

    /// One copy button at the top right of each code block, from that
    /// block's first line fragment (one lookup a block, not a walk of the
    /// text).
    private func placeCopyButtons() {
        guard let layout = textLayoutManager, let content = layout.textContentManager else { return }
        while copyButtons.count < codeBlocks.count {
            let b = NSButton(image: NSImage(systemSymbolName: "doc.on.doc", accessibilityDescription: L("message.copy_code"))!,
                             target: self, action: #selector(copyCode(_:)))
            b.isBordered = false
            b.contentTintColor = NSColor(Theme.onSurfaceVariant)
            b.toolTip = L("message.copy_code")
            addSubview(b)
            copyButtons.append(b)
        }
        for (i, b) in copyButtons.enumerated() {
            guard i < codeBlocks.count,
                  let start = content.location(content.documentRange.location, offsetBy: codeBlocks[i].0.location),
                  let fragment = layout.textLayoutFragment(for: start)
            else { b.isHidden = true; continue }
            b.isHidden = false
            b.identifier = NSUserInterfaceItemIdentifier(codeBlocks[i].1)
            let top = fragment.layoutFragmentFrame.minY + (fragment.textLineFragments.first?.typographicBounds.minY ?? 0)
            b.frame = CGRect(x: bounds.width - 30, y: top - 4, width: 22, height: 22)
        }
    }

    @objc private func copyCode(_ sender: NSButton) {
        guard let code = sender.identifier?.rawValue else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(code, forType: .string)
    }

    // MARK: Copy as plain text

    /// A selection copies as plain text: line breaks as `\n`, list markers
    /// and code as they read, a table as tab-separated rows.
    override func writeSelection(to pboard: NSPasteboard, types: [NSPasteboard.PasteboardType]) -> Bool {
        guard let storage = textStorage else { return false }
        var text = ""
        for value in selectedRanges {
            let range = value.rangeValue
            guard range.length > 0 else { continue }
            storage.enumerateAttribute(.attachment, in: range) { attachment, sub, _ in
                if let table = attachment as? TableAttachment {
                    text += table.plainText + "\n"
                } else {
                    text += (storage.string as NSString).substring(with: sub)
                }
            }
        }
        text = text.replacingOccurrences(of: "\u{2028}", with: "\n")
            .replacingOccurrences(of: "\u{00A0}\n", with: "\n")
        pboard.clearContents()
        return pboard.setString(text, forType: .string)
    }
}

/// One paragraph of a code block, a block quote or a thematic break: its
/// ground (a tint, a bar, a line), then its text.
nonisolated private final class BlockFragment: NSTextLayoutFragment, @unchecked Sendable {
    let block: MarkdownRenderer.Block
    let tint: CGColor
    let line: CGColor

    init(textElement: NSTextElement, range: NSTextRange?, block: MarkdownRenderer.Block,
         tint: CGColor, line: CGColor) {
        self.block = block
        self.tint = tint
        self.line = line
        super.init(textElement: textElement, range: range)
    }

    @available(*, unavailable) required init?(coder: NSCoder) { fatalError("not coded") }

    override var renderingSurfaceBounds: CGRect {
        super.renderingSurfaceBounds.union(groundRect.insetBy(dx: 0, dy: -8))
    }

    /// The block's ground, in the fragment's own coordinates: the whole
    /// width of the text, the paragraph's spacing included.
    private var groundRect: CGRect {
        let width = textLayoutManager?.textContainer?.size.width ?? layoutFragmentFrame.width
        let lines = textLineFragments.reduce(CGRect.null) { $0.union($1.typographicBounds) }
        return CGRect(x: -layoutFragmentFrame.minX, y: lines.minY - 2, width: width, height: lines.height + 4)
    }

    override func draw(at point: CGPoint, in context: CGContext) {
        let ground = groundRect.offsetBy(dx: point.x, dy: point.y)
        switch block {
        case .code:
            // The block's first paragraph has space before it, its last
            // space after: the tint reaches into both, rounded there.
            let style = textLineFragments.first?.attributedString.attribute(
                .paragraphStyle, at: 0, effectiveRange: nil) as? NSParagraphStyle
            let first = (style?.paragraphSpacingBefore ?? 0) > 0
            let last = (style?.paragraphSpacing ?? 0) >= 20
            var rect = ground.insetBy(dx: 0, dy: -1)
            if first { rect.origin.y -= 6; rect.size.height += 6 }
            if last { rect.size.height += 6 }
            context.setFillColor(tint)
            if first || last {
                let r: CGFloat = 8
                let path = CGMutablePath()
                path.addRoundedRect(in: rect, cornerWidth: r, cornerHeight: r)
                // Square the joins with the paragraphs between.
                if !first { path.addRect(CGRect(x: rect.minX, y: rect.minY, width: rect.width, height: r)) }
                if !last { path.addRect(CGRect(x: rect.minX, y: rect.maxY - r, width: rect.width, height: r)) }
                context.addPath(path)
                context.fillPath()
            } else {
                context.fill(rect)
            }
        case .quote:
            context.setFillColor(line)
            let indent = (textLineFragments.first?.attributedString.attribute(
                .paragraphStyle, at: 0, effectiveRange: nil) as? NSParagraphStyle)?.headIndent ?? 16
            context.fill(CGRect(x: ground.minX + indent - 14, y: ground.minY, width: 3, height: ground.height))
        case .rule:
            context.setFillColor(line)
            context.fill(CGRect(x: ground.minX, y: ground.midY, width: ground.width, height: 1))
            return
        }
        super.draw(at: point, in: context)
    }
}

// MARK: Tables

/// A table as an attachment: drawn by a view that scrolls sideways when
/// the table is wider than the column; copied as tab-separated rows.
nonisolated final class TableAttachment: NSTextAttachment, @unchecked Sendable {
    typealias Alignment = MarkdownRenderer.Table.Alignment
    let table: MarkdownRenderer.Table
    var rows: [[String]] { table.rows }
    var alignments: [Alignment] { table.alignments }

    init(_ table: MarkdownRenderer.Table) {
        self.table = table
        super.init(data: nil, ofType: nil)
        allowsTextAttachmentView = true
    }

    required init?(coder: NSCoder) { fatalError("not coded") }

    /// What a copy of the table gives: one line per row, cells separated
    /// by tabs.
    var plainText: String { table.plainText }

    override func viewProvider(for parentView: NSView?, location: any NSTextLocation,
                               textContainer: NSTextContainer?) -> NSTextAttachmentViewProvider? {
        let provider = TableViewProvider(textAttachment: self, parentView: parentView,
                                         textLayoutManager: textContainer?.textLayoutManager, location: location)
        provider.tracksTextAttachmentViewBounds = true
        return provider
    }
}

nonisolated final class TableViewProvider: NSTextAttachmentViewProvider, @unchecked Sendable {
    override func loadView() {
        guard let table = textAttachment as? TableAttachment else { return }
        let rows = table.rows, alignments = table.alignments
        // Text views lay out on the main thread.
        view = MainActor.assumeIsolated {
            NSHostingView(rootView: TableView(rows: rows, alignments: alignments))
        }
    }

    override func attachmentBounds(for attributes: [NSAttributedString.Key: Any], location: any NSTextLocation,
                                   textContainer: NSTextContainer?, proposedLineFragment: CGRect,
                                   position: CGPoint) -> CGRect {
        guard let table = textAttachment as? TableAttachment else { return .zero }
        let height = CGFloat(table.rows.count) * 30 + 2
        return CGRect(x: 0, y: -6, width: proposedLineFragment.width, height: height + 12)
    }
}

/// The table itself: a header row, aligned columns, tinted lines between
/// rows; wider than the column, it scrolls sideways.
private struct TableView: View {
    let rows: [[String]]
    let alignments: [TableAttachment.Alignment]

    var body: some View {
        ScrollView(.horizontal) {
            Grid(alignment: .leading, horizontalSpacing: 0, verticalSpacing: 0) {
                ForEach(Array(rows.enumerated()), id: \.offset) { r, row in
                    GridRow {
                        ForEach(Array(row.enumerated()), id: \.offset) { c, cell in
                            Text(verbatim: cell)
                                .font(r == 0 ? Theme.Fonts.text.weight(.semibold) : Theme.Fonts.text)
                                .foregroundStyle(Theme.onSurface)
                                .lineLimit(1)
                                .fixedSize()
                                .frame(minWidth: 60, maxWidth: .infinity, alignment: alignment(c))
                                .gridColumnAlignment(horizontal(c))
                                .padding(.horizontal, 10)
                                .frame(height: 30)
                                .background(r == 0 ? Theme.containerHigh : (r % 2 == 0 ? Theme.container : .clear))
                        }
                    }
                }
            }
            .clipShape(RoundedRectangle(cornerRadius: 8))
            .overlay { RoundedRectangle(cornerRadius: 8).stroke(Theme.outline, lineWidth: 0.5) }
            .padding(.vertical, 1)
        }
        .scrollIndicators(.automatic)
    }

    /// The column's alignment as the Markdown declares it (`:-:` centre,
    /// `--:` right), header and cells alike; left by default.
    private func horizontal(_ c: Int) -> HorizontalAlignment {
        switch c < alignments.count ? alignments[c] : .left {
        case .left: .leading
        case .center: .center
        case .right: .trailing
        }
    }

    private func alignment(_ c: Int) -> Alignment {
        switch c < alignments.count ? alignments[c] : .left {
        case .left: .leading
        case .center: .center
        case .right: .trailing
        }
    }
}
