import Foundation
import Testing
@testable import Senate
import SenateCore

/// The Town shell's guards (security review 2: M2, L1, L3, L4, L5).
@MainActor
@Suite struct TownShellTests {
    let core = FakeCore()
    let defaults: UserDefaults

    init() {
        let suite = "senate-town-tests-\(UUID().uuidString)"
        defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
    }

    func store() async throws -> CoreStore {
        let core = self.core
        let store = CoreStore(makeCore: { core }, defaults: defaults, notifications: false)
        core.push(online([]))
        for _ in 0..<200 where !store.state.online { try await Task.sleep(for: .milliseconds(5)) }
        return store
    }

    // M2: the bearer goes only to a numeric loopback address with a port, nothing else in it.
    @Test func theBearerGoesOnlyToALoopbackEndpoint() {
        for good in ["http://127.0.0.1:7199", "http://127.0.0.1:7199/", "http://[::1]:7199"] {
            #expect(ObserveClient.vouched(good) != nil, "\(good)")
        }
        for bad in ["http://localhost:7199", "https://127.0.0.1:7199", "http://127.0.0.1", "http://10.0.0.2:7199",
                    "http://evil.example:7199", "http://127.0.0.1:7199/v1", "http://u:p@127.0.0.1:7199",
                    "http://127.0.0.1:7199/?x=1", "http://127.0.0.1.evil.example:7199", "file:///tmp/x"] {
            #expect(ObserveClient.vouched(bad) == nil, "\(bad)")
        }
    }

    // L3: a grant planted in the Keychain is not used unless the daemon issued it to Town.
    @Test func aPlantedGrantIsIgnored() {
        let live = ObserveGrantInfo(id: "g1", name: "Town", expiresAt: "2099-01-01T00:00:00Z")
        let mine = ObserveAccess.Grant(token: "obs1_x", id: "g1", installation: "this-mac", expiresAt: "2099-01-01T00:00:00Z")
        #expect(ObserveAccess.vouched(mine, grants: [live], installation: "this-mac"))
        var planted = mine
        planted.id = "unknown"
        #expect(!ObserveAccess.vouched(planted, grants: [live], installation: "this-mac"))
        #expect(!ObserveAccess.vouched(mine, grants: [live], installation: "another-mac"))
        #expect(!ObserveAccess.vouched(mine, grants: [ObserveGrantInfo(id: "g1", name: "scene", expiresAt: "2099-01-01T00:00:00Z")], installation: "this-mac"))
        var old = mine
        old.expiresAt = "2000-01-01T00:00:00Z"
        #expect(!ObserveAccess.vouched(old, grants: [live], installation: "this-mac"))
        var junk = mine
        junk.expiresAt = "not a date"
        #expect(junk.expired)
    }

    // L4: what the page stores is plain JSON; NaN, infinities and dates are refused, not serialised.
    @Test func thePageStoresOnlyPlainJSON() {
        #expect(TownWebPage.plainJSON(["calm": true, "n": 1.5, "s": "x", "a": [1, NSNull()]] as [String: Any]))
        #expect(!TownWebPage.plainJSON(Double.nan))
        #expect(!TownWebPage.plainJSON(Double.infinity))
        #expect(!TownWebPage.plainJSON(Date()))
        #expect(!TownWebPage.plainJSON(["x": [Double.nan]] as [String: Any]))
        var deep: Any = 1
        for _ in 0..<40 { deep = [deep] }
        #expect(!TownWebPage.plainJSON(deep))
    }

    // L5: a new grant revokes Town's earlier ones (one live Town grant per Senate), and leaves other grants alone.
    @Test func aNewGrantRevokesTheOldOnes() async throws {
        let store = try await store()
        core.observeGrantList = [
            ObserveGrantInfo(id: "old-1", name: "Town", expiresAt: "2099-01-01T00:00:00Z"),
            ObserveGrantInfo(id: "old-2", name: "Town", expiresAt: "2099-01-01T00:00:00Z"),
            ObserveGrantInfo(id: "scene", name: "my scene", expiresAt: "2099-01-01T00:00:00Z"),
            ObserveGrantInfo(id: "fake", name: "Town", expiresAt: "2099-01-01T00:00:00Z"),
        ]
        let page = TownWebPage(store: store)
        try await page.grantAccess()
        #expect(core.calls.contains("observe revoke old-1"))
        #expect(core.calls.contains("observe revoke old-2"))
        #expect(!core.calls.contains("observe revoke scene"))
        #expect(!core.calls.contains("observe revoke fake"))
        // Forgetting revokes it too (and clears the Keychain item this test made).
        page.forgetAccess()
        for _ in 0..<200 where !core.calls.contains("observe revoke fake") { try await Task.sleep(for: .milliseconds(5)) }
        #expect(core.calls.contains("observe revoke fake"))
    }
}
