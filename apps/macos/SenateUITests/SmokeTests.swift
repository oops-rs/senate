import AppKit
import XCTest

/// The app end to end against the core's fake daemon
/// (`scripts/ui-test.sh` starts it and passes its Senate home): the window
/// opens on the inbox, a thread opens and takes a reply, an approval is
/// allowed, a session opens from the sidebar with its conversation and its
/// prompt, a committee opens, ⌘1–⌘3 answer, the account popover opens, and nothing fails an
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
            // The conversation is a web page: its text is a value, not a label.
            .matching(NSPredicate(format: "label CONTAINS 'cargo publish -p refund' OR value CONTAINS 'cargo publish -p refund' OR title CONTAINS 'cargo publish -p refund'")).firstMatch
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
        // The account row opens its popover: Profile, Watch all, Settings…,
        // Refresh, About.
        let account = window.buttons["Account"].firstMatch
        XCTAssertTrue(account.waitForExistence(timeout: 5), "the account row is there")
        account.click()
        XCTAssertTrue(app.buttons["Watch all"].firstMatch.waitForExistence(timeout: 5), "Watch all is in its popover")
        XCTAssertTrue(app.buttons["Settings…"].firstMatch.exists, "and Settings")
        XCTAssertTrue(app.buttons["Profile"].firstMatch.exists, "and Profile")
        window.typeKey(.escape, modifierFlags: [])
    }

    func testSpeakerDeskShowsTheForwardingReceipt() throws {
        let window = app.windows.firstMatch
        XCTAssertTrue(window.waitForExistence(timeout: 10))
        let approvals = window.outlines.descendants(matching: .any)
            .matching(NSPredicate(format: "label == 'Approvals'")).firstMatch
        XCTAssertTrue(approvals.waitForExistence(timeout: 10))
        app.activate()
        approvals.click()
        let opened = XCTNSPredicateExpectation(predicate: NSPredicate(format: "title == 'Approvals'"), object: window)
        XCTAssertEqual(XCTWaiter.wait(for: [opened], timeout: 5), .completed)
        let entry = window.buttons["desk-open-refund-v2:60"].firstMatch
        XCTAssertTrue(entry.waitForExistence(timeout: 10))
        entry.click()
        let forward = window.buttons["desk-forward"].firstMatch
        XCTAssertTrue(forward.waitForExistence(timeout: 5))
        forward.click()
        let receipt = window.staticTexts["desk-outcome"].firstMatch
        XCTAssertTrue(receipt.waitForExistence(timeout: 5))
        let message = receipt.value as? String ?? receipt.label
        XCTAssertTrue(message.contains("ledger in payments") || message.contains("payments::senator::ledger"),
                      receipt.debugDescription)
        XCTAssertTrue(message.contains("#75"), receipt.debugDescription)
    }

    /// A link delivered by the system to the running app changes the page
    /// in the window that is open (no second process, no second window);
    /// one for what is not here shows the quiet page. A Debug build
    /// registers `senate-debug` (read as `senate`), so the installed app
    /// keeps `senate://`. Delivered with NSWorkspace to this build's app,
    /// as `senate open` or a browser would.
    func testLinksNavigateTheOpenWindow() throws {
        let window = app.windows.firstMatch
        XCTAssertTrue(window.waitForExistence(timeout: 10))
        // Beside the runner: Products/Debug/Senate.app.
        let bundle = Bundle.main.bundleURL.deletingLastPathComponent().appendingPathComponent("Senate.app")
        func running() -> [NSRunningApplication] {
            NSWorkspace.shared.runningApplications.filter { $0.bundleURL?.standardizedFileURL == bundle.standardizedFileURL }
        }
        XCTAssertEqual(running().count, 1)
        let pid = running().first?.processIdentifier
        func deliver(_ route: String) {
            let url = URL(string: "senate-debug://\(route)")!
            let configuration = NSWorkspace.OpenConfiguration()
            configuration.activates = false
            let done = expectation(description: route)
            NSWorkspace.shared.open([url], withApplicationAt: bundle, configuration: configuration) { _, error in
                XCTAssertNil(error)
                done.fulfill()
            }
            wait(for: [done], timeout: 10)
        }
        func shows(_ title: String) -> Bool {
            let shown = XCTNSPredicateExpectation(predicate: NSPredicate(format: "title == %@", title), object: window)
            return XCTWaiter.wait(for: [shown], timeout: 10) == .completed
        }
        deliver("open/v1/committee/payments")
        XCTAssertTrue(shows("payments"), window.title)
        deliver("open/v1/committee/gone")
        XCTAssertTrue(shows("Not available here"), window.title)
        // A burst: only its last link is shown.
        deliver("open/v1/committee/docs")
        deliver("open/v1/approvals")
        XCTAssertTrue(shows("Approvals"), window.title)
        // Rejected: nothing changes.
        deliver("intent/v1/reply/payments/1")
        Thread.sleep(forTimeInterval: 1)
        XCTAssertEqual(window.title, "Approvals")
        XCTAssertEqual(running().map(\.processIdentifier), [pid].compactMap { $0 }, "the same process")
        XCTAssertEqual(app.windows.count, 1, "no second window")
    }

    /// In the Dock (the default), Senate has a main menu: the View menu's
    /// sections, and Edit's standard items. Its structure is checked rather
    /// than clicked: the menu bar is shown only while Senate is frontmost,
    /// which a test runner hosted elsewhere cannot always make it.
    func testTheMainMenu() throws {
        XCTAssertTrue(app.windows.firstMatch.waitForExistence(timeout: 10))
        let view = app.menuBars.menuBarItems["View"].firstMatch
        XCTAssertTrue(view.waitForExistence(timeout: 5), "a View menu")
        for section in ["Inbox", "Approvals"] {
            XCTAssertTrue(view.menuItems[section].firstMatch.exists, "View › \(section)")
        }
        XCTAssertTrue(view.menuItems["Refresh"].firstMatch.exists, "View › Refresh")
        let edit = app.menuBars.menuBarItems["Edit"].firstMatch
        XCTAssertTrue(edit.exists, "an Edit menu")
        XCTAssertTrue(edit.menuItems["Copy"].firstMatch.exists, "Edit › Copy")
        XCTAssertTrue(edit.menuItems["Select All"].firstMatch.exists, "Edit › Select All")
    }

    /// A group with no name of its own that only wraps what SwiftUI or
    /// AppKit put there: the split view; the sidebar column's title and
    /// list; the detail column's page; a section header's menu.
    static func isSystemWrapper(_ element: XCUIElement) -> Bool {
        guard element.elementType == .group, element.label.isEmpty, element.identifier.isEmpty else { return false }
        let kinds = element.children(matching: .any).allElementsBoundByIndex.map(\.elementType)
        return kinds == [.splitGroup] || kinds == [.staticText, .scrollView, .button] || kinds == [.scrollView]
            || kinds == [.menuButton]
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
                // AppKit's Touch Bar item, which no view of ours provides.
                if issue.element?.elementType == .touchBar { return true }
                // The containers SwiftUI and AppKit put around ours, which no
                // modifier of ours reaches: the window's hosting group, the
                // split view's two columns, and a list section header's cell.
                if let element = issue.element, Self.isSystemWrapper(element) { return true }
                failed.append("\(issue.compactDescription): \(issue.element?.debugDescription ?? "no element")")
                return false
            }
        } catch {
            XCTFail("\(error.localizedDescription) \(failed.joined(separator: " | "))")
        }
        XCTAssertEqual(failed, [])
    }
}

