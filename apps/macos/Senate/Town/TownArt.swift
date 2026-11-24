import CoreGraphics
import Foundation
import SpriteKit

/// The Town's colours, after the app's Theme (its surfaces, its accent, the
/// colours of meaning and of who a mention names), warmed for a cosy
/// neighbourhood: one set for light, one for dark. Plain values, so the
/// scene needs no AppKit to resolve them.
struct TownPalette: Equatable, Sendable {
    var ground: UInt32
    var groundSpeck: UInt32
    var street: UInt32
    var streetEdge: UInt32
    var floor: UInt32
    var floorLine: UInt32
    var hallFloor: UInt32
    var rug: UInt32
    var wall: UInt32
    var wallTop: UInt32
    var desk: UInt32
    var deskTop: UInt32
    var screen: UInt32
    var glow: UInt32
    var ink: UInt32
    var inkSoft: UInt32
    var skin: UInt32
    var blush: UInt32
    var hair: UInt32
    var trousers: UInt32
    var claude: UInt32
    var codex: UInt32
    var agy: UInt32
    var other: UInt32
    var accent: UInt32
    var attention: UInt32
    var leaf: UInt32
    var leafDark: UInt32
    var trunk: UInt32
    var bubble: UInt32
    var sign: UInt32
    var dark: Bool

    static let light = TownPalette(
        ground: 0xE3EBDB, groundSpeck: 0xD3DFC8, street: 0xF1EEE8, streetEdge: 0xDDD7CC,
        floor: 0xF8F1E5, floorLine: 0xEFE4D2, hallFloor: 0xF3E7D0, rug: 0xE1E8FF,
        wall: 0xB39E84, wallTop: 0xD4C3AB, desk: 0xA97C55, deskTop: 0xC99D72,
        screen: 0x2B303B, glow: 0x7FD39A, ink: 0x161617, inkSoft: 0x6B6B6E,
        skin: 0xF3D3B5, blush: 0xF0A49B, hair: 0x5A4334, trousers: 0x464652,
        claude: 0xDE8A3E, codex: 0x5B58D6, agy: 0x13878C, other: 0x7D7D82,
        accent: 0x2F52C0, attention: 0xF2A93B, leaf: 0x93C47F, leafDark: 0x6EA35C, trunk: 0x9B7653,
        bubble: 0xFFFFFF, sign: 0xFBFBFA, dark: false
    )

    static let dark = TownPalette(
        ground: 0x18201B, groundSpeck: 0x1E2922, street: 0x252527, streetEdge: 0x303034,
        floor: 0x2B2722, floorLine: 0x332E28, hallFloor: 0x33291D, rug: 0x2E3552,
        wall: 0x5C4F40, wallTop: 0x76685A, desk: 0x6B5544, deskTop: 0x8A6E58,
        screen: 0x0D1015, glow: 0x6BD68D, ink: 0xEDEDEE, inkSoft: 0xA3A3A8,
        skin: 0xE9C6A6, blush: 0xD98C86, hair: 0x3A2C24, trousers: 0x3C3C46,
        claude: 0xFFB86B, codex: 0xADABFF, agy: 0x6FD3D6, other: 0xA3A3A8,
        accent: 0xA5B8FF, attention: 0xFFC56E, leaf: 0x3F6B47, leafDark: 0x2E5336, trunk: 0x6B5544,
        bubble: 0x38383C, sign: 0x222224, dark: true
    )

    func shirt(_ runtime: TownRuntime) -> UInt32 {
        switch runtime {
        case .claude: claude
        case .codex: codex
        case .agy: agy
        case .other: other
        }
    }

    static func color(_ hex: UInt32, alpha: CGFloat = 1) -> SKColor {
        SKColor(
            red: CGFloat((hex >> 16) & 0xFF) / 255, green: CGFloat((hex >> 8) & 0xFF) / 255,
            blue: CGFloat(hex & 0xFF) / 255, alpha: alpha
        )
    }

    static func cg(_ hex: UInt32, alpha: CGFloat = 1) -> CGColor {
        CGColor(
            srgbRed: CGFloat((hex >> 16) & 0xFF) / 255, green: CGFloat((hex >> 8) & 0xFF) / 255,
            blue: CGFloat(hex & 0xFF) / 255, alpha: alpha
        )
    }
}

