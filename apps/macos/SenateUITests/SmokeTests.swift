import XCTest

/// The app end to end against the core's fake daemon
/// (`scripts/ui-test.sh` starts it and passes its Senate home): the window
/// opens on home with the waiting threads in the sidebar, a thread opens
/// and takes a reply, an approval is allowed, a committee opens, the
/// sections answer ⌘1–6, and nothing fails an accessibility audit.
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

    func testInboxReplyApprovalsAndSections() throws {
        let window = app.windows.firstMatch
        XCTAssertTrue(window.waitForExistence(timeout: 10), "the window opens")

        // The sidebar lists the fake daemon's threads; one opens beside it.
        let row = window.outlines.staticTexts["which env?"].firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 10), "the sidebar shows the thread")
        row.click()
        let reply = window.textViews.firstMatch
        XCTAssertTrue(reply.waitForExistence(timeout: 5), "the reply box is there")
        reply.click()
        reply.typeText("staging, please")
        window.typeKey(.return, modifierFlags: .command)
        // Sent: answered, the thread no longer waits on you and leaves the sidebar.
        let gone = XCTNSPredicateExpectation(predicate: NSPredicate(format: "exists == false"), object: row)
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

        // ⌘3 Committees: one card per committee; one opens on its roster.
        window.typeKey("3", modifierFlags: .command)
        let payments = window.buttons.matching(NSPredicate(format: "label BEGINSWITH 'payments'")).firstMatch
        XCTAssertTrue(payments.waitForExistence(timeout: 5), "the committees are listed")
        payments.click()
        XCTAssertTrue(window.buttons["All committees"].firstMatch.waitForExistence(timeout: 5), "a committee opens")
        // ⌘4 Sessions, ⌘6 Senates (the fake presides, so ⌘5 is the docket).
        window.typeKey("4", modifierFlags: .command)
        XCTAssertTrue(window.buttons["Attach to decide"].firstMatch.waitForExistence(timeout: 5))
        window.typeKey("6", modifierFlags: .command)
        XCTAssertTrue(window.staticTexts["Address"].firstMatch.waitForExistence(timeout: 5), "the Senates section shows")
    }

    func testAccessibilityAudit() throws {
        let window = app.windows.firstMatch
        XCTAssertTrue(window.waitForExistence(timeout: 10))
        XCTAssertTrue(window.staticTexts["which env?"].firstMatch.waitForExistence(timeout: 10))
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
