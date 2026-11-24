import CoreGraphics
import Foundation

/// Where words go so they cover nobody: speech bubbles above their speaker,
/// names under their people. Pure geometry in scene coordinates (y up), so
/// it is tested apart from SpriteKit.
enum TownPlacement {
    /// A bubble placed: its centre, and where its tail points (the
    /// speaker's head).
    struct Bubble: Equatable {
        var centre: CGPoint
        var tail: CGPoint
    }

    /// Place a bubble of `size` for a speaker whose head is at `head`.
    /// Candidates, in order: straight above, above to the right, above to
    /// the left, beside the head on either side, then higher up in the
    /// first three places. The first that stays
    /// inside `viewport` and covers none of `obstacles` (other people's
    /// faces and names, other bubbles) wins; failing that, the one that
    /// covers least. It is then kept inside the viewport.
    ///
    /// `soft` obstacles (signs, flags) are avoided too, but covering one
    /// counts for less than covering a face.
    static func bubble(size: CGSize, head: CGPoint, obstacles: [CGRect], soft: [CGRect] = [], viewport: CGRect) -> Bubble {
        let gap: CGFloat = 10
        let rise = size.height / 2 + gap
        var candidates: [CGPoint] = []
        for lift in [CGFloat(0), size.height + 6, 2 * (size.height + 6)] {
            let y = head.y + rise + lift
            candidates.append(CGPoint(x: head.x, y: y))
            candidates.append(CGPoint(x: head.x + size.width / 2 - 14, y: y))
            candidates.append(CGPoint(x: head.x - size.width / 2 + 14, y: y))
            if lift == 0 {
                // Beside the head, before climbing higher.
                candidates.append(CGPoint(x: head.x + size.width / 2 + 16, y: head.y + 6))
                candidates.append(CGPoint(x: head.x - size.width / 2 - 16, y: head.y + 6))
            }
        }
        func rect(_ c: CGPoint) -> CGRect {
            CGRect(x: c.x - size.width / 2, y: c.y - size.height / 2, width: size.width, height: size.height)
        }
        func area(_ r: CGRect, _ list: [CGRect]) -> CGFloat {
            list.reduce(0) { sum, o in
                let i = r.intersection(o)
                return i.isNull ? sum : sum + i.width * i.height
            }
        }
        func covered(_ r: CGRect) -> CGFloat { area(r, obstacles) + 0.3 * area(r, soft) }
        let inset = viewport.insetBy(dx: 4, dy: 4)
        let best = candidates.first { inset.contains(rect($0)) && covered(rect($0)) == 0 }
            ?? candidates.min { a, b in
                let (ra, rb) = (rect(a), rect(b))
                return (covered(ra) + outside(ra, inset), a.y) < (covered(rb) + outside(rb, inset), b.y)
            }!
        let centre = keep(rect(best), inside: inset)
        return Bubble(centre: CGPoint(x: centre.midX, y: centre.midY), tail: head)
    }

    /// How much of `r` lies outside `bounds`, as an area.
    private static func outside(_ r: CGRect, _ bounds: CGRect) -> CGFloat {
        let inside = r.intersection(bounds)
        return r.width * r.height - (inside.isNull ? 0 : inside.width * inside.height)
    }

    /// Slide `r` into `bounds` (as far as it fits).
    static func keep(_ r: CGRect, inside bounds: CGRect) -> CGRect {
        var r = r
        if r.width <= bounds.width {
            r.origin.x = min(max(r.minX, bounds.minX), bounds.maxX - r.width)
        }
        if r.height <= bounds.height {
            r.origin.y = min(max(r.minY, bounds.minY), bounds.maxY - r.height)
        }
        return r
    }

    /// A name under a person: its rectangle where it would go, and how much
    /// it matters (higher keeps its place).
    struct Label: Equatable {
        var id: String
        var rect: CGRect
        var priority: Int
    }

    /// Where each name goes: unmoved, nudged down a line, or hidden, so no
    /// two overlap. Higher priority first, then by id, so it is the same
    /// every time. Returns each id's downward shift, or nil to hide it.
    static func labels(_ labels: [Label], step: CGFloat) -> [String: CGFloat?] {
        var placed: [CGRect] = []
        var out: [String: CGFloat?] = [:]
        for label in labels.sorted(by: { ($0.priority, $1.id) > ($1.priority, $0.id) }) {
            var shift: CGFloat?
            for down in [CGFloat(0), step] {
                let r = label.rect.offsetBy(dx: 0, dy: -down)
                if !placed.contains(where: { $0.intersects(r) }) {
                    shift = down
                    placed.append(r)
                    break
                }
            }
            out[label.id] = shift
        }
        return out
    }
}
