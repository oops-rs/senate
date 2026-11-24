import SpriteKit

/// The Town, drawn: a pure function of `TownState` (houses, desks, little
/// people and their moods) plus the visits `TownDirector` makes of the
/// events. No AppKit: input comes in as view points from SwiftUI's gestures,
/// so the same scene runs on iOS.
///
/// It spends nothing when nothing moves: 60 frames a second while someone
/// walks or the person pans; a few a second while someone types or may
/// wander; paused when the town is still or the page is not shown.
@MainActor
final class TownScene: SKScene {
    /// What a click landed on.
    enum Hit: Equatable {
        case senator(id: String, sessionId: String?)
        case thread(TownThreadRef)
        case committee(String)
        case hall
    }

    /// No walking: visits fade a bubble from one senator to the other.
    var reduceMotion = false {
        didSet { if reduceMotion != oldValue { rebuild() } }
    }
    /// The page is shown: off, the view is paused and nothing runs.
    var visible = true {
        didSet { wake() }
    }
    /// Told when a visit starts and ends (for the accessible list's log).
    var onVisit: ((TownAction) -> Void)?

    private(set) var state = TownState()
    private(set) var layout = TownLayout(committees: [])
    private var art = TownArt(palette: .light)
    private var director = TownDirector()

    private let world = SKNode()
    private let cam = SKCameraNode()
    private var ground = SKNode()
    private var houses: [String: SKNode] = [:]
    private var hallNode = SKNode()
    private var people: [String: PersonNode] = [:]
    private var threads: [String: TownThreadRef] = [:]
    /// Senators out on a visit or a wander: their depth follows their y.
    private var walkers: Set<String> = []
    /// Things moving now (walks, fades, a pan): at full frame rate.
    private var movers = 0 {
        didSet { wake() }
    }
    /// Senators strolling about their rooms: slow, so a few frames a second.
    private var strollers = 0 {
        didSet { wake() }
    }
    private var fitted = false
    private var interacting = false

    override init(size: CGSize) {
        super.init(size: size)
        scaleMode = .resizeFill
        anchorPoint = CGPoint(x: 0.5, y: 0.5)
        addChild(world)
        addChild(cam)
        camera = cam
        backgroundColor = TownPalette.color(art.palette.ground)
    }

    required init?(coder: NSCoder) { fatalError("not from a coder") }

    // MARK: Input from the page

    /// Show this state, and act out these events.
    func apply(_ state: TownState, events: [TownEvent] = []) {
        let names = state.committees.map(\.name)
        let relayout = Set(names) != Set(self.state.committees.map(\.name)) || houses.isEmpty
        self.state = state
        if relayout {
            layout = TownLayout(committees: names)
            rebuild()
        } else {
            for committee in state.committees { drawHouse(committee) }
            drawHall()
            placePeople()
        }
        perform(director.accept(events, present: Set(people.keys), reduceMotion: reduceMotion))
        wake()
    }

    /// Redraw everything in these colours.
    func setPalette(_ palette: TownPalette) {
        guard palette != art.palette else { return }
        art = TownArt(palette: palette)
        backgroundColor = TownPalette.color(palette.ground)
        rebuild()
    }

    override func didChangeSize(_ oldSize: CGSize) {
        super.didChangeSize(oldSize)
        if !fitted, size.width > 10, !houses.isEmpty || !state.committees.isEmpty { fit() }
        wake()
    }

    /// Frame the whole town.
    func fit() {
        guard size.width > 10, size.height > 10 else { return }
        let bounds = layout.bounds
        let scale = max(bounds.width / size.width, bounds.height / size.height) * 1.08
        cam.setScale(min(max(scale, 0.6), Self.maxScale))
        cam.position = CGPoint(x: bounds.midX, y: bounds.midY)
        fitted = true
        updateLabels()
        wake()
    }

    static let minScale: CGFloat = 0.35
    static let maxScale: CGFloat = 5