extension SmokeTests {
    /// The pinned Secretary row: none yet, a first message starts it, and
    /// its session's page opens with the message sent.
    func testTheFirstMessageStartsTheSecretary() throws {
        let window = app.windows.firstMatch
        XCTAssertTrue(window.waitForExistence(timeout: 10))
        let row = window.outlines.descendants(matching: .any)
            .matching(NSPredicate(format: "label == 'Secretary'")).firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 10), "the pinned Secretary row")
        row.click()
        let page = XCTNSPredicateExpectation(predicate: NSPredicate(format: "title == 'Your secretary'"), object: window)
        XCTAssertEqual(XCTWaiter.wait(for: [page], timeout: 5), .completed, "its page, before it runs")
        let composer = window.textViews.firstMatch
        XCTAssertTrue(composer.waitForExistence(timeout: 5))
        composer.click()
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString("I'm working on the refund flow.", forType: .string)
        composer.typeKey("v", modifierFlags: .command)
        window.typeKey(.return, modifierFlags: .command)
        // Started and seated: its own session page, with Attach.
        XCTAssertTrue(window.buttons["Attach"].firstMatch.waitForExistence(timeout: 20), "the secretary's session page")
    }
}


extension SmokeTests {
    /// History: its ended sessions, a search that opens a match in its
    /// session, and a restore that brings that session back to life.
    func testHistorySearchAndRestore() throws {
        let window = app.windows.firstMatch
        XCTAssertTrue(window.waitForExistence(timeout: 10))
        let row = window.outlines.descendants(matching: .any).matching(NSPredicate(format: "label == 'History'")).firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 10), "the History row")
        app.activate()
        row.click()
        let past = window.descendants(matching: .any).matching(NSPredicate(format: "label == 'scout-1'")).firstMatch
        XCTAssertTrue(past.waitForExistence(timeout: 10), "an ended session listed")

        let field = window.textFields["Search past and live conversations"].firstMatch
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        field.click()
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString("ledger migration", forType: .string)
        field.typeKey("v", modifierFlags: .command)
        let hit = window.descendants(matching: .any).matching(NSPredicate(format: "label == 'mender-1'")).firstMatch
        XCTAssertTrue(hit.waitForExistence(timeout: 10), "a match in an ended session")
        hit.click()
        let opened = XCTNSPredicateExpectation(predicate: NSPredicate(format: "title == 'mender-1'"), object: window)
        XCTAssertEqual(XCTWaiter.wait(for: [opened], timeout: 10), .completed, "its session opened")

        let restore = window.buttons["Restore"].firstMatch
        XCTAssertTrue(restore.waitForExistence(timeout: 5), "Restore")
        restore.click()
        XCTAssertTrue(window.buttons["Attach"].firstMatch.waitForExistence(timeout: 15), "live again")
    }
}