/// A person's pose: one texture each.
enum TownPose: CaseIterable, Hashable {
    case stand, walkA, walkB, typeA, typeB, sleep, wave
}

/// The Town's sprites, drawn once per palette with Core Graphics: little
/// round people in their runtime's colour (its glyph on the shirt), desks,
/// rooms, trees, a flag. Nothing is loaded from files and nothing is copied:
/// every shape is drawn here.
@MainActor
final class TownArt {
    let palette: TownPalette
    /// Drawn at this many pixels a point, so zooming in stays crisp.
    static let scale: CGFloat = 3

    private var people: [String: SKTexture] = [:]
    private(set) lazy var desk = texture(CGSize(width: 44, height: 22), drawDesk(working: false))
    private(set) lazy var deskWorking = texture(CGSize(width: 44, height: 22), drawDesk(working: true))
    private(set) lazy var speakerDesk = texture(CGSize(width: 92, height: 30), drawSpeakerDesk)
    private(set) lazy var hat = texture(CGSize(width: 16, height: 9), drawHat)
    private(set) lazy var flag = texture(CGSize(width: 22, height: 30), drawFlag)
    private(set) lazy var tree = texture(CGSize(width: 40, height: 48), drawTree)
    private(set) lazy var bush = texture(CGSize(width: 26, height: 16), drawBush)
    private(set) lazy var room = texture(TownLayout.room, drawRoom(TownLayout.room, hall: false))
    private(set) lazy var hall = texture(TownLayout.hallRoom, drawRoom(TownLayout.hallRoom, hall: true))
    private(set) lazy var raisedHand = texture(CGSize(width: 14, height: 14), drawRaised)
    private(set) lazy var waitBadge = texture(CGSize(width: 22, height: 13), drawWaitBadge)

    init(palette: TownPalette) {
        self.palette = palette
    }

    /// A person in `runtime`'s colour in `pose`; `visitor` is the person a
    /// thread waits on (a coat, no glyph).
    func person(_ runtime: TownRuntime, _ pose: TownPose, visitor: Bool = false) -> SKTexture {
        let key = "\(runtime.rawValue).\(pose).\(visitor)"
        if let made = people[key] { return made }
        let made = texture(CGSize(width: 24, height: 34)) { [palette] ctx in
            Self.drawPerson(ctx, palette: palette, runtime: runtime, pose: pose, visitor: visitor)
        }
        people[key] = made
        return made
    }

    // MARK: Drawing

    private func texture(_ size: CGSize, _ draw: @MainActor (CGContext) -> Void) -> SKTexture {
        let scale = Self.scale
        let w = Int(size.width * scale), h = Int(size.height * scale)
        guard let ctx = CGContext(
            data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0,
            space: CGColorSpace(name: CGColorSpace.sRGB)!,
            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
        ) else { return SKTexture() }
        ctx.scaleBy(x: scale, y: scale)
        draw(ctx)
        guard let image = ctx.makeImage() else { return SKTexture() }
        let texture = SKTexture(cgImage: image)
        texture.filteringMode = .linear
        return texture
    }

    private static func fill(_ ctx: CGContext, _ hex: UInt32, alpha: CGFloat = 1, _ path: CGPath) {
        ctx.addPath(path)
        ctx.setFillColor(TownPalette.cg(hex, alpha: alpha))
        ctx.fillPath()
    }

    private static func round(_ rect: CGRect, _ r: CGFloat) -> CGPath {
        CGPath(roundedRect: rect, cornerWidth: r, cornerHeight: r, transform: nil)
    }

