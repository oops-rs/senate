import XCTest

/// The app end to end against the core's fake daemon
/// (`scripts/ui-test.sh` starts it and passes its Senate home): the window
/// opens on the inbox, a thread opens and takes a reply, an approval is
/// allowed, the sections answer ⌘1–5, and nothing fails an accessibility
/// audit.
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

        // The inbox lists the fake daemon's threads; one opens beside it.
        let row = window.staticTexts["which env?"].firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 10), "the inbox shows the thread")
        row.click()
        let reply = window.textViews.firstMatch
        XCTAssertTrue(reply.waitForExistence(timeout: 5), "the reply box is there")
        reply.click()
        reply.typeText("staging, please")
        window.typeKey(.return, modifierFlags: .command)
        // Sent: the box empties.
        let emptied = XCTNSPredicateExpectation(predicate: NSPredicate(format: "value == ''"), object: reply)
        XCTAssertEqual(XCTWaiter.wait(for: [emptied], timeout: 10), .completed, "the reply was sent")

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

        // ⌘3 Sessions, ⌘5 Senates (the fake presides, so ⌘4 is the docket).
        window.typeKey("3", modifierFlags: .command)
        XCTAssertTrue(window.buttons["Attach to decide"].firstMatch.waitForExistence(timeout: 5))
        window.typeKey("5", modifierFlags: .command)
        XCTAssertTrue(window.staticTexts["Chairs"].firstMatch.waitForExistence(timeout: 5))

        let shot = XCTAttachment(screenshot: window.screenshot())
        shot.lifetime = .keepAlways
        add(shot)
    }

    func testAccessibilityAudit() throws {
        let window = app.windows.firstMatch
        XCTAssertTrue(window.waitForExistence(timeout: 10))
        XCTAssertTrue(window.staticTexts["which env?"].firstMatch.waitForExistence(timeout: 10))
        try app.performAccessibilityAudit(for: [.sufficientElementDescription, .hitRegion, .parentChild]) { issue in
            // SwiftUI's own split-view divider has no description.
            issue.element?.elementType == .splitter
        }
    }
}
