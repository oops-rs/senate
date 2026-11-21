import Foundation
import SenateCore

/// Runs a Codex command line from the core in a new Terminal.app window.
enum Terminal {
    struct Failure: LocalizedError {
        let message: String
        var errorDescription: String? { message }
    }

    /// macOS asks once to let Senate control Terminal.
    static func run(_ command: TerminalCommand) throws {
        try run(script: command.appleScript)
    }

    /// A shell line of the app's own (the log stream).
    static func run(shell line: String) throws {
        let escaped = line.replacingOccurrences(of: "\\", with: "\\\\")
            .replacingOccurrences(of: "\"", with: "\\\"")
        try run(script: "tell application \"Terminal\"\n  activate\n  do script \"\(escaped)\"\nend tell")
    }

    private static func run(script source: String) throws {
        guard let script = NSAppleScript(source: source) else {
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
