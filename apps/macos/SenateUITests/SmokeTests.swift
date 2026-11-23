import AppKit
import XCTest

/// The app end to end against the core's fake daemon
/// (`scripts/ui-test.sh` starts it and passes its Senate home): the window
/// opens on the inbox, a thread opens and takes a reply, an approval is
/// allowed, a session opens from the sidebar with its conversation and its
/// prompt, a committee opens, ⌘1–⌘3 answer, the account row's menu opens, and nothing fails an
/// accessibility audit.
final class SmokeTests: XCTestCase {
    var app: XCUIApplication!

    override func setUpWithError() throws {
        continueAfterFailure = false
        let home = ProcessInfo.processInfo.environment["SENATE_UI_HOME"] ?? ""
        try XCTSkipIf(home.isEmpty, "run through scripts/ui-test.sh, which starts the fake daemon")
        app = XCUIApplication()
        app.launchEnvironment = [
            "SENATE_HOME": home,
            "SENATE_APP_SHOW": "1",
            "SENATE_APP_NOTIFICATIONS": "0",
        ]
        app.launchArguments = ["-AppleLanguages", "(en)"]
        app.launch()
    }

    override func tearDown() {
        app?.terminate()
    }

    func testInboxReplyApprovalsSessionsAndCommittees() throws {
        let window = app.windows.firstMatch
        XCTAssertTrue(window.waitForExistence(timeout: 10), "the window opens")

        // The inbox lists the fake daemon's threads; one opens.
        let row = window.buttons["which env?"].firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 10), "the inbox shows the thread")
        row.click()
        let reply = window.textViews.firstMatch
        XCTAssertTrue(reply.waitForExistence(timeout: 5), "the reply box is there")
        reply.click()
        // Pasted, not typed: an input method (pinyin, say) would turn typed
        // letters into candidates.
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString("staging, please", forType: .string)
        reply.typeKey("v", modifierFlags: .command)
        window.typeKey(.return, modifierFlags: .command)
        // Sent: answered, the thread no longer waits on you and leaves the inbox.
        window.typeKey("1", modifierFlags: .command)
        let gone = XCTNSPredicateExpectation(
            predicate: NSPredicate(format: "exists == false"), object: window.buttons["which env?"].firstMatch)
        XCTAssertEqual(XCTWaiter.wait(for: [gone], timeout: 10), .completed, "the reply was sent")

        // ⌘2: Approvals. Allowing the Codex request takes it off the list.
        window.typeKey("2", modifierFlags: .command)
        let allow = window.buttons["Allow"].firstMatch
        XCTAssertTrue(allow.waitForExistence(timeout: 5), "the approval is listed")
        let before = window.buttons.matching(identifier: "Allow").count
        allow.click()
        let fewer = XCTNSPredicateExpectation(
            predicate: NSPredicate(format: "count < %d", before),
            object: window.buttons.matching(identifier: "Allow")
        )
        XCTAssertEqual(XCTWaiter.wait(for: [fewer], timeout: 10), .completed, "the approval was answered")

        // A session from the sidebar: its conversation, its prompt, the composer.
        let lead = window.outlines.descendants(matching: .any)
            .matching(NSPredicate(format: "label == 'lead'")).firstMatch
        XCTAssertTrue(lead.waitForExistence(timeout: 5), "the sidebar lists the sessions")
        lead.click()
        XCTAssertTrue(window.buttons["Attach to decide"].firstMatch.waitForExistence(timeout: 5), "its prompt waits")
        XCTAssertTrue(window.descendants(matching: .any)
            .matching(NSPredicate(format: "label CONTAINS 'cargo publish -p refund'")).firstMatch
            .waitForExistence(timeout: 5), "its conversation shows")
        XCTAssertTrue(window.textViews["Message"].firstMatch.exists, "the composer is there")

        // A committee from the sidebar: its sessions and members.
        window.outlines.descendants(matching: .any)
            .matching(NSPredicate(format: "label BEGINSWITH 'payments'")).firstMatch.click()
        let opened = XCTNSPredicateExpectation(predicate: NSPredicate(format: "title == 'payments'"), object: window)
        XCTAssertEqual(XCTWaiter.wait(for: [opened], timeout: 5), .completed, "a committee opens")
        XCTAssertTrue(window.descendants(matching: .any)
            .matching(NSPredicate(format: "label == 'ledger'")).count >= 2, "with its sessions on this Mac")

        // ⌘3: your profile (the Senates you sit in), also the account row.
        window.typeKey("3", modifierFlags: .command)
        XCTAssertTrue(window.staticTexts["Address"].firstMatch.waitForExistence(timeout: 5), "the profile shows")
        // The menu beside the account row: Watch all, Settings, About, Refresh.
        let more = window.menuButtons["More"].firstMatch
        XCTAssertTrue(more.waitForExistence(timeout: 5), "the account row's menu is there")
        more.click()
        XCTAssertTrue(app.menuItems["Watch all"].firstMatch.waitForExistence(timeout: 5), "Watch all is in it")
        XCTAssertTrue(app.menuItems["Settings…"].firstMatch.exists, "and Settings")
        window.typeKey(.escape, modifierFlags: [])
    }

    func testAccessibilityAudit() throws {
        let window = app.windows.firstMatch
        XCTAssertTrue(window.waitForExistence(timeout: 10))
        XCTAssertTrue(window.buttons["which env?"].firstMatch.waitForExistence(timeout: 10))
        // Which elements failed: the audit's own failure only names the kind of
        // issue, so go on past it to list them.
        continueAfterFailure = true
        var failed: [String] = []
        do {
            try app.performAccessibilityAudit(for: [.sufficientElementDescription, .hitRegion, .parentChild]) { issue in
                // SwiftUI's own split-view divider has no description.
                if issue.element?.elementType == .splitter { return true }
                failed.append("\(issue.compactDescription): \(issue.element?.debugDescription ?? "no element")")
                return false
            }
        } catch {
            XCTFail("\(error.localizedDescription) \(failed.joined(separator: " | "))")
        }
        XCTAssertEqual(failed, [])
    }
}