    /// Move the view by a drag's step, in view points (y down).
    func pan(by delta: CGSize) {
        cam.position.x -= delta.width * cam.xScale
        cam.position.y += delta.height * cam.yScale
        clampCamera()
        fitted = true
        wake()
    }

    /// Zoom by `factor` (> 1 closer) keeping the point under `anchor` (view
    /// points, y down) where it is.
    func zoom(by factor: CGFloat, at anchor: CGPoint? = nil) {
        let before = anchor.map(scenePoint)
        let scale = min(max(cam.xScale / factor, Self.minScale), Self.maxScale)
        cam.setScale(scale)
        if let anchor, let before {
            let after = scenePoint(anchor)
            cam.position.x += before.x - after.x
            cam.position.y += before.y - after.y
        }
        clampCamera()
        fitted = true
        updateLabels()
        wake()
    }

    /// A gesture is under way: full frame rate until it ends.
    func setInteracting(_ on: Bool) {
        interacting = on
        wake()
    }

    var zoomScale: CGFloat { cam.xScale }

    /// The scene point under a view point (y down, from the top left).
    func scenePoint(_ p: CGPoint) -> CGPoint {
        CGPoint(
            x: cam.position.x + (p.x - size.width / 2) * cam.xScale,
            y: cam.position.y + (size.height / 2 - p.y) * cam.yScale
        )
    }

    /// The view point (y down) over a scene point.
    func viewPoint(_ p: CGPoint) -> CGPoint {
        CGPoint(
            x: (p.x - cam.position.x) / cam.xScale + size.width / 2,
            y: size.height / 2 - (p.y - cam.position.y) / cam.yScale
        )
    }

    /// Where a senator stands now, in the scene.
    func position(of id: String) -> CGPoint? { people[id]?.position }

    /// What is under a view point: a bubble or a flag first, then a
    /// senator, then a house.
    func hit(at viewPoint: CGPoint) -> Hit? {
        let point = scenePoint(viewPoint)
        let found = nodes(at: point).sorted { $0.zPosition > $1.zPosition }
        var best: (Hit, Int)?
        for node in found {
            var cursor: SKNode? = node
            while let n = cursor {
                if let hit = classify(n.name), best.map({ rank(hit) > $0.1 }) ?? true {
                    best = (hit, rank(hit))
                }
                cursor = n.parent
            }
        }
        return best?.0
    }

    private func rank(_ hit: Hit) -> Int {
        switch hit {
        case .thread: 3
        case .senator: 2
        case .committee, .hall: 1
        }
    }

    private func classify(_ name: String?) -> Hit? {
        guard let name, let colon = name.firstIndex(of: ":") else { return name == "hall" ? .hall : nil }
        let value = String(name[name.index(after: colon)...])
        switch name[..<colon] {
        case "senator": return people[value].map { .senator(id: value, sessionId: $0.senator.sessionId) }
        case "thread": return threads[value].map { .thread($0) }
        case "house": return .committee(value)
        default: return nil
        }
    }

    // MARK: Pace

    override func update(_ currentTime: TimeInterval) {
        for id in walkers {
            if let p = people[id] { p.zPosition = Self.depth(p.position.y) }
        }
        pace()
    }

    /// Unpause, and set the frame rate for what is going on.
    private func wake() {
        guard let view else { return }
        if !visible {
            view.isPaused = true
            return
        }
        view.isPaused = false
        pace()
    }

    private func pace() {
        guard let view, visible else { return }
        let fps: Int
        if movers > 0 || interacting {
            fps = 60
        } else if strollers > 0 {
            fps = 15
        } else if hasAmbient {
            fps = 4
        } else {
            // Still: one more frame draws the last change, then rest.
            fps = 0
        }
        if fps == 0 {
            if !view.isPaused {
                DispatchQueue.main.async { [weak self] in
                    guard let self, self.movers == 0, !self.interacting, !self.hasAmbient else { return }
                    self.view?.isPaused = true
                }
            }
        } else if view.preferredFramesPerSecond != fps {
            view.preferredFramesPerSecond = fps
        }
    }

