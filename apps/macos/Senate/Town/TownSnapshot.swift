#if DEBUG
import Foundation

/// Development only: with `SENATE_APP_SNAPSHOT=<dir>` and
/// `SENATE_APP_SNAPSHOT_TOWN=1`, picture the Town page and quit: the town
/// as it opens, the scripted visits (`TownScript.busy`) mid-walk and
/// mid-conversation, and the accessible list. With
/// `SENATE_APP_TOWN_DEMO=1` the town is the scripted one, the same every
/// time; without it, the store's (the fake daemon's).
enum TownSnapshot {
    static let env = "SENATE_APP_SNAPSHOT_TOWN"

    static func run(_ store: CoreStore, shot: (String) async -> Void) async {
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
}
#endif
