import AppKit
import Foundation
import os

/// Signposts around the work that grows with the inbox (Instruments' Points
/// of Interest shows them); with `SENATE_APP_PERF=1` each is also printed
/// with its duration, for a quick measure against the fake daemon's
/// `FAKE_THREADS`.
enum Perf {
    nonisolated static let signposter = OSSignposter(subsystem: Log.subsystem, category: .pointsOfInterest)
    nonisolated static let printing = ProcessInfo.processInfo.environment["SENATE_APP_PERF"] == "1"

    static func measure<T>(_ name: StaticString, _ detail: @autoclosure () -> String = "", _ body: () throws -> T) rethrows -> T {
        let state = signposter.beginInterval(name)
        let start = ContinuousClock.now
        defer {
            signposter.endInterval(name, state)
            if printing {
                let ms = (ContinuousClock.now - start).components.attoseconds / 1_000_000_000_000_000
                report("\(name) \(detail()) \(ms) ms")
            }
        }
        return try body()
    }

    static func measure<T>(_ name: StaticString, _ detail: @autoclosure () -> String = "", _ body: () async throws -> T) async rethrows -> T {
        let state = signposter.beginInterval(name)
        let start = ContinuousClock.now
        defer {
            signposter.endInterval(name, state)
            if printing {
                let ms = (ContinuousClock.now - start).components.attoseconds / 1_000_000_000_000_000
                report("\(name) \(detail()) \(ms) ms")
            }
        }
        return try await body()
    }
}

extension Perf {
    /// With `SENATE_APP_PERF=1`: how late a 50 ms tick on the main actor
    /// runs, the worst every two seconds: rendering and layout included, so
    /// a stall anywhere on the main thread shows.
    static func watchMainThread() {
        guard printing else { return }
        Task { @MainActor in
            var worst: Duration = .zero
            var since = ContinuousClock.now
            while !Task.isCancelled {
                let asked = ContinuousClock.now
                try? await Task.sleep(for: .milliseconds(50))
                worst = max(worst, ContinuousClock.now - asked - .milliseconds(50))
                if ContinuousClock.now - since > .seconds(2) {
                    let ms = worst.components.attoseconds / 1_000_000_000_000_000 + worst.components.seconds * 1000
                    report("main-thread worst stall \(ms) ms")
                    worst = .zero
                    since = .now
                }
            }
        }
    }
}

extension Perf {
    /// A measure line, to stderr (unbuffered: the run may be cut short).
    nonisolated static func report(_ line: String) {
        FileHandle.standardError.write(Data("perf: \(line)\n".utf8))
    }

    /// With `SENATE_APP_PERF=1`: once online, open the window, open the
    /// thread `SENATE_APP_PERF_THREAD` (else the first), Inbox, Approvals and
    /// the first session
    /// for two seconds each, and quit. Any build, so Release can be measured.
    static func tour(_ store: CoreStore) {
        guard printing else { return }
        Task {
            for _ in 0..<50 where !store.state.online {
                try? await Task.sleep(for: .milliseconds(200))
            }
            store.show(.inbox)
            try? await Task.sleep(for: .seconds(1))
            let root = ProcessInfo.processInfo.environment["SENATE_APP_PERF_THREAD"]
            if let key = store.state.inbox.first(where: { root == String($0.root) })?.key
                ?? store.state.inbox.first?.key {
                report("-- thread")
                store.place = .thread(key)
                try? await Task.sleep(for: .seconds(2))
            }
            for section in store.sections {
                report("-- \(section.rawValue)")
                store.place = .section(section)
                try? await Task.sleep(for: .seconds(2))
            }
            if let session = store.shownSessions.first {
                report("-- session")
                store.place = .session(session.sessionId)
                try? await Task.sleep(for: .seconds(2))
            }
            report("-- inbox again")
            store.place = .section(.inbox)
            try? await Task.sleep(for: .seconds(2))
            NSApplication.shared.terminate(nil)
        }
    }
}

// MARK: The session page, measured

extension Perf {
    nonisolated static let sessionTourOn = ProcessInfo.processInfo.environment["SENATE_APP_PERF"] == "session"
    /// Posted with a row's id to scroll the open conversation to it.
    static let scrollToRow = Notification.Name("SenatePerfScrollToRow")
    /// Posted with a tool call's id to open it.
    static let openCall = Notification.Name("SenatePerfOpenCall")

    /// Frames and stalls over a phase: every frame's interval from the
    /// display link, and how late a 4 ms tick on the main actor runs.
    @MainActor final class Probe: NSObject {
        private var link: CADisplayLink?
        private var last: CFTimeInterval = 0
        private(set) var frames = 0
        private(set) var dropped = 0
        private(set) var worstFrame: CFTimeInterval = 0
        private(set) var worstStall: Duration = .zero
        private var ticking: Task<Void, Never>?
        /// Watch updates merged during the phase, and the time they took.
        static var updates = 0
        static var updateTime: Duration = .zero
        private var updates: Int { Probe.updates }
        private var updateTime: Duration { Probe.updateTime }