    /// Something animates slowly: someone types, or may wander.
    private var hasAmbient: Bool {
        people.values.contains { p in
            !p.isHidden && (p.senator.mood == .working || (!reduceMotion && p.senator.mood == .idle))
        }
    }

    // MARK: Building

    private static func depth(_ y: CGFloat) -> CGFloat { 10 - y / 10_000 }

    /// A sprite the size of its texture in points (textures are drawn at
    /// `TownArt.scale` pixels a point).
    static func sprite(_ texture: SKTexture) -> SKSpriteNode {
        let size = texture.size()
        return SKSpriteNode(texture: texture, size: CGSize(width: size.width / TownArt.scale, height: size.height / TownArt.scale))
    }

    private func rebuild() {
        director.reset()
        walkers = []
        movers = 0
        strollers = 0
        world.removeAllChildren()
        houses = [:]
        people = [:]
        threads = [:]
        ground = SKNode()
        world.addChild(ground)
        drawGround()
        hallNode = SKNode()
        world.addChild(hallNode)
        drawHall()
        for committee in state.committees { drawHouse(committee) }
        placePeople()
        if !fitted { fit() }
        updateLabels()
        wake()
    }

    /// Grass, streets below every row of lots, avenues between columns, and
    /// a few trees where the hash of a lot says so.
    private func drawGround() {
        let p = art.palette
        var rows: Set<Int> = [0]
        var columns: Set<Int> = [0]
        for cell in layout.cells.values {
            rows.insert(cell.y)
            columns.insert(cell.x)
        }
        let (minX, maxX) = (columns.min() ?? 0, columns.max() ?? 0)
        let (minY, maxY) = (rows.min() ?? 0, rows.max() ?? 0)
        let left = CGFloat(minX) * TownLayout.lot.width - TownLayout.lot.width / 2 - 20
        let right = CGFloat(maxX) * TownLayout.lot.width + TownLayout.lot.width / 2 + 20
        let bottom = CGFloat(minY) * TownLayout.lot.height - TownLayout.lot.height / 2 - 10
        let top = CGFloat(maxY) * TownLayout.lot.height + TownLayout.lot.height / 2
        for row in minY...maxY {
            let street = SKSpriteNode(color: TownPalette.color(p.street), size: CGSize(width: right - left, height: 26))
            street.position = CGPoint(x: (left + right) / 2, y: TownLayout.streetY(row: row))
            street.zPosition = -2
            ground.addChild(street)
            for edge in [-14.0, 14.0] {
                let line = SKSpriteNode(color: TownPalette.color(p.streetEdge), size: CGSize(width: right - left, height: 1.5))
                line.position = CGPoint(x: street.position.x, y: street.position.y + edge)
                line.zPosition = -1.9
                ground.addChild(line)
            }
        }
        for column in minX...(maxX + 1) {
            let x = CGFloat(column) * TownLayout.lot.width - TownLayout.lot.width / 2
            let avenue = SKSpriteNode(color: TownPalette.color(p.street), size: CGSize(width: 22, height: top - bottom))
            avenue.position = CGPoint(x: x, y: (top + bottom) / 2)
            avenue.zPosition = -2
            ground.addChild(avenue)
        }
        // Trees and bushes at the corners of lots, the hall's included.
        let cells = [TownLayout.GridCell(x: 0, y: 0)] + layout.cells.values.sorted { ($0.x, $0.y) < ($1.x, $1.y) }
        for cell in cells {
            let c = TownLayout.centre(of: cell)
            let h = TownLayout.hash("\(cell.x),\(cell.y)")
            if h & 1 == 0 || cell == TownLayout.GridCell(x: 0, y: 0) {
                let tree = TownScene.sprite(art.tree)
                tree.anchorPoint = CGPoint(x: 0.5, y: 0)
                tree.position = CGPoint(x: c.x - 128, y: c.y + 70)
                tree.zPosition = Self.depth(tree.position.y)
                ground.addChild(tree)
            }
            if h & 2 == 0 {
                let bush = TownScene.sprite(art.bush)
                bush.position = CGPoint(x: c.x + 60, y: c.y - 82)
                bush.zPosition = Self.depth(bush.position.y)
                ground.addChild(bush)
            }
        }
    }

