import AppKit
import QuartzCore
import SwiftUI

/// A working session's mark: the runtime glyph's own frame carries the
/// progress (a segment of its outline travelling round it, in the accent,
/// over a faint full outline) with the letter inside. The frames are the
/// geometry `scripts/make-runtime-icons.swift` draws (rounded square for
/// Claude Code and the terminal, circle for Codex, square for Antigravity,
/// whose filled form turns outlined while it works); the letters are its
/// `Runtime…Letter` glyphs. Same size as the still mark, so nothing moves
/// when a session starts or stops working.
struct WorkingRuntimeMark: View {
    let runtime: String
    var side: CGFloat = 14
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        ZStack {
            RuntimeFrame(runtime: runtime, travelling: !reduceMotion)
            Image(Self.letter(runtime))
                .renderingMode(.template)
                .resizable()
                .interpolation(.high)
                .foregroundStyle(Theme.primary)
        }
        .frame(width: side, height: side)
        .help(runtime)
    }

    static func letter(_ runtime: String) -> String {
        RuntimeMark.asset(runtime) + "Letter"
    }
}

/// The frame, drawn by Core Animation: the travelling segment runs in the
/// render server (no timer, no work on the main thread), pauses while the
/// window is hidden, and goes when the row does.
private struct RuntimeFrame: NSViewRepresentable {
    let runtime: String
    let travelling: Bool

    func makeNSView(context: Context) -> RuntimeFrameView { RuntimeFrameView() }

    func updateNSView(_ view: RuntimeFrameView, context: Context) {
        view.configure(shape: RuntimeFrameView.Shape(runtime), travelling: travelling)
    }
}

final class RuntimeFrameView: NSView {
    enum Shape: Equatable {
        case roundedSquare, circle, square

        init(_ runtime: String) {
            switch runtime {
            case "codex": self = .circle
            case "agy": self = .square
            default: self = .roundedSquare
            }
        }

        /// In the script's 16-point box. The square is the outline of
        /// Antigravity's filled square, inset by half the stroke.
        var path: CGPath {
            switch self {
            case .roundedSquare:
                CGPath(roundedRect: CGRect(x: 0.75, y: 0.75, width: 14.5, height: 14.5),
                       cornerWidth: 3.5, cornerHeight: 3.5, transform: nil)
            case .circle:
                CGPath(ellipseIn: CGRect(x: 0.75, y: 0.75, width: 14.5, height: 14.5), transform: nil)
            case .square:
                CGPath(roundedRect: CGRect(x: 0.65, y: 0.65, width: 14.7, height: 14.7),
                       cornerWidth: 3.4, cornerHeight: 3.4, transform: nil)
            }
        }
    }

    /// One lap, and how much of the outline the segment covers.
    static let period: CFTimeInterval = 1.6
    static let segment: CGFloat = 0.28

    private let track = CAShapeLayer()
    private let moving = CAShapeLayer()
    private var shape: Shape?
    private var travelling = true
    private var occlusion: NSObjectProtocol?

    init() {
        super.init(frame: .zero)
        wantsLayer = true
        for l in [track, moving] {
            l.fillColor = nil
            l.lineCap = .round
            l.lineJoin = .round
            layer?.addSublayer(l)
        }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not from a nib") }

    func configure(shape: Shape, travelling: Bool) {
        guard shape != self.shape || travelling != self.travelling else { return }
        self.shape = shape
        self.travelling = travelling
        needsLayout = true
        colours()
    }

    override func layout() {
        super.layout()
        guard let shape else { return }
        let scale = bounds.width / 16
        var t = CGAffineTransform(scaleX: scale, y: scale)
        let lap = shape.path.copy(using: &t) ?? shape.path
        // Two laps in one path: the segment runs from the first lap into
        // the second and starts again where the second began, so it never
        // jumps at the seam.
        let twice = CGMutablePath()
        twice.addPath(lap)
        twice.addPath(lap)
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        for l in [track, moving] {
            l.frame = bounds
            l.lineWidth = 1.3 * scale
        }
        track.path = lap
        moving.path = twice
        CATransaction.commit()
        animate()
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        colours()
    }

    private func colours() {
        effectiveAppearance.performAsCurrentDrawingAppearance {
            let accent = NSColor(Theme.primary).cgColor
            CATransaction.begin()
            CATransaction.setDisableActions(true)
            // Reduce Motion: the whole outline in the accent, still.
            track.strokeColor = travelling ? NSColor(Theme.primary).withAlphaComponent(0.28).cgColor : accent
            moving.strokeColor = accent
            moving.isHidden = !travelling
            CATransaction.commit()
        }
    }

    private func animate() {
        moving.removeAllAnimations()
        guard travelling else { return }
        let length = Self.segment / 2
        #if DEBUG
        // Snapshots: the segment held at a phase of the lap (0 to 1).
        if let phase = ProcessInfo.processInfo.environment["SENATE_APP_WORKING_PHASE"].flatMap(Double.init) {
            moving.strokeStart = CGFloat(phase) / 2
            moving.strokeEnd = CGFloat(phase) / 2 + length
            return
        }
        #endif
        moving.strokeStart = 0
        moving.strokeEnd = length
        let start = CABasicAnimation(keyPath: "strokeStart")
        start.fromValue = 0
        start.toValue = 0.5
        let end = CABasicAnimation(keyPath: "strokeEnd")
        end.fromValue = length
        end.toValue = 0.5 + length
        let lap = CAAnimationGroup()
        lap.animations = [start, end]
        lap.duration = Self.period
        lap.repeatCount = .infinity
        lap.timingFunction = CAMediaTimingFunction(name: .linear)
        lap.isRemovedOnCompletion = false
        moving.add(lap, forKey: "travel")
    }

    // MARK: Paused while the window is hidden

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        if let occlusion { NotificationCenter.default.removeObserver(occlusion) }
        occlusion = nil
        guard let window else { return }
        occlusion = NotificationCenter.default.addObserver(
            forName: NSWindow.didChangeOcclusionStateNotification, object: window, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.followOcclusion() }
        }
        followOcclusion()
    }

    private func followOcclusion() {
        guard let window, let layer = moving as CALayer? else { return }
        let visible = window.occlusionState.contains(.visible)
        if visible, layer.speed == 0 {
            let paused = layer.timeOffset
            layer.speed = 1
            layer.timeOffset = 0
            layer.beginTime = 0
            layer.beginTime = layer.convertTime(CACurrentMediaTime(), from: nil) - paused
        } else if !visible, layer.speed != 0 {
            let now = layer.convertTime(CACurrentMediaTime(), from: nil)
            layer.speed = 0
            layer.timeOffset = now
        }
    }

    isolated deinit {
        if let occlusion { NotificationCenter.default.removeObserver(occlusion) }
    }
}
