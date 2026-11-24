import Foundation

/// The town as a list (for VoiceOver, and for anyone who prefers it), read
/// from the same Observe data the scene gets: the snapshot kept current by
/// `state.patch` frames, and what happened since the page opened from the
/// domain events. Metadata only, as the scene: no message text.
@MainActor
@Observable
final class TownList {
    struct Senator: Identifiable, Hashable {
        let id: String
        let name: String
        let runtime: String
        let state: String
        let chair: Bool
        let link: String?
    }

    struct Committee: Identifiable, Hashable {
        var id: String
        var name: String
        var link: String?
        var senators: [Senator]
        /// Threads waiting on the person: their links.
        var waiting: [String]
    }

    struct Entry: Identifiable, Hashable {
        let id: Int
        let at: Date
        let text: String
        let link: String?
    }

    private(set) var committees: [Committee] = []
    private(set) var docket = 0
    private(set) var deskLink: String?
    private(set) var log: [Entry] = []
    @ObservationIgnored private var state: [String: Any] = [:]
    @ObservationIgnored private var counter = 0

    /// The keys each collection's rows are known by (as the published client's `applyFrame`).
    private static let keys: [String: [String]] = [
        "links": ["id"], "committees": ["id"], "projects": ["id"], "senators": ["committee_id", "id"],
        "retired": ["committee_id", "id"], "threads": ["committee_id", "root_seq"], "desk_items": ["committee_id", "root_seq"],
        "approvals": ["id"],
    ]

    func snapshot(_ snapshot: [String: Any]) {
        state = snapshot
        rebuild()
    }

    func frame(_ frame: [String: Any]) {
        guard let kind = frame["kind"] as? String else { return }
        let data = frame["data"] as? [String: Any] ?? [:]
        if kind == "state.patch" {
            apply(data)
            rebuild()
            return
        }
        note(kind, data, committee: frame["committee_id"] as? String, link: frame["link"] as? String)
    }

    private func apply(_ patch: [String: Any]) {
        let upsert = patch["upsert"] as? [String: Any] ?? [:]
        let remove = patch["remove"] as? [String: Any] ?? [:]
        for field in ["senate", "windows", "navigation"] {
            if let replace = patch["replace"] as? [String: Any], let value = replace[field] { state[field] = value }
        }
        for (field, keys) in Self.keys {
            let ups = upsert[field] as? [[String: Any]] ?? [], rms = remove[field] as? [[String: Any]] ?? []
            guard !ups.isEmpty || !rms.isEmpty else { continue }
            let key = { (row: [String: Any]) in keys.map { "\(row[$0] ?? "")" }.joined(separator: "\u{0}") }
            var rows = (state[field] as? [[String: Any]] ?? []).map { (key($0), $0) }
            let gone = Set(rms.map(key))
            rows.removeAll { gone.contains($0.0) }
            for row in ups {
                if let i = rows.firstIndex(where: { $0.0 == key(row) }) { rows[i] = (key(row), row) } else { rows.append((key(row), row)) }
            }
            state[field] = rows.map(\.1)
        }
    }

    private func rows(_ field: String) -> [[String: Any]] { state[field] as? [[String: Any]] ?? [] }

    private func name(of id: String?) -> String? {
        guard let id else { return nil }
        let s = rows("senators").first { $0["id"] as? String == id }
        return (s?["alias"] as? String) ?? (s == nil ? nil : id)
    }

    private func rebuild() {
        let senators = rows("senators")
        let threads = rows("threads")
        committees = rows("committees").filter { $0["archived"] as? Bool != true }.compactMap { c in
            guard let id = c["id"] as? String else { return nil }
            let chair = c["chair_id"] as? String
            let members: [Senator] = senators.filter { $0["committee_id"] as? String == id }.compactMap { s in
                guard let sid = s["id"] as? String else { return nil }
                return Senator(id: sid, name: (s["alias"] as? String) ?? sid, runtime: (s["runtime"] as? String) ?? "",
                               state: (s["state"] as? String) ?? "unknown", chair: sid == chair, link: s["link"] as? String)
            }.sorted { $0.chair && !$1.chair }
            let waiting = threads.filter { $0["committee_id"] as? String == id && $0["waits_on"] as? String == "person" }
                .map { ($0["link"] as? String) ?? "" }
            return Committee(id: id, name: (c["name"] as? String) ?? id, link: c["link"] as? String, senators: members, waiting: waiting)
        }
        docket = rows("desk_items").count
        deskLink = (state["navigation"] as? [String: Any])?["desk"] as? String
    }

    /// A domain event in words (who, never what was said).
    private func note(_ kind: String, _ d: [String: Any], committee: String?, link: String?) {
        let senator = { (ref: Any?) -> String? in
            guard let ref = ref as? [String: Any], ref["kind"] as? String == "senator" else { return nil }
            return self.name(of: ref["id"] as? String)
        }
        let text: String?
        switch kind {
        case "message.posted":
            let from = senator(d["from"]) ?? L("town.list.someone")
            let to = (d["to"] as? [Any])?.compactMap(senator).first ?? committee ?? ""
            text = L("town.list.wrote", from, to)
        case "senator.state_changed":
            text = name(of: d["senator_id"] as? String).map { L("town.now", $0, Self.label(d["state"] as? String)) }
        case "senator.joined":
            text = name(of: d["senator_id"] as? String).map { L("town.arrived", $0) }
        case "senator.left":
            text = name(of: d["senator_id"] as? String).map { L("town.left", $0) }
        case "approval.asked":
            text = name(of: d["senator_id"] as? String).map { L("town.list.asks", $0) }
        case "thread.state_changed" where d["state"] as? String == "archived":
            text = L("town.list.settled", committee ?? "")
        default:
            text = nil
        }
        guard let text else { return }
        counter += 1
        log.insert(Entry(id: counter, at: .now, text: text, link: link), at: 0)
        if log.count > 60 { log.removeLast(log.count - 60) }
    }

    static func label(_ state: String?) -> String {
        switch state {
        case "working": L("town.mood.working")
        case "idle": L("town.mood.idle")
        case "waiting_on_person": L("town.mood.waiting")
        case "paused": L("town.mood.paused")
        case "stopped", "away": L("town.mood.stopped")
        default: state ?? ""
        }
    }
}