    private func label(_ text: String, size: CGFloat, bold: Bool = false, color: UInt32) -> SKLabelNode {
        let label = SKLabelNode(text: text)
        label.fontName = bold ? "HelveticaNeue-Bold" : "HelveticaNeue-Medium"
        label.fontSize = size
        label.fontColor = TownPalette.color(color)
        label.verticalAlignmentMode = .center
        label.horizontalAlignmentMode = .center
        return label
    }

    /// A name on a small sign above a room.
    private func sign(_ text: String, above room: CGRect, emphasis: Bool = false) -> SKNode {
        let node = SKNode()
        let shown = text.count > 26 ? String(text.prefix(25)) + "…" : text
        let title = label(shown, size: 12, bold: true, color: emphasis ? art.palette.accent : art.palette.ink)
        let width = title.frame.width + 18
        let plate = SKShapeNode(rectOf: CGSize(width: width, height: 20), cornerRadius: 10)
        plate.fillColor = TownPalette.color(art.palette.sign)
        plate.strokeColor = TownPalette.color(art.palette.streetEdge)
        plate.lineWidth = 1
        node.addChild(plate)
        node.addChild(title)
        node.position = CGPoint(x: room.midX, y: room.maxY + 14)
        node.zPosition = 30
        return node
    }

    private func drawHouse(_ committee: TownCommittee) {
        guard let room = layout.room(committee.name) else { return }
        houses[committee.name]?.removeFromParent()
        let house = SKNode()
        house.name = "house:\(committee.name)"
        let floor = TownScene.sprite(art.room)
        floor.position = CGPoint(x: room.midX, y: room.midY)
        floor.zPosition = 0
        floor.name = "house:\(committee.name)"
        house.addChild(floor)
        house.addChild(sign(committee.name, above: room))
        for (index, senator) in committee.senators.enumerated() where index < TownLayout.deskColumns * TownLayout.deskRows {
            let desk = TownScene.sprite(senator.mood == .working ? art.deskWorking : art.desk)
            desk.position = TownLayout.seat(index, in: room)
            desk.zPosition = Self.depth(desk.position.y)
            desk.alpha = senator.mood == .stopped ? 0.55 : 1
            house.addChild(desk)
        }
        if let first = committee.waiting.first {
            // A raised flag with how many wait, and a visitor at the door.
            threads[first.key] = first
            let flag = TownScene.sprite(art.flag)
            flag.anchorPoint = CGPoint(x: 0.15, y: 0)
            flag.position = CGPoint(x: room.maxX - 14, y: room.maxY - 2)
            flag.zPosition = 31
            flag.name = "thread:\(first.key)"
            if committee.waiting.count > 1 {
                let count = label("\(committee.waiting.count)", size: 9, bold: true, color: art.palette.dark ? 0x1B1B1E : 0x5C3400)
                count.position = CGPoint(x: 9, y: 24)
                flag.addChild(count)
            }
            house.addChild(flag)
            let visitor = TownScene.sprite(art.person(.other, .wave, visitor: true))
            visitor.anchorPoint = CGPoint(x: 0.5, y: 0)
            let door = TownLayout.door(of: room)
            visitor.position = CGPoint(x: door.x + 30, y: door.y - 30)
            visitor.zPosition = Self.depth(visitor.position.y)
            visitor.name = "thread:\(first.key)"
            house.addChild(visitor)
        }
        world.addChild(house)
        houses[committee.name] = house
    }