    static func drawPerson(_ ctx: CGContext, palette p: TownPalette, runtime: TownRuntime, pose: TownPose, visitor: Bool) {
        // The visitor is a person, not an agent: a coat in the colour of
        // what waits on you, a letter in hand.
        let shirt = visitor ? p.attention : p.shirt(runtime)
        let asleep = pose == .sleep
        // Shadow.
        fill(ctx, 0x000000, alpha: p.dark ? 0.28 : 0.12, CGPath(ellipseIn: CGRect(x: 4, y: 0.5, width: 16, height: 4.5), transform: nil))
        // Legs.
        let legs: (CGFloat, CGFloat) = switch pose {
        case .walkA: (1.6, -1.6)
        case .walkB: (-1.6, 1.6)
        default: (0, 0)
        }
        fill(ctx, p.trousers, round(CGRect(x: 7.5, y: 2.5 + max(legs.0, 0), width: 3.6, height: 8 - abs(legs.0)), 1.6))
        fill(ctx, p.trousers, round(CGRect(x: 12.9, y: 2.5 + max(legs.1, 0), width: 3.6, height: 8 - abs(legs.1)), 1.6))
        // Arms, behind the body when down; forward (up) while typing.
        let armLift: (CGFloat, CGFloat) = switch pose {
        case .typeA: (3, 1)
        case .typeB: (1, 3)
        case .walkA: (1, -1)
        case .walkB: (-1, 1)
        default: (0, 0)
        }
        fill(ctx, shirt, round(CGRect(x: 3.2, y: 10 + armLift.0, width: 3.6, height: 9), 1.8))
        if pose == .wave {
            fill(ctx, shirt, round(CGRect(x: 17.6, y: 17, width: 3.6, height: 10), 1.8))
            fill(ctx, p.skin, CGPath(ellipseIn: CGRect(x: 17.4, y: 26, width: 4, height: 4), transform: nil))
        } else {
            fill(ctx, shirt, round(CGRect(x: 17.2, y: 10 + armLift.1, width: 3.6, height: 9), 1.8))
        }
        // Body.
        fill(ctx, shirt, round(CGRect(x: 5, y: 8, width: 14, height: 13.5), 5))
        if visitor {
            // A coat's lapels, and a letter held in front.
            ctx.setStrokeColor(TownPalette.cg(0x5C3400, alpha: 0.6))
            ctx.setLineWidth(1)
            ctx.move(to: CGPoint(x: 12, y: 9)); ctx.addLine(to: CGPoint(x: 9, y: 20)); ctx.strokePath()
            ctx.move(to: CGPoint(x: 12, y: 9)); ctx.addLine(to: CGPoint(x: 15, y: 20)); ctx.strokePath()
            let letter = CGRect(x: 6.5, y: 9.5, width: 9, height: 6.5)
            fill(ctx, 0xFFFFFF, round(letter, 1))
            ctx.setStrokeColor(TownPalette.cg(0x8A4F00))
            ctx.setLineWidth(0.8)
            ctx.addPath(round(letter, 1)); ctx.strokePath()
            ctx.move(to: CGPoint(x: letter.minX, y: letter.maxY))
            ctx.addLine(to: CGPoint(x: letter.midX, y: letter.midY - 0.5))
            ctx.addLine(to: CGPoint(x: letter.maxX, y: letter.maxY))
            ctx.strokePath()
        } else {
            drawGlyph(ctx, runtime, at: CGPoint(x: 12, y: 14.6), color: p.dark ? 0x1B1B1E : 0xFFFFFF)
        }
        // Head.
        let head = CGPoint(x: 12, y: asleep ? 24.5 : 26)
        fill(ctx, p.skin, CGPath(ellipseIn: CGRect(x: head.x - 7.5, y: head.y - 7, width: 15, height: 14), transform: nil))
        // Hair: a soft cap over the top of the head.
        ctx.saveGState()
        ctx.addEllipse(in: CGRect(x: head.x - 7.5, y: head.y - 7, width: 15, height: 14))
        ctx.clip()
        fill(ctx, p.hair, CGPath(ellipseIn: CGRect(x: head.x - 9, y: head.y + 1.5, width: 18, height: 12), transform: nil))
        ctx.restoreGState()
        // Face.
        let ink: UInt32 = 0x26262B
        if asleep {
            ctx.setStrokeColor(TownPalette.cg(ink))
            ctx.setLineWidth(0.9)
            ctx.setLineCap(.round)
            for x in [head.x - 3, head.x + 3] {
                ctx.move(to: CGPoint(x: x - 1.3, y: head.y - 1))
                ctx.addQuadCurve(to: CGPoint(x: x + 1.3, y: head.y - 1), control: CGPoint(x: x, y: head.y - 2.2))
                ctx.strokePath()
            }
        } else {
            for x in [head.x - 2.8, head.x + 2.8] {
                fill(ctx, ink, CGPath(ellipseIn: CGRect(x: x - 0.95, y: head.y - 1.9, width: 1.9, height: 2.3), transform: nil))
            }
        }
        for x in [head.x - 4.6, head.x + 4.6] {
            fill(ctx, p.blush, alpha: 0.7, CGPath(ellipseIn: CGRect(x: x - 1.4, y: head.y - 4, width: 2.8, height: 1.6), transform: nil))
        }
    }