        func start(in view: NSView) {
            link = view.displayLink(target: self, selector: #selector(frame(_:)))
            link?.add(to: .main, forMode: .common)
            ticking = Task { @MainActor [weak self] in
                while !Task.isCancelled {
                    let asked = ContinuousClock.now
                    try? await Task.sleep(for: .milliseconds(4))
                    guard let self else { return }
                    self.worstStall = max(self.worstStall, ContinuousClock.now - asked - .milliseconds(4))
                }
            }
        }

        @objc private func frame(_ link: CADisplayLink) {
            let now = link.timestamp
            if last > 0 {
                let interval = now - last
                frames += 1
                worstFrame = max(worstFrame, interval)
                // Longer than one and a half 60 Hz frames: a hitch a person
                // sees (the refresh rate adapts, so its own target is no
                // measure of a dropped frame).
                if interval > 0.025 { dropped += 1 }
            }
            last = now
        }

        func stop(_ phase: String) {
            link?.invalidate()
            ticking?.cancel()
            let c = MarkdownCache.shared.counters
            MarkdownCache.shared.counters = (0, .zero, 0, .zero)
            let ms = { (d: Duration) in d.components.seconds * 1000 + d.components.attoseconds / 1_000_000_000_000_000 }
            report("phase \(phase): main-thread markdown renders \(c.renders) (\(ms(c.renderTime)) ms), "
                + "height measures \(c.measures) (\(ms(c.measureTime)) ms), timeline updates \(updates) (\(ms(updateTime)) ms)")
            Probe.updates = 0
            Probe.updateTime = .zero
            let stall = worstStall.components.attoseconds / 1_000_000_000_000_000 + worstStall.components.seconds * 1000
            report(String(format: "phase %@: frames %d, over 25 ms %d, worst frame %.1f ms, worst stall %lld ms",
                          phase, frames, dropped, worstFrame * 1000, stall))
        }
    }

    /// With `SENATE_APP_PERF=session` and the fake daemon's `FAKE_BIG`
    /// session (`marathon`, growing every `FAKE_TIMELINE_EVERY_MS`): open
    /// it, scroll up through it at speed (older pages read on the way) and
    /// down again, read in the middle while it updates, open a 64 KiB call,
    /// switch between two sessions; print each phase's frames and stalls,
    /// and quit.
    static func sessionTour(_ store: CoreStore) {
        guard sessionTourOn else { return }
        Task { @MainActor in
            for _ in 0..<100 where store.session("marathon-1") == nil {
                try? await Task.sleep(for: .milliseconds(200))
            }
            guard store.session("marathon-1") != nil,
                  let view = NSApplication.shared.windows.first(where: { $0.isVisible && $0.frame.width > 600 })?.contentView
            else {
                report("no marathon session (run the fake daemon with FAKE_BIG=3000)")
                NSApplication.shared.terminate(nil)
                return
            }
            @MainActor func phase(_ name: String, _ body: @MainActor () async -> Void) async {
                let probe = Probe()
                probe.start(in: view)
                await body()
                probe.stop(name)
            }
            try? await Task.sleep(for: .seconds(1))
            await phase("open") {
                store.place = .session("marathon-1")
                try? await Task.sleep(for: .seconds(2.5))
            }
            let rows = { store.timeline?.rows.map(\.id) ?? [] }
            await phase("scroll up") {
                var i = rows().count - 1
                for _ in 0..<400 {
                    let ids = rows()
                    i = min(max(i - 1, 0), ids.count - 1)
                    NotificationCenter.default.post(name: scrollToRow, object: ids[i])
                    try? await Task.sleep(for: .milliseconds(12))
                }
            }
            await phase("scroll down") {
                var ids = rows()
                var i = 0
                for _ in 0..<400 {
                    ids = rows()
                    i = min(i + 1, ids.count - 1)
                    NotificationCenter.default.post(name: scrollToRow, object: ids[i])
                    try? await Task.sleep(for: .milliseconds(12))
                }
            }
            await phase("read while it updates") {
                let ids = rows()
                NotificationCenter.default.post(name: scrollToRow, object: ids[ids.count / 2])
                try? await Task.sleep(for: .seconds(4))
            }
            await phase("open a 64 KiB call") {
                let big = store.timeline?.rows.first { row in
                    if case let .tool(call) = row.kind { return (call.output?.utf8.count ?? 0) > 60_000 }
                    return false
                }
                if let big, case let .tool(call) = big.kind {
                    NotificationCenter.default.post(name: scrollToRow, object: big.id)
                    try? await Task.sleep(for: .milliseconds(500))
                    NotificationCenter.default.post(name: openCall, object: call.callId)
                }
                try? await Task.sleep(for: .seconds(1.5))
            }
            await phase("switch sessions") {
                for _ in 0..<3 {
                    store.place = .session("s-claude")
                    try? await Task.sleep(for: .seconds(1))
                    store.place = .session("marathon-1")
                    try? await Task.sleep(for: .seconds(1))
                }
            }
            NSApplication.shared.terminate(nil)
        }
    }
}