    private func drawHall() {
        hallNode.removeAllChildren()
        let room = layout.hall
        hallNode.name = "hall"
        let floor = TownScene.sprite(art.hall)
        floor.position = CGPoint(x: room.midX, y: room.midY)
        floor.zPosition = 0
        floor.name = "hall"
        hallNode.addChild(floor)
        hallNode.addChild(sign(L("town.hall"), above: room, emphasis: true))
        let desk = TownScene.sprite(art.speakerDesk)
        desk.position = CGPoint(x: layout.secretarySeat.x, y: layout.secretarySeat.y)
        desk.zPosition = Self.depth(desk.position.y)
        hallNode.addChild(desk)
        if state.hall.docket > 0 {
            let flag = TownScene.sprite(art.flag)
            flag.anchorPoint = CGPoint(x: 0.15, y: 0)
            flag.position = CGPoint(x: room.maxX - 16, y: room.maxY - 2)
            flag.zPosition = 31
            flag.name = "hall"
            hallNode.addChild(flag)
        }
    }

    // MARK: People

    /// Where a senator lives: its seat, its room.
    private func home(of id: String) -> (seat: CGPoint, room: CGRect)? {
        if let secretary = state.hall.secretary, secretary.id == id, state.home(of: id) == nil {
            return (layout.secretarySeat, layout.hall)
        }
        for committee in state.committees {
            if let index = committee.senators.firstIndex(where: { $0.id == id }), let room = layout.room(committee.name) {
                return (TownLayout.seat(index, in: room), room)
            }
        }
        return nil
    }

    private func placePeople() {
        let all = state.senators
        for (id, node) in people where all[id] == nil {
            node.removeFromParent()
            people[id] = nil
            walkers.remove(id)
        }
        for id in all.keys.sorted() {
            guard let senator = all[id], let home = home(of: id) else { continue }
            let node: PersonNode
            if let existing = people[id] {
                node = existing
            } else {
                node = PersonNode(senator: senator)
                node.name = "senator:\(id)"
                world.addChild(node)
                people[id] = node
            }
            node.senator = senator
            node.home = home.seat
            node.room = home.room
            if !director.out.contains(id) { settle(node) }
        }
        updateLabels()
    }

    /// Put a senator in its place for its mood.
    private func settle(_ node: PersonNode) {
        let s = node.senator
        endStroll(node)
        node.removeAllActions()
        node.body.removeAllActions()
        walkers.remove(s.id)
        node.isHidden = s.mood == .stopped
        node.setChair(s.chair, art: art)
        node.setRaised(s.mood == .waiting, art: art)
        node.setAsleep(s.mood == .paused, palette: art.palette)
        node.alpha = s.mood == .paused ? 0.85 : 1
        switch s.mood {
        case .working, .waiting, .paused:
            node.position = CGPoint(x: node.home.x, y: node.home.y + 2)
            node.seated = true
        case .idle, .stopped:
            node.position = CGPoint(x: node.home.x, y: node.home.y - 24)
            node.seated = false
        }
        node.zPosition = Self.depth(node.position.y)
        node.setLabel(s.name, palette: art.palette, seated: node.seated)
        switch s.mood {
        case .working:
            if reduceMotion {
                node.body.texture = art.person(s.runtime, .typeA)
            } else {
                node.body.texture = art.person(s.runtime, .typeA)
                node.body.run(.repeatForever(.animate(with: [art.person(s.runtime, .typeA), art.person(s.runtime, .typeB)], timePerFrame: 0.3)))
            }
        case .waiting: node.body.texture = art.person(s.runtime, .wave)
        case .paused: node.body.texture = art.person(s.runtime, .sleep)
        case .idle, .stopped: node.body.texture = art.person(s.runtime, .stand)
        }
        if s.mood == .idle && !reduceMotion { scheduleWander(node) }
    }

    // MARK: Wandering

