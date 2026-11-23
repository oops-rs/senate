#!/usr/bin/env swift
// Draws the Senate app icon, the chamber: a hemicycle of seats seen from
// above, three rows fading outward, the Speaker's seat lit at the centre of
// the floor. It follows the system's appearance: ink-navy seats and a
// vermilion Speaker on paper in light, white seats and an amber Speaker on
// ink navy in dark.
//
// It writes an Icon Composer document, Senate/AppIcon.icon: the mark as one
// transparent layer per appearance (Assets/) and icon.json, which sets each
// appearance's ground and the layer to use; the system draws the macOS
// tile, its shape and its shadow. Plain Core Graphics: no symbol font or
// bitmap goes into the mark. Also writes docs/screenshots/icon.png (both
// appearances side by side) when `ictool` from Icon Composer is installed.
//
// Usage, from apps/macos/: scripts/make-icon.swift
import AppKit
import Foundation

let here = URL(fileURLWithPath: CommandLine.arguments[0]).deletingLastPathComponent()
let app = here.deletingLastPathComponent()
let doc = app.appendingPathComponent("Senate/AppIcon.icon")
let assets = doc.appendingPathComponent("Assets")
try? FileManager.default.removeItem(at: doc)
try FileManager.default.createDirectory(at: assets, withIntermediateDirectories: true)
let space = CGColorSpace(name: CGColorSpace.sRGB)!

struct RGB {
    let r, g, b: CGFloat
    init(_ hex: UInt32) {
        r = CGFloat((hex >> 16) & 0xFF) / 255
        g = CGFloat((hex >> 8) & 0xFF) / 255
        b = CGFloat(hex & 0xFF) / 255
    }
    func cg(_ a: CGFloat = 1) -> CGColor { CGColor(red: r, green: g, blue: b, alpha: a) }
    /// As icon.json writes a colour.
    var json: String { String(format: "srgb:%.5f,%.5f,%.5f,1.00000", r, g, b) }
}

/// One appearance: the ground's gradient, the seats (rows fading outward
/// by `fade`), the Speaker's seat.
struct Look {
    let top, bottom, seats, speaker: RGB
    let fade: [CGFloat]
}
let light = Look(top: RGB(0xFCFBF7), bottom: RGB(0xEAE6DC), seats: RGB(0x1C2A55), speaker: RGB(0xE4572E),
                 fade: [1, 0.74, 0.52])
let dark = Look(top: RGB(0x1E2A4A), bottom: RGB(0x0B1124), seats: RGB(0xFFFFFF), speaker: RGB(0xFFB547),
                fade: [1, 0.82, 0.64])

/// The mark's shapes, once, on a 100-unit grid (y up): three rows of seats
/// around the floor (the same gap between seats in every row), the
/// Speaker's seat at the centre of the floor, and the floor's edge. Every
/// picture of the mark (the icon's layers, the sidebar's and menu bar's
/// assets, the SVG) fills these paths.
enum Mark {
    static let centre = CGPoint(x: 50, y: 27)
    static let rows: [CGPath] = [(19.0, 4), (30.0, 6), (41.0, 8)].map { radius, seats in
        let path = CGMutablePath()
        let gap = 0.16 / (radius / 19)
        let span = Double.pi / Double(seats)
        for i in 0..<seats {
            let a0 = Double(i) * span + gap / 2, a1 = Double(i + 1) * span - gap / 2
            let arc = CGMutablePath()
            arc.addArc(center: centre, radius: CGFloat(radius), startAngle: CGFloat(a0),
                       endAngle: CGFloat(a1), clockwise: false)
            path.addPath(arc.copy(strokingWithWidth: 7.5, lineCap: .butt, lineJoin: .miter, miterLimit: 10))
        }
        return path
    }
    static let speaker = CGPath(ellipseIn: CGRect(x: centre.x - 6, y: centre.y - 6, width: 12, height: 12),
                                transform: nil)
    static let floor = CGPath(roundedRect: CGRect(x: 14, y: 17, width: 72, height: 3.5),
                              cornerWidth: 1.75, cornerHeight: 1.75, transform: nil)
    /// Where the mark lies on its grid.
    static let bounds = CGRect(x: 5.25, y: 17, width: 89.5, height: 54.75)