    /// The runtime's mark: an asterisk, a hexagon, a triangle, a dot. Simple
    /// shapes of our own, not any product's logo.
    private static func drawGlyph(_ ctx: CGContext, _ runtime: TownRuntime, at c: CGPoint, color: UInt32) {
        ctx.setStrokeColor(TownPalette.cg(color))
        ctx.setFillColor(TownPalette.cg(color))
        ctx.setLineWidth(1.1)
        ctx.setLineCap(.round)
        switch runtime {
        case .claude:
            for i in 0..<4 {
                let a = CGFloat(i) * .pi / 4
                ctx.move(to: CGPoint(x: c.x - cos(a) * 2.8, y: c.y - sin(a) * 2.8))
                ctx.addLine(to: CGPoint(x: c.x + cos(a) * 2.8, y: c.y + sin(a) * 2.8))
            }
            ctx.strokePath()
        case .codex:
            for i in 0...6 {
                let a = CGFloat(i) * .pi / 3 + .pi / 6
                let pt = CGPoint(x: c.x + cos(a) * 2.9, y: c.y + sin(a) * 2.9)
                if i == 0 { ctx.move(to: pt) } else { ctx.addLine(to: pt) }
            }
            ctx.strokePath()
        case .agy:
            ctx.move(to: CGPoint(x: c.x, y: c.y + 3))
            ctx.addLine(to: CGPoint(x: c.x + 2.9, y: c.y - 2))
            ctx.addLine(to: CGPoint(x: c.x - 2.9, y: c.y - 2))
            ctx.closePath()
            ctx.strokePath()
        case .other:
            ctx.fillEllipse(in: CGRect(x: c.x - 1.5, y: c.y - 1.5, width: 3, height: 3))
        }
    }

    private func drawDesk(working: Bool) -> @MainActor (CGContext) -> Void {
        { [palette] ctx in
            let p = palette
            Self.fill(ctx, 0x000000, alpha: p.dark ? 0.25 : 0.10, Self.round(CGRect(x: 2, y: 0, width: 40, height: 5), 2.5))
            Self.fill(ctx, p.desk, Self.round(CGRect(x: 2, y: 2, width: 40, height: 12), 3))
            Self.fill(ctx, p.deskTop, Self.round(CGRect(x: 2, y: 8, width: 40, height: 8), 3))
            // A laptop, open towards its senator (the far side).
            Self.fill(ctx, p.screen, Self.round(CGRect(x: 14, y: 12, width: 16, height: 9.5), 1.5))
            if working {
                Self.fill(ctx, p.glow, alpha: 0.85, Self.round(CGRect(x: 15.5, y: 13.5, width: 13, height: 6.5), 1))
                ctx.setStrokeColor(TownPalette.cg(p.screen, alpha: 0.6))
                ctx.setLineWidth(0.7)
                for (i, w) in [CGFloat(8), 5, 9].enumerated() {
                    ctx.move(to: CGPoint(x: 17, y: 18.5 - CGFloat(i) * 2))
                    ctx.addLine(to: CGPoint(x: 17 + w, y: 18.5 - CGFloat(i) * 2))
                }
                ctx.strokePath()
            }
            // A mug.
            Self.fill(ctx, p.dark ? 0xD9D9DC : 0xFFFFFF, Self.round(CGRect(x: 34, y: 10, width: 4, height: 4.5), 1))
        }
    }