    private func scheduleWander(_ node: PersonNode) {
        let wait = 4 + node.random() * 9
        node.run(.sequence([.wait(forDuration: wait), .run { [weak self, weak node] in
            guard let self, let node else { return }
            self.wander(node)
        }]), withKey: "wander")
    }

    private func wander(_ node: PersonNode) {
        guard node.senator.mood == .idle, !director.out.contains(node.senator.id) else { return }
        let room = node.room
        let target = CGPoint(
            x: room.minX + 22 + node.random() * (room.width - 44),
            y: room.minY + 16 + node.random() * max(room.height - 110, 10)
        )
        walkers.insert(node.senator.id)
        strollers += 1
        node.strolling = true
        walk(node, along: [node.position, target], speed: 40, legs: 0.3) { [weak self, weak node] in
            guard let self, let node else { return }
            node.strolling = false
            self.strollers -= 1
            self.walkers.remove(node.senator.id)
            node.body.texture = self.art.person(node.senator.runtime, .stand)
            self.scheduleWander(node)
        }
    }

    /// A stroll cut short (a visit, a new mood): it no longer counts.
    private func endStroll(_ node: PersonNode) {
        if node.strolling {
            node.strolling = false
            strollers -= 1
        }
    }

    // MARK: Visits

    private func perform(_ actions: [TownAction]) {
        for action in actions {
            switch action {
            case let .mood(id, _):
                // The state already holds the mood: put it in its place.
                if let node = people[id], !director.out.contains(id) { settle(node) }
            case let .visit(from, to, thread, line, walk):
                threads[thread.key] = thread
                onVisit?(action)
                if walk {
                    startWalk(from: from, to: to, thread: thread, line: line)
                } else {
                    fade(from: from, to: to, thread: thread, line: line)
                }
            }
        }
    }

    /// Where a visitor stands: beside the senator visited, else inside the
    /// thread's house (the hall when it is not in town).
    private func destination(to: String?, thread: TownThreadRef) -> (CGPoint, CGRect) {
        if let to, let target = people[to] {
            let spot = target.seated ? CGPoint(x: target.home.x - 30, y: target.home.y - 16)
                : CGPoint(x: target.position.x - 24, y: target.position.y - 4)
            return (spot, target.room)
        }
        let room = layout.room(thread.committee) ?? layout.hall
        return (CGPoint(x: room.midX, y: room.minY + 26), room)
    }

    private func startWalk(from: String, to: String?, thread: TownThreadRef, line: String) {
        guard let node = people[from] else {
            perform(director.cameHome(from))
            return
        }
        node.removeAction(forKey: "wander")
        endStroll(node)
        node.removeAction(forKey: "walk")
        node.body.removeAllActions()
        node.setRaised(false, art: art)
        node.setAsleep(false, palette: art.palette)
        node.isHidden = false
        let start = node.position
        let (spot, room) = destination(to: to, thread: thread)
        let out = TownLayout.path(from: start, in: node.room, to: spot, in: room)
        walkers.insert(from)
        movers += 1
        walk(node, along: out, speed: max(150, TownLayout.length(out) / 6)) { [weak self, weak node] in
            guard let self, let node else { return }
            node.body.texture = self.art.person(node.senator.runtime, .stand)
            let bubble = self.bubble(line, thread: thread)
            bubble.position = CGPoint(x: node.position.x, y: node.position.y + 46)
            bubble.setScale(0.6)
            bubble.alpha = 0
            self.world.addChild(bubble)
            bubble.run(.sequence([
                .group([.fadeIn(withDuration: 0.18), .scale(to: 1, duration: 0.18)]),
                .wait(forDuration: 2.6),
                .fadeOut(withDuration: 0.25),
                .removeFromParent(),
            ]))
            node.run(.sequence([.wait(forDuration: 3.0), .run { [weak self, weak node] in
                guard let self, let node else { return }
                let back = TownLayout.path(from: node.position, in: room, to: self.restSpot(node), in: node.room)
                self.walk(node, along: back, speed: max(150, TownLayout.length(back) / 6)) { [weak self, weak node] in
                    guard let self, let node else { return }
                    self.movers -= 1
                    self.walkers.remove(node.senator.id)
                    self.settle(node)
                    self.perform(self.director.cameHome(node.senator.id))
                }
            }]), withKey: "visit")
        }
    }

