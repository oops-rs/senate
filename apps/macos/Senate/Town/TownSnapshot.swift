#if DEBUG
import AppKit

/// Development only: with `SENATE_APP_SNAPSHOT=<dir>` and
/// `SENATE_APP_SNAPSHOT_TOWN=1`, picture the Town page and quit: the town
/// as it opens, the scripted visits (`TownScript.busy`) mid-walk and
/// mid-conversation, and the accessible list. With
/// `SENATE_APP_TOWN_DEMO=1` the town is the scripted one, the same every
/// time; without it, the store's (the fake daemon's).
enum TownSnapshot {
    static let env = "SENATE_APP_SNAPSHOT_TOWN"

    static func run(_ store: CoreStore, shot: (String) async -> Void, grab: (String) async -> Void) async {
        if TownWebPage.enabled { return await works(store, shot: shot, grab: grab) }
        let large = ProcessInfo.processInfo.environment[TownScript.env] == "large"
        store.select(.town)
        await shot("town-quiet")
        // SENATE_APP_TOWN_HOLD=<s>: stay on the quiet town that long (to
        // measure what a still town costs), then go on.
        if let hold = ProcessInfo.processInfo.environment["SENATE_APP_TOWN_HOLD"].flatMap(Double.init) {
            let web = TownWebPage.shown.map { ($0.web.value(forKey: "_webProcessIdentifier") as? Int) ?? 0 } ?? 0
            print("snapshot: holding the quiet town \(hold) s app=\(ProcessInfo.processInfo.processIdentifier) web=\(web)")
            try? await Task.sleep(for: .seconds(hold))
        }
        if TownScript.requested == nil {
            // The live feed: the fake daemon's trickle (an event every
            // 1.2 s after the watch opens), pictured as it plays.
            for i in 1...4 { await shot("town-live-\(i)") }
        }
        if TownScript.requested != nil {
            NotificationCenter.default.post(name: TownModel.playScript, object: large ? "large" : nil)
            // `shot` waits 1.5 s: in-house visits are talking, the others
            // still walking.
            try? await Task.sleep(for: .milliseconds(300))
            await shot("town-busy")
            try? await Task.sleep(for: .seconds(2))
            await shot("town-busy-later")
        }
        if let page = TownWebPage.shown {
            let stats = await page.evaluate("return window.town.stats()")
            print("snapshot: town page \(String(describing: stats))")
            // A hover card, then a click through to the senator's session.
            _ = await page.evaluate("return window.town.probe(false)")
            await shot("town-hover")
            _ = await page.evaluate("return window.town.probe(true)")
            await shot("town-clicked")
            store.select(.town)
            try? await Task.sleep(for: .seconds(1))
        }
        NotificationCenter.default.post(name: TownModel.showListNote, object: true)
        await shot("town-list")
        NotificationCenter.default.post(name: TownModel.showListNote, object: nil)
    }

