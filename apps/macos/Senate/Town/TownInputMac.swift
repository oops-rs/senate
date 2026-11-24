#if os(macOS)
import AppKit
import SwiftUI

/// The mouse wheel and the trackpad's two fingers, which SwiftUI does not
/// hand to a view that is not a scroll view: scrolling pans the town, and
/// with ⌘ (or a wheel's ⌥) zooms it at the pointer. macOS only; the scene
/// itself never sees an event.
struct TownScrollCatcher: NSViewRepresentable {
    /// A pan step in view points (y down), or a zoom factor at a point.
    var pan: (CGSize) -> Void
    var zoom: (CGFloat, CGPoint) -> Void
    /// A scroll started or ended (full frame rate meanwhile).
    var interacting: (Bool) -> Void

    func makeNSView(context: Context) -> CatcherView {
        let view = CatcherView()
        view.handlers = self
        return view
    }

    func updateNSView(_ view: CatcherView, context: Context) {
        view.handlers = self
    }

    final class CatcherView: NSView {
        var handlers: TownScrollCatcher?
        private var monitor: Any?
        private var settle: DispatchWorkItem?

        // Clicks go to the views above.
        override func hitTest(_ point: NSPoint) -> NSView? { nil }

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            if let monitor { NSEvent.removeMonitor(monitor) }
            monitor = nil
            guard window != nil else { return }
            monitor = NSEvent.addLocalMonitorForEvents(matching: .scrollWheel) { [weak self] event in
                guard let self, let handlers = self.handlers, event.window === self.window else { return event }
                let local = self.convert(event.locationInWindow, from: nil)
                guard self.bounds.contains(local) else { return event }
                let point = CGPoint(x: local.x, y: self.isFlipped ? local.y : self.bounds.height - local.y)
                handlers.interacting(true)
                let precise = event.hasPreciseScrollingDeltas
                let dx = event.scrollingDeltaX * (precise ? 1 : 8)
                let dy = event.scrollingDeltaY * (precise ? 1 : 8)
                if event.modifierFlags.contains(.command) || event.modifierFlags.contains(.option) {
                    handlers.zoom(exp(dy * 0.01), point)
                } else {
                    handlers.pan(CGSize(width: dx, height: dy))
                }
                self.settle?.cancel()
                let done = DispatchWorkItem { handlers.interacting(false) }
                self.settle = done
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.25, execute: done)
                return nil
            }
        }

        override func removeFromSuperview() {
            if let monitor { NSEvent.removeMonitor(monitor) }
            monitor = nil
            super.removeFromSuperview()
        }
    }
}
#endif
