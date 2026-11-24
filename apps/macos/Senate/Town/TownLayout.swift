import CoreGraphics
import Foundation

/// Where everything stands, from the committees' names alone: the town hall
/// in the middle, each committee's house on a lot of a spiral around it,
/// streets between the lots. The same names give the same town on every
/// launch (the lots go in the order of a stable hash of the names, not of
/// arrival), and a house's rooms are the same size whatever lives there.
///
/// Scene coordinates: y up, the hall's lot centred on the origin.
struct TownLayout: Equatable {
    /// A lot: a room and the street around it.
    static let lot = CGSize(width: 300, height: 250)
    /// A committee's room, inside its lot.
    static let room = CGSize(width: 232, height: 168)
    /// The town hall's room (it fills more of its lot).
    static let hallRoom = CGSize(width: 260, height: 180)
    /// Desks in a room: three across, two deep; more senators stand.
    static let deskColumns = 3
    static let deskRows = 2

    /// Each committee's lot, as a cell of the grid (the hall is (0, 0)).
    private(set) var cells: [String: GridCell] = [:]

    struct GridCell: Hashable {
        var x: Int
        var y: Int
    }

    init(committees names: [String]) {
        let ordered = Array(Set(names)).sorted { (Self.hash($0), $0) < (Self.hash($1), $1) }
        let spiral = Self.spiral(count: ordered.count)
        for (name, cell) in zip(ordered, spiral) { cells[name] = cell }
    }

    // MARK: Lots and rooms

    static func centre(of cell: GridCell) -> CGPoint {
        CGPoint(x: CGFloat(cell.x) * lot.width, y: CGFloat(cell.y) * lot.height)
    }

    /// A committee's room; `nil` for a committee not in town.
    func room(_ committee: String) -> CGRect? {
        cells[committee].map { Self.roomRect(around: Self.centre(of: $0), size: Self.room) }
    }

    /// The town hall's room.
    var hall: CGRect { Self.roomRect(around: .zero, size: Self.hallRoom) }

    private static func roomRect(around centre: CGPoint, size: CGSize) -> CGRect {
        // A little above the lot's middle: the street runs below.
        CGRect(x: centre.x - size.width / 2, y: centre.y - size.height / 2 + 14, width: size.width, height: size.height)
    }

    /// Every lot's rectangle, the hall's included: what "fit" frames.
    var bounds: CGRect {
        var all = hall.insetBy(dx: -40, dy: -50)
        for cell in cells.values {
            let c = Self.centre(of: cell)
            all = all.union(CGRect(x: c.x - Self.lot.width / 2, y: c.y - Self.lot.height / 2,
                                   width: Self.lot.width, height: Self.lot.height))
        }
        return all
    }

    // MARK: Inside a room

    /// Where the `index`th senator of a room sits (the chair is 0, at the
    /// front desk): desks on a 3 × 2 grid, then standing room along the
    /// bottom wall.
    static func seat(_ index: Int, in room: CGRect) -> CGPoint {
        let perRow = deskColumns
        if index < deskColumns * deskRows {
            let col = index % perRow
            let row = index / perRow
            let dx = room.width / CGFloat(perRow + 1)
            return CGPoint(x: room.minX + dx * CGFloat(col + 1), y: room.maxY - 54 - CGFloat(row) * 62)
        }
        let extra = index - deskColumns * deskRows
        let across = 7
        let dx = room.width / CGFloat(across + 1)
        return CGPoint(x: room.minX + dx * CGFloat(extra % across + 1),
                       y: room.minY + 22 + CGFloat(extra / across % 2) * 14)
    }

    /// The Secretary's place at the Speaker's desk.
    var secretarySeat: CGPoint { CGPoint(x: hall.midX, y: hall.maxY - 64) }

    /// A room's door: the middle of its bottom wall.
    static func door(of room: CGRect) -> CGPoint { CGPoint(x: room.midX, y: room.minY) }

    // MARK: Walking

    /// The street below a room.
    static func streetY(below room: CGRect) -> CGFloat {
        streetY(row: Int(((room.midY - 14) / lot.height).rounded()))
    }

    /// The street running below a row of lots.
    static func streetY(row: Int) -> CGFloat { CGFloat(row) * lot.height - 104 }

    /// The way from a room to another along the streets: out of the door,
    /// along the street, up or down the avenue beside the lot, along the
    /// target's street and in at its door. The two end points are `from`
    /// and `to`; a walk inside one room is straight.
    static func path(from: CGPoint, in a: CGRect, to: CGPoint, in b: CGRect) -> [CGPoint] {
        if a == b { return [from, to] }
        let doorA = door(of: a)
        let doorB = door(of: b)
        let yA = streetY(below: a)
        let yB = streetY(below: b)
        var points = [from, doorA, CGPoint(x: doorA.x, y: yA)]
        if abs(yA - yB) > 1 {
            // The avenue between this lot and the next one towards the target.
            let avenue = doorA.x + (doorB.x >= doorA.x ? lot.width / 2 : -lot.width / 2)
            points.append(CGPoint(x: avenue, y: yA))
            points.append(CGPoint(x: avenue, y: yB))
        }
        points += [CGPoint(x: doorB.x, y: yB), doorB, to]
        return dedupe(points)
    }

    /// How long a path is, in points.
    static func length(_ path: [CGPoint]) -> CGFloat {
        zip(path, path.dropFirst()).reduce(0) { $0 + hypot($1.1.x - $1.0.x, $1.1.y - $1.0.y) }
    }

    private static func dedupe(_ points: [CGPoint]) -> [CGPoint] {
        var out: [CGPoint] = []
        for p in points where out.last.map({ hypot($0.x - p.x, $0.y - p.y) > 0.5 }) ?? true { out.append(p) }
        return out
    }

    // MARK: Determinism

    /// FNV-1a over the name's UTF-8: the same on every launch and machine
    /// (Swift's `hashValue` is seeded per process).
    static func hash(_ text: String) -> UInt64 {
        var h: UInt64 = 0xcbf2_9ce4_8422_2325
        for byte in text.utf8 {
            h ^= UInt64(byte)
            h = h &* 0x100_0000_01b3
        }
        return h
    }

    /// The first `count` cells of a square spiral around (0, 0), not (0, 0)
    /// itself: ring 1 (the eight around the hall), then ring 2, …; within a
    /// ring, the nearest the hall first (the sides' middles before the
    /// corners), then from the top row down, left to right. A small town
    /// stays compact around its hall.
    static func spiral(count: Int) -> [GridCell] {
        var cells: [GridCell] = []
        var ring = 1
        while cells.count < count {
            var ringCells: [GridCell] = []
            for y in -ring...ring {
                for x in -ring...ring where max(abs(x), abs(y)) == ring { ringCells.append(GridCell(x: x, y: y)) }
            }
            ringCells.sort { a, b in
                (abs(a.x) + abs(a.y), -a.y, a.x) < (abs(b.x) + abs(b.y), -b.y, b.x)
            }
            cells += ringCells
            ring += 1
        }
        return Array(cells.prefix(count))
    }
}