    private func restSpot(_ node: PersonNode) -> CGPoint {
        switch node.senator.mood {
        case .working, .waiting, .paused: CGPoint(x: node.home.x, y: node.home.y + 2)
        case .idle, .stopped: CGPoint(x: node.home.x, y: node.home.y - 24)
        }
    }

    /// Reduce Motion: a bubble over the sender fades into one over whom it
    /// is for. Nobody moves.
    private func fade(from: String, to: String?, thread: TownThreadRef, line: String) {
        guard let sender = people[from] else { return }
        let (spot, _) = destination(to: to, thread: thread)
        let first = bubble(line, thread: thread)
        first.position = CGPoint(x: sender.position.x, y: sender.position.y + 46)
        first.alpha = 0
        let second = bubble(line, thread: thread)
        second.position = CGPoint(x: spot.x + 24, y: spot.y + 52)
        second.alpha = 0
        world.addChild(first)
        world.addChild(second)
        movers += 1
        first.run(.sequence([.fadeIn(withDuration: 0.3), .wait(forDuration: 1.4), .fadeOut(withDuration: 0.5), .removeFromParent()]))
        second.run(.sequence([.wait(forDuration: 1.7), .fadeIn(withDuration: 0.5), .wait(forDuration: 1.8),
                              .fadeOut(withDuration: 0.4), .removeFromParent(), .run { [weak self] in self?.movers -= 1 }]))
    }

    /// Walk a path at `speed` points a second, legs going, then `done`.
    private func walk(
        _ node: PersonNode, along path: [CGPoint], speed: CGFloat, legs step: TimeInterval = 0.16,
        done: @escaping () -> Void
    ) {
        node.seated = false
        node.setLabel(node.senator.name, palette: art.palette, seated: false)
        var steps: [SKAction] = []
        for (a, b) in zip(path, path.dropFirst()) {
            let d = hypot(b.x - a.x, b.y - a.y)
            guard d > 0.5 else { continue }
            steps.append(.move(to: b, duration: TimeInterval(d / speed)))
        }
        let legs = SKAction.repeatForever(.animate(
            with: [art.person(node.senator.runtime, .walkA), art.person(node.senator.runtime, .walkB)],
            timePerFrame: step
        ))
        node.body.run(legs, withKey: "legs")
        node.run(.sequence(steps + [.run { [weak node] in node?.body.removeAction(forKey: "legs") }, .run(done)]),
                 withKey: "walk")
    }

    /// A speech bubble with the first words of what was said; a click on it
    /// opens the thread.
    private func bubble(_ text: String, thread: TownThreadRef) -> SKNode {
        let node = SKNode()
        node.name = "thread:\(thread.key)"
        node.zPosition = 60
        let words = label(text.isEmpty ? "…" : text, size: 11, color: art.palette.ink)
        let width = max(words.frame.width + 18, 34)
        let plate = SKShapeNode(rectOf: CGSize(width: width, height: 22), cornerRadius: 11)
        plate.fillColor = TownPalette.color(art.palette.bubble)
        plate.strokeColor = TownPalette.color(art.palette.streetEdge)
        plate.lineWidth = 1
        let tail = SKShapeNode(path: {
            let p = CGMutablePath()
            p.move(to: CGPoint(x: -5, y: -10.5))
            p.addLine(to: CGPoint(x: 0, y: -17))
            p.addLine(to: CGPoint(x: 5, y: -10.5))
            return p
        }())
        tail.fillColor = plate.fillColor
        tail.strokeColor = plate.fillColor
        node.addChild(tail)
        node.addChild(plate)
        node.addChild(words)
        return node
    }

