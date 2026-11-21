#!/usr/bin/env swift
// Draws the Senate app icon (a columned hall on a deep-blue tile) into
// Senate/Assets.xcassets/AppIcon.appiconset, every size macOS asks for.
// Plain Core Graphics paths: no symbol font or bitmap goes into the icon.
//
// Usage, from apps/macos/: scripts/make-icon.swift
import AppKit
import Foundation

let here = URL(fileURLWithPath: CommandLine.arguments[0]).deletingLastPathComponent()
let set = here.deletingLastPathComponent()
    .appendingPathComponent("Senate/Assets.xcassets/AppIcon.appiconset")
try FileManager.default.createDirectory(at: set, withIntermediateDirectories: true)

func draw(_ px: Int) -> Data {
    let s = CGFloat(px)
    let space = CGColorSpace(name: CGColorSpace.sRGB)!
    let ctx = CGContext(
        data: nil, width: px, height: px, bitsPerComponent: 8, bytesPerRow: 0,
        space: space, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
    )!
    // The macOS icon grid: an 824/1024 tile with a 185/1024 corner radius.
    let inset = s * 100 / 1024
    let tile = CGRect(x: inset, y: inset, width: s - 2 * inset, height: s - 2 * inset)
    let radius = s * 185 / 1024
    let path = CGPath(roundedRect: tile, cornerWidth: radius, cornerHeight: radius, transform: nil)

    // A soft shadow under the tile.
    ctx.saveGState()
    ctx.setShadow(offset: CGSize(width: 0, height: -s * 10 / 1024), blur: s * 24 / 1024,
                  color: CGColor(gray: 0, alpha: 0.35))
    ctx.addPath(path)
    ctx.setFillColor(CGColor(red: 0.10, green: 0.16, blue: 0.36, alpha: 1))
    ctx.fillPath()
    ctx.restoreGState()

    // Deep blue, lighter at the top.
    ctx.saveGState()
    ctx.addPath(path)
    ctx.clip()
    let gradient = CGGradient(colorsSpace: space, colors: [
        CGColor(red: 0.27, green: 0.40, blue: 0.78, alpha: 1),
        CGColor(red: 0.09, green: 0.14, blue: 0.34, alpha: 1),
    ] as CFArray, locations: [0, 1])!
    ctx.drawLinearGradient(gradient, start: CGPoint(x: 0, y: tile.maxY),
                           end: CGPoint(x: 0, y: tile.minY), options: [])
    ctx.restoreGState()

    // The hall, in warm white: pediment, frieze, four columns, steps.
    let ink = CGColor(red: 0.98, green: 0.95, blue: 0.88, alpha: 1)
    ctx.setFillColor(ink)
    let u = tile.width / 100
    let x0 = tile.minX, y0 = tile.minY
    func rect(_ x: CGFloat, _ y: CGFloat, _ w: CGFloat, _ h: CGFloat) {
        ctx.fill(CGRect(x: x0 + x * u, y: y0 + y * u, width: w * u, height: h * u))
    }
    // Pediment.
    ctx.move(to: CGPoint(x: x0 + 18 * u, y: y0 + 66 * u))
    ctx.addLine(to: CGPoint(x: x0 + 50 * u, y: y0 + 84 * u))
    ctx.addLine(to: CGPoint(x: x0 + 82 * u, y: y0 + 66 * u))
    ctx.closePath()
    ctx.fillPath()
    // Frieze.
    rect(20, 60, 60, 4.5)
    // Columns.
    for x in [25.0, 39.0, 53.0, 67.0] {
        rect(x, 30, 8, 28)
        rect(x - 1.5, 56, 11, 2.5)   // capital
        rect(x - 1.5, 28, 11, 2.5)   // base
    }
    // Steps.
    rect(19, 22, 62, 4.5)
    rect(15, 16, 70, 4.5)

    let image = ctx.makeImage()!
    return NSBitmapImageRep(cgImage: image).representation(using: .png, properties: [:])!
}

var images: [[String: String]] = []
for points in [16, 32, 128, 256, 512] {
    for scale in [1, 2] {
        let px = points * scale
        let name = "icon_\(points)x\(points)\(scale == 2 ? "@2x" : "").png"
        try draw(px).write(to: set.appendingPathComponent(name))
        images.append([
            "idiom": "mac", "size": "\(points)x\(points)", "scale": "\(scale)x", "filename": name,
        ])
    }
}
let contents: [String: Any] = ["images": images, "info": ["author": "xcode", "version": 1]]
try JSONSerialization.data(withJSONObject: contents, options: [.prettyPrinted, .sortedKeys])
    .write(to: set.appendingPathComponent("Contents.json"))
let catalog = set.deletingLastPathComponent().appendingPathComponent("Contents.json")
try #"{ "info" : { "author" : "xcode", "version" : 1 } }"#.write(to: catalog, atomically: true, encoding: .utf8)
print("make-icon: wrote \(set.path)")
