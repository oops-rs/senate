import Foundation
import MetricKit
import os

/// Crash, hang and exception reports the system collects for the app
/// (MetricKit), written to the unified log when they arrive (the next
/// launch after one). The system's own reports stay in
/// ~/Library/Logs/DiagnosticReports; nothing leaves the machine.
final class Diagnostics: NSObject, MXMetricManagerSubscriber {
    static let shared = Diagnostics()
    private let log = Logger(subsystem: Log.subsystem, category: "diagnostics")

    func start() {
        MXMetricManager.shared.add(self)
    }

    nonisolated func didReceive(_ payloads: [MXDiagnosticPayload]) {
        let log = Logger(subsystem: Log.subsystem, category: "diagnostics")
        for payload in payloads {
            let crashes = payload.crashDiagnostics?.count ?? 0
            let hangs = payload.hangDiagnostics?.count ?? 0
            let json = String(decoding: payload.jsonRepresentation(), as: UTF8.self)
            log.fault("diagnostics: \(crashes) crash(es), \(hangs) hang(s): \(json, privacy: .public)")
        }
    }
}
