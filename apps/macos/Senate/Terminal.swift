import Foundation
import SenateCore

/// Runs a Codex command line from the core in a new Terminal.app window.
enum Terminal {
    struct Failure: LocalizedError {
        let message: String
        var errorDescription: String? { message }
    }

    /// macOS asks once to let Senate control Terminal.
    @MainActor
    static func run(_ command: TerminalCommand) throws {
        guard let script = NSAppleScript(source: command.appleScript) else {
            throw Failure(message: L("terminal.no_script"))
        }
        var error: NSDictionary?
        script.executeAndReturnError(&error)
        if let error {
            let reason = error[NSAppleScript.errorMessage] as? String ?? "\(error)"
            throw Failure(message: L("terminal.failed", reason))
        }
    }
}
