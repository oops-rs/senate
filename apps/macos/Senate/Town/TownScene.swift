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
    /// Names are shown at this zoom (they hide when it is too far to read).
    private var labelsShown = true
    /// Senators from elsewhere on a visit, by id: drawn, not clickable.
    private var guests: [String: PersonNode] = [:]
    /// Senators walking out of town: kept until they reach the edge.
    private var departing: Set<String> = []
    /// Bubbles showing now, and where (so the next one avoids them).
    private var bubbles: [(node: SKNode, rect: CGRect)] = []
    /// ✓ marks showing now (at most a few).
    private var checks = 0
    /// Where the signs, flags and visitors are: bubbles keep off them.
    private var signRects: [String: [CGRect]] = [:]
    private var signs: [CGRect] { signRects.values.flatMap { $0 } }

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
        // Whoever left without a walk out goes now.
        for (id, node) in people where state.senators[id] == nil && !departing.contains(id) {
            node.removeFromParent()
            people[id] = nil
            walkers.remove(id)
        }
        arrangeLabels()
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

    /// Zoom in on a house (or the hall) under a view point, gliding there.
    func focus(at viewPoint: CGPoint) {
        let room: CGRect
        switch hit(at: viewPoint) {
        case let .committee(name)?: guard let r = layout.room(name) else { return }; room = r
        case .hall?: room = layout.hall
        default: return
        }
        let scale = min(max(max(room.width * 1.7 / size.width, room.height * 1.9 / size.height), Self.minScale), Self.maxScale)
        let target = CGPoint(x: room.midX, y: room.midY + 10)
        fitted = true
        movers += 1
        let duration = reduceMotion ? 0 : 0.35
        let move = SKAction.move(to: target, duration: duration)
        move.timingMode = .easeInEaseOut
        let zoom = SKAction.scale(to: scale, duration: duration)
        zoom.timingMode = .easeInEaseOut
        cam.run(.sequence([.group([move, zoom]), .run { [weak self] in
            self?.movers -= 1
            self?.updateLabels()
        }]))
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
        for g in guests.values { g.zPosition = Self.depth(g.position.y) }
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
        departing = []
        guests = [:]
        bubbles = []
        checks = 0
        world.removeAllChildren()
        signRects = [:]
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
        signRects[text, default: []].append(CGRect(x: room.midX - width / 2, y: room.maxY + 4, width: width, height: 20))
        return node
    }

    private func drawHouse(_ committee: TownCommittee) {
        guard let room = layout.room(committee.name) else { return }
        houses[committee.name]?.removeFromParent()
        signRects[committee.name] = nil
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
            signRects[committee.name, default: []].append(CGRect(x: flag.position.x - 4, y: flag.position.y, width: 24, height: 30))
            // The visitor: someone come for the person, waiting at the door,
            // a "waiting" badge over its head and whom it waits on below.
            let visitor = SKNode()
            let figure = TownScene.sprite(art.person(.other, .stand, visitor: true))
            figure.anchorPoint = CGPoint(x: 0.5, y: 0)
            visitor.addChild(figure)
            let badge = TownScene.sprite(art.waitBadge)
            badge.position = CGPoint(x: 0, y: 42)
            badge.zPosition = 0.02
            visitor.addChild(badge)
            let whom = label(L("town.waits_on", state.person ?? L("town.you")), size: 8.5, bold: true,
                             color: art.palette.dark ? art.palette.attention : 0x8A4F00)
            whom.verticalAlignmentMode = .top
            whom.position = CGPoint(x: 0, y: -2)
            whom.zPosition = 0.5
            visitor.addChild(whom)
            visitor.setScale(1.35)
            let door = TownLayout.door(of: room)
            visitor.position = CGPoint(x: door.x + 34, y: door.y - 30)
            visitor.zPosition = Self.depth(visitor.position.y)
            visitor.name = "thread:\(first.key)"
            house.addChild(visitor)
            signRects[committee.name, default: []].append(CGRect(x: visitor.position.x - 22, y: visitor.position.y - 14, width: 44, height: 80))
        }
        world.addChild(house)
        houses[committee.name] = house
    }

    private func drawHall() {
        hallNode.removeAllChildren()
        signRects[L("town.hall")] = nil
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
        arrangeLabels()
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
            self.arrangeLabels()
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
            case let .arrive(id, walk): arrive(id, walk: walk)
            case let .depart(id, walk): depart(id, walk: walk)
            case let .check(thread, a, b):
                threads[thread.key] = thread
                check(thread, over: [a, b].compactMap { $0 })
            }
        }
    }

    /// Come into town from its nearer edge, along the street, in at the door.
    private func arrive(_ id: String, walk: Bool) {
        guard let node = people[id] else { return }
        settle(node)
        guard walk else {
            node.alpha = 0
            movers += 1
            node.run(.sequence([.fadeAlpha(to: node.senator.mood == .paused ? 0.85 : 1, duration: 0.5),
                                .run { [weak self] in self?.movers -= 1 }]))
            return
        }
        let rest = restSpot(node)
        node.removeAllActions()
        node.body.removeAllActions()
        let path = TownLayout.pathFromEdge(layout.edgeX(for: node.room), to: rest, in: node.room)
        node.position = path[0]
        node.isHidden = false
        walkers.insert(id)
        movers += 1
        self.walk(node, along: path, speed: max(150, TownLayout.length(path) / 6)) { [weak self, weak node] in
            guard let self, let node else { return }
            self.movers -= 1
            self.walkers.remove(id)
            self.settle(node)
            self.arrangeLabels()
            self.perform(self.director.cameHome(id))
        }
    }

    /// Leave town: out of the door and along the street to the edge; then
    /// gone, or (a stopped session) hidden at home.
    private func depart(_ id: String, walk: Bool) {
        guard let node = people[id] else { return }
        let finish = { [weak self, weak node] in
            guard let self, let node else { return }
            self.departing.remove(id)
            self.walkers.remove(id)
            if self.state.senators[id] == nil {
                node.removeFromParent()
                self.people[id] = nil
            } else {
                self.settle(node)
            }
            self.arrangeLabels()
        }
        // Someone not in sight (stopped already) has nothing to walk.
        guard walk, !node.isHidden || node.alpha > 0 else {
            if walk { perform(director.cameHome(id)) }
            departing.insert(id)
            movers += 1
            node.run(.sequence([.fadeOut(withDuration: 0.5), .run { [weak self] in
                self?.movers -= 1
                finish()
            }]))
            return
        }
        node.removeAllActions()
        node.body.removeAllActions()
        endStroll(node)
        node.isHidden = false
        node.alpha = 1
        node.setRaised(false, art: art)
        node.setAsleep(false, palette: art.palette)
        let path = Array(TownLayout.pathFromEdge(layout.edgeX(for: node.room), to: node.position, in: node.room).reversed())
        departing.insert(id)
        walkers.insert(id)
        movers += 1
        self.walk(node, along: path, speed: max(150, TownLayout.length(path) / 6)) { [weak self] in
            guard let self else { return }
            self.movers -= 1
            finish()
            self.perform(self.director.cameHome(id))
        }
    }

    /// A ✓ over each of the two for a moment: the thread is settled.
    private func check(_ thread: TownThreadRef, over ids: [String]) {
        for id in ids {
            guard checks < 6, let node = people[id], !node.isHidden else { continue }
            let mark = SKNode()
            mark.name = "thread:\(thread.key)"
            mark.zPosition = 58
            let disc = SKShapeNode(circleOfRadius: 9)
            disc.fillColor = TownPalette.color(art.palette.glow)
            disc.strokeColor = .clear
            let tick = label("✓", size: 12, bold: true, color: art.palette.dark ? 0x10241A : 0xFFFFFF)
            mark.addChild(disc)
            mark.addChild(tick)
            mark.position = CGPoint(x: node.position.x + 16, y: node.position.y + 58)
            mark.alpha = 0
            world.addChild(mark)
            checks += 1
            movers += 1
            mark.run(.sequence([
                .group([.fadeIn(withDuration: 0.2), reduceMotion ? .wait(forDuration: 0.2) : .moveBy(x: 0, y: 6, duration: 0.2)]),
                .wait(forDuration: 1.4), .fadeOut(withDuration: 0.3), .removeFromParent(),
                .run { [weak self] in
                    self?.checks -= 1
                    self?.movers -= 1
                },
            ]))
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

    /// A senator who does not live in town: in from the nearer edge to whom
    /// it speaks, a bubble, and out again.
    private func guestVisit(from: String, to: String?, thread: TownThreadRef, line: String) {
        let (spot, room) = destination(to: to, thread: thread)
        let guest = PersonNode(senator: TownSenator(id: from, name: from, runtime: TownRuntime(guessing: from), mood: .idle))
        guest.body.texture = art.person(guest.senator.runtime, .stand)
        guest.setLabel(from, palette: art.palette, seated: false)
        guest.nameLabel?.isHidden = true
        let path = TownLayout.pathFromEdge(layout.edgeX(for: room), to: spot, in: room)
        guest.position = path[0]
        guest.room = room
        world.addChild(guest)
        guests[from] = guest
        movers += 1
        walk(guest, along: path, speed: max(150, TownLayout.length(path) / 6)) { [weak self, weak guest] in
            guard let self, let guest else { return }
            guest.zPosition = Self.depth(guest.position.y)
            guest.body.texture = self.art.person(guest.senator.runtime, .stand)
            self.showBubble(line, thread: thread, over: guest, hold: 2.6)
            guest.run(.sequence([.wait(forDuration: 3.0), .run { [weak self, weak guest] in
                guard let self, let guest else { return }
                self.walk(guest, along: path.reversed(), speed: max(150, TownLayout.length(path) / 6)) { [weak self, weak guest] in
                    guard let self else { return }
                    guest?.removeFromParent()
                    self.guests[from] = nil
                    self.movers -= 1
                    self.perform(self.director.cameHome(from))
                }
            }]))
        }
    }

    private func startWalk(from: String, to: String?, thread: TownThreadRef, line: String) {
        guard let node = people[from] else {
            guestVisit(from: from, to: to, thread: thread, line: line)
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
        node.nameLabel?.isHidden = true
        let out = TownLayout.path(from: start, in: node.room, to: spot, in: room)
        walkers.insert(from)
        movers += 1
        walk(node, along: out, speed: max(150, TownLayout.length(out) / 6)) { [weak self, weak node] in
            guard let self, let node else { return }
            node.body.texture = self.art.person(node.senator.runtime, .stand)
            self.showBubble(line, thread: thread, over: node, hold: 2.6)
            node.run(.sequence([.wait(forDuration: 3.0), .run { [weak self, weak node] in
                guard let self, let node else { return }
                let back = TownLayout.path(from: node.position, in: room, to: self.restSpot(node), in: node.room)
                self.walk(node, along: back, speed: max(150, TownLayout.length(back) / 6)) { [weak self, weak node] in
                    guard let self, let node else { return }
                    self.movers -= 1
                    self.walkers.remove(node.senator.id)
                    self.settle(node)
                    self.arrangeLabels()
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
        if let sender = people[from] { showBubble(line, thread: thread, over: sender, hold: 1.4) }
        let target = to.flatMap { people[$0] }
        movers += 1
        run(.sequence([.wait(forDuration: 1.7), .run { [weak self] in
            guard let self else { return }
            if let target {
                self.showBubble(line, thread: thread, over: target, hold: 1.8)
            } else {
                let (spot, _) = self.destination(to: nil, thread: thread)
                self.showBubble(line, thread: thread, head: CGPoint(x: spot.x, y: spot.y + 20), exclude: nil, hold: 1.8)
            }
            self.movers -= 1
        }]))
    }

    /// The part of the scene on screen now.
    var viewport: CGRect {
        CGRect(x: cam.position.x - size.width / 2 * cam.xScale, y: cam.position.y - size.height / 2 * cam.yScale,
               width: size.width * cam.xScale, height: size.height * cam.yScale)
    }

    private func showBubble(_ text: String, thread: TownThreadRef, over node: PersonNode, hold: TimeInterval) {
        let head = CGPoint(x: node.position.x, y: node.position.y + 34 * node.xScale + 2)
        showBubble(text, thread: thread, head: head, exclude: node.senator.id, hold: hold)
    }

    /// A bubble above `head`, placed to cover no one else's face or name
    /// and no other bubble, inside the view; its tail points at the head.
    private func showBubble(_ text: String, thread: TownThreadRef, head: CGPoint, exclude: String?, hold: TimeInterval) {
        bubbles.removeAll { $0.node.parent == nil }
        // Readable when zoomed out: a bubble grows with the zoom.
        let scale = min(max(cam.xScale * 0.85, 1), 2.4)
        let words = label(text.isEmpty ? "…" : text, size: 11, color: art.palette.ink)
        let local = CGSize(width: max(words.frame.width + 18, 34), height: 22)
        let size = CGSize(width: local.width * scale, height: local.height * scale)
        var obstacles = bubbles.map(\.rect)
        for (id, p) in people where id != exclude && !p.isHidden {
            obstacles.append(p.calculateAccumulatedFrame())
        }
        // Houses' signs and flags, and the visitors at their doors: kept
        // clear when there is room, but faces first.
        let placed = TownPlacement.bubble(size: size, head: head, obstacles: obstacles, soft: signs, viewport: viewport)
        let node = bubble(words, size: local, tip: CGPoint(x: (placed.tail.x - placed.centre.x) / scale,
                                                          y: (placed.tail.y - placed.centre.y) / scale),
                          thread: thread)
        node.position = placed.centre
        node.setScale(scale * 0.6)
        node.alpha = 0
        world.addChild(node)
        let rect = CGRect(x: placed.centre.x - size.width / 2, y: placed.centre.y - size.height / 2,
                          width: size.width, height: size.height)
        bubbles.append((node, rect))
        node.run(.sequence([
            .group([.fadeIn(withDuration: 0.18), .scale(to: scale, duration: 0.18)]),
            .wait(forDuration: hold),
            .fadeOut(withDuration: 0.25),
            .removeFromParent(),
        ]))
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
    /// `tip` is where the tail points, from the bubble's centre, unscaled.
    private func bubble(_ words: SKLabelNode, size: CGSize, tip: CGPoint, thread: TownThreadRef) -> SKNode {
        let node = SKNode()
        node.name = "thread:\(thread.key)"
        node.zPosition = 60
        let plate = SKShapeNode(rectOf: size, cornerRadius: size.height / 2)
        plate.fillColor = TownPalette.color(art.palette.bubble)
        plate.strokeColor = TownPalette.color(art.palette.dark ? art.palette.inkSoft : art.palette.streetEdge, alpha: art.palette.dark ? 0.5 : 1)
        plate.lineWidth = 1
        // The tail: from the bottom edge, as near the head as the round
        // ends allow, to just short of the head.
        let baseX = min(max(tip.x, -size.width / 2 + 12), size.width / 2 - 12)
        let end = CGPoint(x: tip.x, y: min(tip.y + 3, -size.height / 2 - 4))
        let tail = SKShapeNode(path: {
            let p = CGMutablePath()
            p.move(to: CGPoint(x: baseX - 5, y: -size.height / 2 + 1))
            p.addLine(to: end)
            p.addLine(to: CGPoint(x: baseX + 5, y: -size.height / 2 + 1))
            p.closeSubpath()
            return p
        }())
        tail.fillColor = plate.fillColor
        tail.strokeColor = plate.strokeColor
        tail.lineWidth = 1
        node.addChild(tail)
        node.addChild(plate)
        // The plate's fill covers the tail's stroke where they join.
        let seam = SKShapeNode(rectOf: CGSize(width: 9, height: 2))
        seam.fillColor = plate.fillColor
        seam.strokeColor = .clear
        seam.position = CGPoint(x: baseX, y: -size.height / 2 + 1.2)
        node.addChild(seam)
        node.addChild(words)
        return node
    }

    /// Names show when close enough to read them.
    private func updateLabels() {
        arrangeLabels()
    }

    /// Names under people, none over another: the chair's and those who
    /// wait on a person keep their place, others step down a line or hide.
    /// Walkers' names hide while they walk.
    private func arrangeLabels() {
        let show = cam.xScale < 1.5
        guard show else {
            // Too far to read: hidden, nothing to arrange.
            if labelsShown {
                for node in people.values { node.nameLabel?.isHidden = true }
                labelsShown = false
            }
            return
        }
        labelsShown = true
        var labels: [TownPlacement.Label] = []
        for (id, node) in people {
            guard let label = node.nameLabel else { continue }
            label.position.y = node.labelBaseY
            guard show, !node.isHidden, !walkers.contains(id) || node.strolling else {
                label.isHidden = true
                continue
            }
            let f = node.labelFrame
            let s = node.xScale
            let rect = CGRect(x: node.position.x + f.minX * s, y: node.position.y + f.minY * s,
                              width: f.width * s, height: f.height * s)
            let priority = node.senator.chair ? 3 : node.senator.mood == .waiting ? 2 : node.senator.mood == .working ? 1 : 0
            labels.append(.init(id: id, rect: rect, priority: priority))
        }
        let step: CGFloat = 11 * 1.35
        for (id, shift) in TownPlacement.labels(labels, step: step) {
            guard let node = people[id], let label = node.nameLabel else { continue }
            if let shift {
                label.isHidden = false
                label.position.y = node.labelBaseY - shift / node.xScale
            } else {
                label.isHidden = true
            }
        }
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
        labelBaseY = seated ? -12 : -2
        label.position = CGPoint(x: 0, y: labelBaseY)
        if label.text != measuredText {
            measuredText = label.text
            measuredSize = label.frame.size
        }
    }

    /// The name's frame at its unmoved place, measured once per text.
    var labelFrame: CGRect {
        CGRect(x: -measuredSize.width / 2, y: labelBaseY - measuredSize.height, width: measuredSize.width, height: measuredSize.height)
    }

    private var measuredText: String?
    private var measuredSize: CGSize = .zero

    /// Where the name sits before any collision moves it.
    private(set) var labelBaseY: CGFloat = -2
}
