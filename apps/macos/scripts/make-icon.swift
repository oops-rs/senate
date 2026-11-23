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

/// The mark alone, on a transparent 1024 canvas (the canvas is the tile):
/// drawn on a 100-unit grid, y up, then scaled to 76 % and centred, which
/// leaves it the margin system icons keep inside the tile.
func mark(_ look: Look, _ px: Int = 1024) -> CGImage {
    let s = CGFloat(px)
    let ctx = CGContext(data: nil, width: px, height: px, bitsPerComponent: 8, bytesPerRow: 0,
                        space: space, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    let k: CGFloat = 0.76
    let u = s * k / 100
    // The mark spans y 17…71.75 on its grid: centre that on the canvas.
    let dx = s * (1 - k) / 2, dy = s / 2 - 44.4 * u
    func P(_ x: CGFloat, _ y: CGFloat) -> CGPoint { CGPoint(x: dx + x * u, y: dy + y * u) }
    let centre = P(50, 27)
    ctx.setLineCap(.butt)
    ctx.setLineWidth(7.5 * u)
    // Three rows of seats around the floor, the same gap between seats in
    // every row.
    for (row, (radius, seats)) in [(19.0, 4), (30.0, 6), (41.0, 8)].enumerated() {
        ctx.setStrokeColor(look.seats.cg(look.fade[row]))
        let gap = 0.16 / (radius / 19)
        let span = Double.pi / Double(seats)
        for i in 0..<seats {
            let a0 = Double(i) * span + gap / 2, a1 = Double(i + 1) * span - gap / 2
            ctx.addArc(center: centre, radius: CGFloat(radius) * u,
                       startAngle: CGFloat(a0), endAngle: CGFloat(a1), clockwise: false)
            ctx.strokePath()
        }
    }
    // The Speaker, at the centre of the floor.
    ctx.setFillColor(look.speaker.cg())
    ctx.fillEllipse(in: CGRect(x: centre.x - 6 * u, y: centre.y - 6 * u, width: 12 * u, height: 12 * u))
    // The floor's edge.
    ctx.setFillColor(look.seats.cg(look.fade[1]))
    let floor = CGRect(origin: P(14, 17), size: CGSize(width: 72 * u, height: 3.5 * u))
    ctx.addPath(CGPath(roundedRect: floor, cornerWidth: 1.75 * u, cornerHeight: 1.75 * u, transform: nil))
    ctx.fillPath()
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