    private func drawSpeakerDesk(_ ctx: CGContext) {
        let p = palette
        Self.fill(ctx, 0x000000, alpha: p.dark ? 0.25 : 0.10, Self.round(CGRect(x: 2, y: 0, width: 88, height: 6), 3))
        Self.fill(ctx, p.desk, Self.round(CGRect(x: 2, y: 2, width: 88, height: 18), 4))
        Self.fill(ctx, p.deskTop, Self.round(CGRect(x: 2, y: 12, width: 88, height: 10), 4))
        // The Speaker's amber band, and a little bell.
        Self.fill(ctx, p.attention, Self.round(CGRect(x: 6, y: 6, width: 80, height: 3), 1.5))
        Self.fill(ctx, p.attention, CGPath(ellipseIn: CGRect(x: 70, y: 18, width: 8, height: 7), transform: nil))
        // Papers.
        Self.fill(ctx, p.dark ? 0xCFCFD3 : 0xFFFFFF, Self.round(CGRect(x: 14, y: 15, width: 12, height: 8), 1))
        Self.fill(ctx, p.dark ? 0xB9B9BE : 0xF1F1F0, Self.round(CGRect(x: 17, y: 17, width: 12, height: 8), 1))
    }

    private func drawHat(_ ctx: CGContext) {
        let p = palette
        Self.fill(ctx, p.accent, Self.round(CGRect(x: 0.5, y: 0.5, width: 15, height: 3), 1.5))
        Self.fill(ctx, p.accent, Self.round(CGRect(x: 3.5, y: 2, width: 9, height: 6.5), 2.5))
        Self.fill(ctx, p.attention, CGPath(rect: CGRect(x: 3.5, y: 2.6, width: 9, height: 1.4), transform: nil))
    }

    private func drawFlag(_ ctx: CGContext) {
        let p = palette
        Self.fill(ctx, p.ink, alpha: 0.7, Self.round(CGRect(x: 3, y: 0, width: 1.6, height: 30), 0.8))
        let pennant = CGMutablePath()
        pennant.move(to: CGPoint(x: 4.6, y: 29))
        pennant.addLine(to: CGPoint(x: 21, y: 24))
        pennant.addLine(to: CGPoint(x: 4.6, y: 18))
        pennant.closeSubpath()
        Self.fill(ctx, p.attention, pennant)
    }

    private func drawRaised(_ ctx: CGContext) {
        let p = palette
        Self.fill(ctx, p.attention, CGPath(ellipseIn: CGRect(x: 0.5, y: 0.5, width: 13, height: 13), transform: nil))
        Self.fill(ctx, p.dark ? 0x1B1B1E : 0xFFFFFF, Self.round(CGRect(x: 6.1, y: 5, width: 1.8, height: 6), 0.9))
        Self.fill(ctx, p.dark ? 0x1B1B1E : 0xFFFFFF, CGPath(ellipseIn: CGRect(x: 6.1, y: 2.2, width: 1.8, height: 1.8), transform: nil))
    }

    /// "Waiting": a pill with three dots, over a visitor's head.
    private func drawWaitBadge(_ ctx: CGContext) {
        let p = palette
        Self.fill(ctx, p.attention, Self.round(CGRect(x: 0.5, y: 0.5, width: 21, height: 12), 6))
        for x in [CGFloat(6.5), 11, 15.5] {
            Self.fill(ctx, p.dark ? 0x1B1B1E : 0xFFFFFF, CGPath(ellipseIn: CGRect(x: x - 1.4, y: 5.1, width: 2.8, height: 2.8), transform: nil))
        }
    }

    private func drawTree(_ ctx: CGContext) {
        let p = palette
        Self.fill(ctx, 0x000000, alpha: p.dark ? 0.3 : 0.12, CGPath(ellipseIn: CGRect(x: 6, y: 0, width: 28, height: 8), transform: nil))
        Self.fill(ctx, p.trunk, Self.round(CGRect(x: 17, y: 3, width: 6, height: 16), 2))
        for (x, y, r) in [(CGFloat(12), CGFloat(24), CGFloat(11)), (28, 24, 10), (20, 34, 12)] {
            Self.fill(ctx, p.leafDark, CGPath(ellipseIn: CGRect(x: x - r, y: y - r - 1.5, width: r * 2, height: r * 2), transform: nil))
            Self.fill(ctx, p.leaf, CGPath(ellipseIn: CGRect(x: x - r, y: y - r + 0.5, width: r * 2, height: r * 2 - 1), transform: nil))
        }
    }

