import Foundation
import os

/// The app's unified-log subsystem. The Rust core logs under the same one
/// (category `core`), so `log stream --predicate 'subsystem == "dev.senate.app"'`
/// or Console shows both.
enum Log {
    nonisolated static let subsystem = "dev.senate.app"
    nonisolated static let app = Logger(subsystem: subsystem, category: "app")
}
