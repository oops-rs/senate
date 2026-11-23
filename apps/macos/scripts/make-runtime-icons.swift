#!/usr/bin/env swift
// Draws the sidebar's runtime glyphs into Senate/Assets.xcassets, as
// single-colour vector PDFs the app renders as templates (they take the
// row's text colour). Our own glyphs, not the vendors' logos: one bold
// letter in a frame of its own shape for each runtime Senate knows by name
// (C in a square for Claude Code, X in a circle for Codex, A cut out of a
// filled square for Antigravity), so adjacent rows differ at a glance, and
// a terminal prompt for ACP and anything else. See
// Senate/Assets.xcassets/RUNTIME-ICONS.md for why.
//
// Usage, from apps/macos/: scripts/make-runtime-icons.swift
import AppKit
import CoreText
import Foundation

let here = URL(fileURLWithPath: CommandLine.arguments[0]).deletingLastPathComponent()
let catalog = here.deletingLastPathComponent().appendingPathComponent("Senate/Assets.xcassets")

/// The glyph's box, in points: the row's text is 13 pt.
let side: CGFloat = 16

/// `letters` as an outline path (no font is embedded), centred in the box.
func letterPath(_ letters: String, size: CGFloat = 9.5) -> CGPath {
    let font = CTFontCreateWithName("SF Pro Text Bold" as CFString, size, nil)
    let line = CTLineCreateWithAttributedString(
        NSAttributedString(string: letters, attributes: [.font: font, .kern: -0.1])
    )
    let bounds = CTLineGetBoundsWithOptions(line, .useGlyphPathBounds)
    let origin = CGPoint(x: (side - bounds.width) / 2 - bounds.minX, y: (side - bounds.height) / 2 - bounds.minY)
    let path = CGMutablePath()
    for run in CTLineGetGlyphRuns(line) as! [CTRun] {
        let count = CTRunGetGlyphCount(run)
        var glyphs = [CGGlyph](repeating: 0, count: count)
        var positions = [CGPoint](repeating: .zero, count: count)
        CTRunGetGlyphs(run, CFRange(location: 0, length: count), &glyphs)
        CTRunGetPositions(run, CFRange(location: 0, length: count), &positions)
        let runFont = (CTRunGetAttributes(run) as NSDictionary)[kCTFontAttributeName] as! CTFont
        for (glyph, at) in zip(glyphs, positions) {
            guard let outline = CTFontCreatePathForGlyph(runFont, glyph, nil) else { continue }
            path.addPath(outline, transform: CGAffineTransform(translationX: origin.x + at.x, y: origin.y + at.y))
        }
    }
    return path
}

let inner = CGRect(x: 0.75, y: 0.75, width: side - 1.5, height: side - 1.5)

/// A letter in an outlined frame.
func outlined(_ ctx: CGContext, _ letter: String, _ frame: CGPath) {
    ctx.addPath(frame)
    ctx.setLineWidth(1.3)
    ctx.strokePath()
    ctx.addPath(letterPath(letter))
    ctx.fillPath()
}

/// A letter cut out of a filled frame.
func knockedOut(_ ctx: CGContext, _ letter: String, _ frame: CGPath) {
    let path = CGMutablePath()
    path.addPath(frame)
    path.addPath(letterPath(letter))
    ctx.addPath(path)
    ctx.fillPath(using: .evenOdd)
}

let square = CGPath(roundedRect: inner, cornerWidth: 3.5, cornerHeight: 3.5, transform: nil)
let circle = CGPath(ellipseIn: inner, transform: nil)
let solid = CGPath(roundedRect: CGRect(x: 0, y: 0, width: side, height: side),
                   cornerWidth: 4, cornerHeight: 4, transform: nil)

/// A prompt in a square: `>_`.
func terminal(_ ctx: CGContext) {
    ctx.addPath(square)
    ctx.setLineWidth(1.3)
    ctx.strokePath()
    ctx.setLineWidth(1.4)
    ctx.setLineCap(.round)
    ctx.setLineJoin(.round)
    ctx.move(to: CGPoint(x: 4.3, y: 10.6))
    ctx.addLine(to: CGPoint(x: 7.0, y: 8.0))
    ctx.addLine(to: CGPoint(x: 4.3, y: 5.4))
    ctx.strokePath()
    ctx.move(to: CGPoint(x: 8.6, y: 5.2))
    ctx.addLine(to: CGPoint(x: 11.8, y: 5.2))
    ctx.strokePath()
}

func write(_ name: String, _ draw: (CGContext) -> Void) throws {
    let set = catalog.appendingPathComponent("\(name).imageset")
    try FileManager.default.createDirectory(at: set, withIntermediateDirectories: true)
    let url = set.appendingPathComponent("\(name).pdf")
    var box = CGRect(x: 0, y: 0, width: side, height: side)
    let ctx = CGContext(url as CFURL, mediaBox: &box, nil)!
    ctx.beginPDFPage(nil)
    ctx.setFillColor(.black)
    ctx.setStrokeColor(.black)
    draw(ctx)
    ctx.endPDFPage()
    ctx.closePDF()
    let contents: [String: Any] = [
        "images": [["idiom": "universal", "filename": "\(name).pdf"]],
        "info": ["author": "xcode", "version": 1],
        "properties": ["template-rendering-intent": "template", "preserves-vector-representation": true],
    ]
    try JSONSerialization.data(withJSONObject: contents, options: [.prettyPrinted, .sortedKeys])
        .write(to: set.appendingPathComponent("Contents.json"))
}

try write("RuntimeClaude") { outlined($0, "C", square) }
try write("RuntimeCodex") { outlined($0, "X", circle) }
try write("RuntimeAgy") { knockedOut($0, "A", solid) }
try write("RuntimeTerminal") { terminal($0) }
print("make-runtime-icons: wrote \(catalog.path)")
