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