    /// Names show when close enough to read them.
    private func updateLabels() {
        let show = cam.xScale < 1.5
        for node in people.values { node.nameLabel?.isHidden = !show }
    }

    private func clampCamera() {
        let b = layout.bounds.insetBy(dx: -200, dy: -200)
        cam.position.x = min(max(cam.position.x, b.minX), b.maxX)
        cam.position.y = min(max(cam.position.y, b.minY), b.maxY)
    }
}

/// One little person: a body sprite, and what it may wear or show (a
/// chair's hat, a raised hand, a sleeper's z, its name).
@MainActor
final class PersonNode: SKNode {
    var senator: TownSenator
    var home: CGPoint = .zero
    var room: CGRect = .zero
    var seated = false
    /// Out on a stroll about its room.
    var strolling = false
    let body = SKSpriteNode()
    private var hat: SKSpriteNode?
    private var raised: SKSpriteNode?
    private var zzz: SKLabelNode?
    private(set) var nameLabel: SKLabelNode?
    /// Its own seeded generator: the same wanders on every launch.
    private var seed: UInt64

    init(senator: TownSenator) {
        self.senator = senator
        seed = TownLayout.hash(senator.id) | 1
        super.init()
        body.anchorPoint = CGPoint(x: 0.5, y: 0)
        body.size = CGSize(width: 24, height: 34)
        addChild(body)
        // Big enough to read at the town's usual zoom.
        setScale(1.35)
    }

    required init?(coder: NSCoder) { fatalError("not from a coder") }

    /// SplitMix64, in [0, 1).
    func random() -> CGFloat {
        seed &+= 0x9E37_79B9_7F4A_7C15
        var z = seed
        z = (z ^ (z >> 30)) &* 0xBF58_476D_1CE4_E5B9
        z = (z ^ (z >> 27)) &* 0x94D0_49BB_1331_11EB
        z ^= z >> 31
        return CGFloat(z >> 11) / CGFloat(1 << 53)
    }

    func setChair(_ on: Bool, art: TownArt) {
        if on, hat == nil {
            let hat = TownScene.sprite(art.hat)
            hat.position = CGPoint(x: 0, y: 34)
            hat.zPosition = 0.01
            addChild(hat)
            self.hat = hat
        } else if !on {
            hat?.removeFromParent()
            hat = nil
        }
    }

    func setRaised(_ on: Bool, art: TownArt) {
        if on, raised == nil {
            let mark = TownScene.sprite(art.raisedHand)
            mark.position = CGPoint(x: 13, y: 40)
            mark.zPosition = 0.02
            addChild(mark)
            raised = mark
        } else if !on {
            raised?.removeFromParent()
            raised = nil
        }
    }

    func setAsleep(_ on: Bool, palette: TownPalette) {
        if on, zzz == nil {
            let z = SKLabelNode(text: "z z")
            z.fontName = "HelveticaNeue-Bold"
            z.fontSize = 9
            z.fontColor = TownPalette.color(palette.inkSoft)
            z.position = CGPoint(x: 13, y: 34)
            z.zPosition = 0.02
            addChild(z)
            zzz = z
        } else if !on {
            zzz?.removeFromParent()
            zzz = nil
        }
    }

    func setLabel(_ text: String, palette: TownPalette, seated: Bool) {
        let label = nameLabel ?? {
            let l = SKLabelNode()
            l.fontName = "HelveticaNeue-Medium"
            l.fontSize = 8.5
            l.verticalAlignmentMode = .top
            l.zPosition = 0.5
            addChild(l)
            nameLabel = l
            return l
        }()
        label.text = text.count > 16 ? String(text.prefix(15)) + "…" : text
        label.fontColor = TownPalette.color(palette.inkSoft)
        label.position = CGPoint(x: 0, y: seated ? -12 : -2)
    }
}