    /// Fill the mark into `ctx`, already transformed to the grid.
    static func fill(_ ctx: CGContext, seats: CGColor, speaker sp: CGColor?, fade: [CGFloat]) {
        for (row, path) in rows.enumerated() {
            ctx.addPath(path)
            ctx.setFillColor(seats.copy(alpha: fade[row])!)
            ctx.fillPath()
        }
        ctx.addPath(floor)
        ctx.setFillColor(seats.copy(alpha: fade[1])!)
        ctx.fillPath()
        if let sp {
            ctx.addPath(speaker)
            ctx.setFillColor(sp)
            ctx.fillPath()
        }
    }
}

/// The mark alone, on a transparent 1024 canvas (the canvas is the tile),
/// scaled to 76 % and centred: the margin system icons keep in the tile.
func mark(_ look: Look, _ px: Int = 1024) -> CGImage {
    let s = CGFloat(px)
    let ctx = CGContext(data: nil, width: px, height: px, bitsPerComponent: 8, bytesPerRow: 0,
                        space: space, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    let u = s * 0.76 / 100
    ctx.translateBy(x: s * 0.12, y: s / 2 - Mark.bounds.midY * u)
    ctx.scaleBy(x: u, y: u)
    Mark.fill(ctx, seats: look.seats.cg(), speaker: look.speaker.cg(), fade: look.fade)
    return ctx.makeImage()!
}

func png(_ image: CGImage, _ url: URL) throws {
    try NSBitmapImageRep(cgImage: image).representation(using: .png, properties: [:])!.write(to: url)
}

try png(mark(light), assets.appendingPathComponent("chamber-light.png"))
try png(mark(dark), assets.appendingPathComponent("chamber-dark.png"))

func gradient(_ look: Look) -> [String: Any] {
    ["linear-gradient": [look.top.json, look.bottom.json], "orientation": ["start": ["x": 0.5, "y": 0], "stop": ["x": 0.5, "y": 0.7]]]
}
let icon: [String: Any] = [
    "fill-specializations": [
        ["value": gradient(light)],
        ["appearance": "dark", "value": gradient(dark)],
    ],
    "groups": [[
        "layers": [[
            "name": "chamber",
            // No plain `image-name`: with one, Icon Composer's renderer
            // ignores the dark specialization.
            "image-name-specializations": [
                ["value": "chamber-light.png"],
                ["appearance": "dark", "value": "chamber-dark.png"],
            ],
            "glass": false,
        ]],
        "shadow": ["kind": "neutral", "opacity": 0.5],
        "translucency": ["enabled": false, "value": 0.5],
        "specular": false,
    ]],
    "supported-platforms": ["squares": ["macOS"]],
]
try JSONSerialization.data(withJSONObject: icon, options: [.prettyPrinted, .sortedKeys])
    .write(to: doc.appendingPathComponent("icon.json"))
print("make-icon: wrote \(doc.path)")

// The mark without its tile, as vectors: for the sidebar beside "Senate"
// (`SenateMark`: the seats, a template in the text's colour with the rows
// fading; `SenateMarkSpeaker`: the Speaker's seat in amber, drawn over it),
// for the menu bar (`SenateGlyph`: one colour, every row whole), and as the
// logo's source, docs/logo/senate-mark.svg.
let catalog = app.appendingPathComponent("Senate/Assets.xcassets")
func pdfAsset(_ name: String, height: CGFloat, template: Bool, _ draw: (CGContext) -> Void) throws {
    let set = catalog.appendingPathComponent("\(name).imageset")
    try? FileManager.default.removeItem(at: set)
    try FileManager.default.createDirectory(at: set, withIntermediateDirectories: true)
    let k = height / Mark.bounds.height
    var box = CGRect(x: 0, y: 0, width: Mark.bounds.width * k, height: height)
    let url = set.appendingPathComponent("\(name).pdf")
    let ctx = CGContext(url as CFURL, mediaBox: &box, nil)!
    ctx.beginPDFPage(nil)
    ctx.scaleBy(x: k, y: k)
    ctx.translateBy(x: -Mark.bounds.minX, y: -Mark.bounds.minY)
    draw(ctx)
    ctx.endPDFPage()
    ctx.closePDF()
    let contents: [String: Any] = [
        "images": [["idiom": "universal", "filename": "\(name).pdf"]],
        "info": ["author": "xcode", "version": 1],
        "properties": ["template-rendering-intent": template ? "template" : "original",
                       "preserves-vector-representation": true],
    ]
    try JSONSerialization.data(withJSONObject: contents, options: [.prettyPrinted, .sortedKeys])
        .write(to: set.appendingPathComponent("Contents.json"))
}
let black = CGColor(gray: 0, alpha: 1)
try pdfAsset("SenateMark", height: 13, template: true) { Mark.fill($0, seats: black, speaker: nil, fade: dark.fade) }
try pdfAsset("SenateMarkSpeaker", height: 13, template: false) {
    $0.addPath(Mark.speaker)
    $0.setFillColor(dark.speaker.cg())
    $0.fillPath()
}
try pdfAsset("SenateGlyph", height: 16, template: true) {
    Mark.fill($0, seats: black, speaker: black, fade: [1, 1, 1])
}

/// A path as SVG path data, in the mark's box with y down.
func svgPath(_ path: CGPath) -> String {
    var d = ""
    func f(_ p: CGPoint) -> String {
        String(format: "%.3f %.3f", p.x - Mark.bounds.minX, Mark.bounds.maxY - p.y)
    }
    path.applyWithBlock { e in
        let pts = e.pointee.points
        switch e.pointee.type {
        case .moveToPoint: d += "M\(f(pts[0]))"
        case .addLineToPoint: d += "L\(f(pts[0]))"
        case .addQuadCurveToPoint: d += "Q\(f(pts[0])) \(f(pts[1]))"
        case .addCurveToPoint: d += "C\(f(pts[0])) \(f(pts[1])) \(f(pts[2]))"
        case .closeSubpath: d += "Z"
        @unknown default: break
        }
    }
    return d
}
var svg = String(format: "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 %.2f %.2f\">\n",
                 Mark.bounds.width, Mark.bounds.height)
svg += "  <title>Senate</title>\n"
for (row, path) in Mark.rows.enumerated() {
    svg += "  <path fill=\"#1C2A55\" fill-opacity=\"\(light.fade[row])\" d=\"\(svgPath(path))\"/>\n"
}
svg += "  <path fill=\"#1C2A55\" fill-opacity=\"\(light.fade[1])\" d=\"\(svgPath(Mark.floor))\"/>\n"
svg += "  <path fill=\"#E4572E\" d=\"\(svgPath(Mark.speaker))\"/>\n</svg>\n"
let logo = app.appendingPathComponent("docs/logo")
try FileManager.default.createDirectory(at: logo, withIntermediateDirectories: true)
try svg.write(to: logo.appendingPathComponent("senate-mark.svg"), atomically: true, encoding: .utf8)
print("make-icon: wrote SenateMark, SenateMarkSpeaker, SenateGlyph and docs/logo/senate-mark.svg")

// docs/screenshots/icon.png: the icon as macOS renders it, light and dark.
let ictool = "/Applications/Xcode.app/Contents/Applications/Icon Composer.app/Contents/Executables/ictool"
guard FileManager.default.isExecutableFile(atPath: ictool) else {
    print("make-icon: no ictool (Icon Composer); docs/screenshots/icon.png left as it is")
    exit(0)
}
let tmp = FileManager.default.temporaryDirectory.appendingPathComponent("senate-icon-\(getpid())")
try FileManager.default.createDirectory(at: tmp, withIntermediateDirectories: true)
var rendered: [CGImage] = []
for rendition in ["Default", "Dark"] {
    let file = tmp.appendingPathComponent("\(rendition).png")
    let p = Process()
    p.executableURL = URL(fileURLWithPath: ictool)
    p.arguments = [doc.path, "--export-image", "--output-file", file.path, "--platform", "macOS",
                   "--rendition", rendition, "--width", "512", "--height", "512", "--scale", "1"]
    try p.run()
    p.waitUntilExit()
    guard p.terminationStatus == 0, let image = NSImage(contentsOf: file)?
        .cgImage(forProposedRect: nil, context: nil, hints: nil) else {
        print("make-icon: ictool could not render \(rendition)")
        exit(1)
    }
    rendered.append(image)
}
let W = 1104, H = 552
let sheet = CGContext(data: nil, width: W, height: H, bitsPerComponent: 8, bytesPerRow: 0,
                      space: space, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
sheet.setFillColor(RGB(0xF1F1F0).cg())
sheet.fill(CGRect(x: 0, y: 0, width: W / 2, height: H))
sheet.setFillColor(RGB(0x1C1C1E).cg())
sheet.fill(CGRect(x: W / 2, y: 0, width: W / 2, height: H))
for (i, image) in rendered.enumerated() {
    sheet.draw(image, in: CGRect(x: 20 + i * W / 2, y: 20, width: 512, height: 512))
}
try png(sheet.makeImage()!, app.appendingPathComponent("docs/screenshots/icon.png"))
try? FileManager.default.removeItem(at: tmp)
print("make-icon: wrote docs/screenshots/icon.png")