    /// The web Town (the Works): the town, a run of frames as the scripted
    /// day plays (visits, workers, a held delivery, the gate, a committee
    /// built), the construction, a house's inside, the Speaker's house, a
    /// hover card and the list. `SENATE_APP_TOWN_FRAMES=<n>` frames (default 40).
    static func works(_ store: CoreStore, shot: (String) async -> Void, grab: (String) async -> Void) async {
        store.select(.town)
        for _ in 0..<100 where TownWebPage.shown?.ready != true { try? await Task.sleep(for: .milliseconds(100)) }
        guard let page = TownWebPage.shown else { return }
        // Pictures by day in light, by night in dark, whatever the clock.
        let dark = ProcessInfo.processInfo.environment["SENATE_APP_SNAPSHOT_APPEARANCE"] == "dark"
        _ = await page.evaluate("window.town.setOptions({ time: t }); return true", ["t": dark ? "night" : "day"])
        // SENATE_APP_TOWN_GRANT=1 (live, against the fake daemon): picture the
        // one-click access, click it, and forget the grant again at the end.
        let granting = ProcessInfo.processInfo.environment["SENATE_APP_TOWN_GRANT"] == "1" && TownWebPage.demo == nil
        if granting {
            for _ in 0..<30 where !page.accessNeeded { try? await Task.sleep(for: .milliseconds(100)) }
            await shot("works-access")
            do { try await page.grantAccess() } catch { print("snapshot: grant failed: \(error)") }
            try? await Task.sleep(for: .seconds(2))
        }
        defer { if granting { page.forgetAccess() } }
        await shot("works-town")
        // SENATE_APP_TOWN_HOLD=<s>: stay on the whole town that long, the world running (to measure what it costs).
        if let hold = ProcessInfo.processInfo.environment["SENATE_APP_TOWN_HOLD"].flatMap(Double.init) {
            // SENATE_APP_TOWN_HOLD_ZOOM=<z>: hold close up on the plaza instead of the whole town.
            if let zoom = ProcessInfo.processInfo.environment["SENATE_APP_TOWN_HOLD_ZOOM"].flatMap(Double.init) {
                _ = await page.evaluate("return window.town.look('hall', z)", ["z": zoom])
            }
            let web = (page.web.value(forKey: "_webProcessIdentifier") as? Int) ?? 0
            print("snapshot: holding the quiet town \(hold) s app=\(ProcessInfo.processInfo.processIdentifier) web=\(web)")
            try? await Task.sleep(for: .seconds(hold))
        }
        // SENATE_APP_TOWN_PHASES=<s>: what the town costs in each state, `<s>` seconds each:
        // in front (key), visible behind another window, on another page, minimised.
        if let each = ProcessInfo.processInfo.environment["SENATE_APP_TOWN_PHASES"].flatMap(Double.init) {
            let web = (page.web.value(forKey: "_webProcessIdentifier") as? Int) ?? 0
            let ids = "app=\(ProcessInfo.processInfo.processIdentifier) web=\(web)"
            let window = page.web.window
            window?.makeKeyAndOrderFront(nil)
            NSApp.activate(ignoringOtherApps: true)
            print("snapshot: phase key \(ids)")
            try? await Task.sleep(for: .seconds(each)); print("snapshot: stats key \(String(describing: await page.evaluate("return window.town.stats()")))")
            // A small window of our own takes the key; the town stays fully visible.
            let other = NSPanel(contentRect: NSRect(x: 0, y: 0, width: 160, height: 80), styleMask: [.titled], backing: .buffered, defer: false)
            other.setFrameOrigin(NSPoint(x: (NSScreen.main?.visibleFrame.maxX ?? 800) - 170, y: (NSScreen.main?.visibleFrame.minY ?? 0) + 10))
            other.becomesKeyOnlyIfNeeded = false
            other.makeKeyAndOrderFront(nil)
            print("snapshot: phase notkey \(ids) key=\(window?.isKeyWindow == true)")
            try? await Task.sleep(for: .seconds(each)); print("snapshot: stats notkey \(String(describing: await page.evaluate("return window.town.stats()")))")
            other.close()
            window?.makeKeyAndOrderFront(nil)
            store.select(.section(.inbox))
            print("snapshot: phase hidden \(ids)")
            try? await Task.sleep(for: .seconds(each))
            store.select(.town)
            try? await Task.sleep(for: .seconds(2))
            window?.miniaturize(nil)
            print("snapshot: phase minimised \(ids)")
            try? await Task.sleep(for: .seconds(each))
            window?.deminiaturize(nil)
            print("snapshot: phase done")
            return
        }
        // SENATE_APP_TOWN_WALK=<n>: close-ups at about ten frames a second (people walking).
        if let walk = ProcessInfo.processInfo.environment["SENATE_APP_TOWN_WALK"].flatMap(Int.init) {
            for (name, place, zoom) in [("walk-ring", "hall", 2.0), ("walk-promenade", "memory", 2.6)] {
                _ = await page.evaluate("return window.town.look(w, z)", ["w": place, "z": zoom])
                try? await Task.sleep(for: .milliseconds(400))
                for i in 0..<walk {
                    let start = ContinuousClock.now
                    await grab(String(format: "\(name)-%03d", i))
                    let left = Duration.milliseconds(100) - (ContinuousClock.now - start)
                    if left > .zero { try? await Task.sleep(for: left) }
                }
            }
            _ = await page.evaluate("return window.town.fit()")
        }
        let frames = ProcessInfo.processInfo.environment["SENATE_APP_TOWN_FRAMES"].flatMap(Int.init) ?? 40
        for i in 0..<frames {
            let start = ContinuousClock.now
            await grab(String(format: "frame-%03d", i))
            let left = Duration.milliseconds(300) - (ContinuousClock.now - start)
            if left > .zero { try? await Task.sleep(for: left) }
        }
        // The committee built while you watch (the script adds one after 12 s).
        func constructing() async -> Int {
            ((await page.evaluate("return window.town.stats()") as? [String: Any])?["constructing"] as? Int) ?? 0
        }
        for _ in 0..<300 where await constructing() == 0 { try? await Task.sleep(for: .milliseconds(100)) }
        try? await Task.sleep(for: .milliseconds(1200))
        await grab("works-construction-1")
        try? await Task.sleep(for: .milliseconds(1800))
        await grab("works-construction-2")
        try? await Task.sleep(for: .milliseconds(1300))
        await grab("works-construction-3")
        for _ in 0..<100 where await constructing() > 0 { try? await Task.sleep(for: .milliseconds(100)) }
        await shot("works-built")
        _ = await page.evaluate("return window.town.look('speaker', 1.6)")
        await shot("works-speaker")
        _ = await page.evaluate("return window.town.look('quarters', 1.2)")
        await shot("works-monuments")
        _ = await page.evaluate("return window.town.fit()")
        _ = await page.evaluate("return window.town.probe(false)")
        await shot("works-hover")
        // A click through: the page asks to open the senator's link; the app's router takes it.
        _ = await page.evaluate("return window.town.probe(true)")
        try? await Task.sleep(for: .milliseconds(800))
        await shot("works-clicked")
        print("snapshot: routed \(store.links.routed)")
        store.select(.town)
        try? await Task.sleep(for: .seconds(1))
        _ = await page.evaluate("return window.town.enter('senate')")
        await shot("works-inside")
        for i in 0..<12 {
            await grab(String(format: "inside-%03d", i))
            try? await Task.sleep(for: .milliseconds(250))
        }
        _ = await page.evaluate("return window.town.leave()")
        NotificationCenter.default.post(name: TownModel.showListNote, object: true)
        await shot("works-list")
        NotificationCenter.default.post(name: TownModel.showListNote, object: nil)
    }
}
#endif