    private func drawBush(_ ctx: CGContext) {
        let p = palette
        for (x, r) in [(CGFloat(7), CGFloat(6)), (14, 7.5), (20, 5.5)] {
            Self.fill(ctx, p.leafDark, CGPath(ellipseIn: CGRect(x: x - r, y: 1, width: r * 2, height: r * 2), transform: nil))
            Self.fill(ctx, p.leaf, CGPath(ellipseIn: CGRect(x: x - r + 0.5, y: 2.5, width: r * 2 - 1, height: r * 2 - 1.5), transform: nil))
        }
    }

    /// A room seen from above: floorboards, a rug, a plant in a corner,
    /// walls with a door in the middle of the bottom one.
    private func drawRoom(_ size: CGSize, hall: Bool) -> @MainActor (CGContext) -> Void {
        { [palette] ctx in
            let p = palette
            let rect = CGRect(origin: .zero, size: size)
            let wall: CGFloat = 7
            Self.fill(ctx, 0x000000, alpha: p.dark ? 0.3 : 0.08, Self.round(rect.offsetBy(dx: 0, dy: -2).insetBy(dx: 1, dy: 1), 12))
            Self.fill(ctx, p.wall, Self.round(rect, 12))
            let inner = rect.insetBy(dx: wall, dy: wall)
            Self.fill(ctx, hall ? p.hallFloor : p.floor, Self.round(inner, 6))
            // Floorboards.
            ctx.saveGState()
            ctx.addPath(Self.round(inner, 6))
            ctx.clip()
            ctx.setStrokeColor(TownPalette.cg(p.floorLine))
            ctx.setLineWidth(1)
            var y = inner.minY + 14
            while y < inner.maxY {
                ctx.move(to: CGPoint(x: inner.minX, y: y))
                ctx.addLine(to: CGPoint(x: inner.maxX, y: y))
                y += 14
            }
            ctx.strokePath()
            ctx.restoreGState()
            // A rug in the middle.
            let rug = CGRect(x: inner.midX - inner.width * 0.3, y: inner.minY + 18, width: inner.width * 0.6, height: inner.height * 0.34)
            Self.fill(ctx, hall ? p.attention : p.rug, alpha: hall ? 0.35 : 0.9, Self.round(rug, rug.height / 2))
            // A plant in the top-left corner.
            Self.fill(ctx, p.trunk, Self.round(CGRect(x: inner.minX + 5, y: inner.maxY - 17, width: 11, height: 10), 3))
            for (dx, dy) in [(CGFloat(7), CGFloat(-6)), (13, -5), (10, -1)] {
                Self.fill(ctx, p.leaf, CGPath(ellipseIn: CGRect(x: inner.minX + dx - 4, y: inner.maxY + dy - 4, width: 8, height: 8), transform: nil))
            }
            // The wall's top edge, lighter, and the door: a gap in the bottom wall.
            ctx.setStrokeColor(TownPalette.cg(p.wallTop))
            ctx.setLineWidth(2)
            ctx.addPath(Self.round(rect.insetBy(dx: 2, dy: 2), 10))
            ctx.strokePath()
            let door = CGRect(x: rect.midX - 17, y: 0, width: 34, height: wall + 1)
            Self.fill(ctx, hall ? p.hallFloor : p.floor, CGPath(rect: door, transform: nil))
            Self.fill(ctx, p.deskTop, CGPath(rect: CGRect(x: door.minX, y: 0, width: door.width, height: 2.5), transform: nil))
            if hall {
                // Two columns either side of the hall's door.
                for x in [door.minX - 12, door.maxX + 4] {
                    Self.fill(ctx, p.wallTop, Self.round(CGRect(x: x, y: -0, width: 8, height: 14), 2))
                }
            }
        }
    }
}
