import Foundation

/// A scene's `manifest.json` (docs/observe.md, "Scene host contract"),
/// validated strictly: exactly the contract's fields, each of its type and
/// within bounds, no duplicate keys, nothing else. A scene is code from
/// somewhere else; its manifest is read as untrusted input, and its name is
/// only a label (a scene is known by the identity the app gives it when it
/// is installed, never by what it calls itself).
nonisolated struct SceneManifest: Equatable, Sendable {
    /// The scene host contract this app speaks.
    static let hostVersion = 1
    /// The Observe major this app's client reads.
    static let observeMajor = 1
    static let maxBytes = 16 * 1024

    var name: String
    var version: String
    /// A normalised bundle-relative path to an `.html` file.
    var entry: String
    var observeMajor: Int
    var required: [String]
    var optional: [String]
    var cardReads: [String]
    var cardWrites: [String]
    var minHostVersion: Int

    /// Every Observe capability it names, required first.
    var capabilities: [String] { required + optional }
    /// It asks to see this Senate's direct links (a separate permission).
    var wantsTopology: Bool { capabilities.contains("topology") }

    enum Problem: Error, Equatable, CustomStringConvertible {
        case tooLarge
        case notJSON
        case duplicateKey(String)
        case unknownField(String)
        case missingField(String)
        case invalid(String)
        case needsNewerApp(Int)

        var description: String {
            switch self {
            case .tooLarge: L("scenes.problem.manifest_size")
            case .notJSON: L("scenes.problem.manifest_json")
            case let .duplicateKey(key): L("scenes.problem.duplicate", key)
            case let .unknownField(key): L("scenes.problem.unknown", key)
            case let .missingField(key): L("scenes.problem.missing", key)
            case let .invalid(key): L("scenes.problem.invalid", key)
            case let .needsNewerApp(version): L("scenes.problem.newer", Int64(version))
            }
        }
    }

    static func parse(_ data: Data) throws(Problem) -> SceneManifest {
        guard data.count <= maxBytes else { throw .tooLarge }
        do {
            try StrictJSON.check(data)
        } catch let StrictJSON.Failure.duplicate(key) {
            throw .duplicateKey(String(key.prefix(64)))
        } catch {
            throw .notJSON
        }
        guard let root = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else { throw .notJSON }
        try exactly(root, ["name", "version", "entry", "observe", "cards", "min_host_version"], at: "")

        guard let name = root["name"] as? String, validName(name) else { throw .invalid("name") }
        guard let version = root["version"] as? String, matches(version, versionPattern) else { throw .invalid("version") }
        guard let entry = root["entry"] as? String, entry.utf8.count <= 256, ScenePath.validRelative(entry),
              entry.lowercased().hasSuffix(".html")
        else { throw .invalid("entry") }

        guard let observe = root["observe"] as? [String: Any] else { throw .invalid("observe") }
        try exactly(observe, ["major", "required", "optional"], at: "observe.")
        guard let major = integer(observe["major"]), major == observeMajor else { throw .invalid("observe.major") }
        let required = try list(observe["required"], "observe.required", max: 32, capabilityPattern)
        let optional = try list(observe["optional"], "observe.optional", max: 32, capabilityPattern)
        guard Set(required).isDisjoint(with: optional) else { throw .invalid("observe.optional") }

        guard let cards = root["cards"] as? [String: Any] else { throw .invalid("cards") }
        try exactly(cards, ["reads", "writes"], at: "cards.")
        let reads = try list(cards["reads"], "cards.reads", max: 16, schemaPattern)
        let writes = try list(cards["writes"], "cards.writes", max: 16, schemaPattern)

        guard let minHost = integer(root["min_host_version"]), minHost >= 1 else { throw .invalid("min_host_version") }
        guard minHost <= hostVersion else { throw .needsNewerApp(minHost) }

        return SceneManifest(name: name, version: version, entry: entry, observeMajor: major, required: required,
                             optional: optional, cardReads: reads, cardWrites: writes, minHostVersion: minHost)
    }

    // MARK: Rules

    static let versionPattern = #"^(0|[1-9][0-9]{0,5})\.(0|[1-9][0-9]{0,5})\.(0|[1-9][0-9]{0,5})(-[0-9A-Za-z.-]{1,32})?$"#
    static let capabilityPattern = #"^[a-z][a-z0-9_]{0,47}$"#
    /// `town.look/v1`: dotted lowercase words, a slash, a version.
    static let schemaPattern = #"^[a-z][a-z0-9-]{0,31}(\.[a-z][a-z0-9-]{0,31}){0,3}/v[1-9][0-9]{0,2}$"#

    /// 1–64 characters, no surrounding space, nothing invisible or
    /// direction-changing (it is shown in the app's own UI and to the daemon).
    static func validName(_ name: String) -> Bool {
        let count = name.unicodeScalars.count
        guard count >= 1, count <= 64, name.trimmingCharacters(in: .whitespacesAndNewlines) == name else { return false }
        return name.unicodeScalars.allSatisfy { scalar in
            switch scalar.properties.generalCategory {
            case .control, .format, .privateUse, .surrogate, .unassigned, .lineSeparator, .paragraphSeparator: false
            default: !(0xE0000...0xE007F).contains(scalar.value)
            }
        }
    }

    private static func exactly(_ object: [String: Any], _ keys: Set<String>, at path: String) throws(Problem) {
        for key in object.keys where !keys.contains(key) { throw .unknownField(path + String(key.prefix(64))) }
        for key in keys.sorted() where object[key] == nil { throw .missingField(path + key) }
    }

    private static func list(_ value: Any?, _ field: String, max: Int, _ pattern: String) throws(Problem) -> [String] {
        guard let items = value as? [Any], items.count <= max else { throw .invalid(field) }
        var out: [String] = []
        for item in items {
            guard let s = item as? String, matches(s, pattern), !out.contains(s) else { throw .invalid(field) }
            out.append(s)
        }
        return out
    }

    /// A JSON integer (not a boolean, not a fraction).
    private static func integer(_ value: Any?) -> Int? {
        guard let n = value as? NSNumber, CFGetTypeID(n) != CFBooleanGetTypeID() else { return nil }
        let d = n.doubleValue
        guard d.isFinite, d == d.rounded(), abs(d) < 1_000_000 else { return nil }
        return Int(d)
    }

    static func matches(_ s: String, _ pattern: String) -> Bool {
        s.range(of: pattern, options: .regularExpression) != nil && !s.contains("\n")
    }
}

/// Paths inside a scene bundle: segments of `[A-Za-z0-9_][A-Za-z0-9._-]*`
/// (no dot files, no `.` or `..`, ASCII only), at most 16 deep.
nonisolated enum ScenePath {
    static func validSegment(_ s: Substring) -> Bool {
        guard (1...128).contains(s.utf8.count), let first = s.unicodeScalars.first, first != "." , first != "-" else { return false }
        return s.unicodeScalars.allSatisfy { c in
            c.isASCII && (("a"..."z").contains(c) || ("A"..."Z").contains(c) || ("0"..."9").contains(c) || "._-".unicodeScalars.contains(c))
        }
    }

    static func validRelative(_ path: String) -> Bool {
        let parts = path.split(separator: "/", omittingEmptySubsequences: false)
        return (1...16).contains(parts.count) && parts.allSatisfy(validSegment)
    }
}

/// A strict reading of JSON text: well-formed, at most 32 levels deep, and no
/// object with the same key twice (which `JSONSerialization` would quietly
/// resolve, so what the app checked and what a scene meant could differ).
nonisolated enum StrictJSON {
    enum Failure: Error { case syntax, tooDeep, duplicate(String) }

    static func check(_ data: Data) throws(Failure) {
        var reader = Reader(bytes: [UInt8](data))
        reader.space()
        try reader.value(depth: 0)
        reader.space()
        guard reader.at == reader.bytes.count else { throw .syntax }
    }

    private struct Reader {
        let bytes: [UInt8]
        var at = 0

        mutating func space() {
            while at < bytes.count, [0x20, 0x09, 0x0A, 0x0D].contains(bytes[at]) { at += 1 }
        }

        mutating func expect(_ literal: String) throws(Failure) {
            for b in literal.utf8 {
                guard at < bytes.count, bytes[at] == b else { throw .syntax }
                at += 1
            }
        }

        mutating func value(depth: Int) throws(Failure) {
            guard depth < 32 else { throw .tooDeep }
            guard at < bytes.count else { throw .syntax }
            switch bytes[at] {
            case UInt8(ascii: "{"):
                at += 1
                var keys = Set<String>()
                space()
                if at < bytes.count, bytes[at] == UInt8(ascii: "}") { at += 1; return }
                while true {
                    space()
                    let key = try string()
                    guard keys.insert(key).inserted else { throw .duplicate(key) }
                    space()
                    try expect(":")
                    space()
                    try value(depth: depth + 1)
                    space()
                    guard at < bytes.count else { throw .syntax }
                    if bytes[at] == UInt8(ascii: ",") { at += 1; continue }
                    try expect("}")
                    return
                }
            case UInt8(ascii: "["):
                at += 1
                space()
                if at < bytes.count, bytes[at] == UInt8(ascii: "]") { at += 1; return }
                while true {
                    space()
                    try value(depth: depth + 1)
                    space()
                    guard at < bytes.count else { throw .syntax }
                    if bytes[at] == UInt8(ascii: ",") { at += 1; continue }
                    try expect("]")
                    return
                }
            case UInt8(ascii: "\""): _ = try string()
            case UInt8(ascii: "t"): try expect("true")
            case UInt8(ascii: "f"): try expect("false")
            case UInt8(ascii: "n"): try expect("null")
            default: try number()
            }
        }

        /// A string's decoded text (escapes resolved, so `"a"` and `"a"` are one key).
        mutating func string() throws(Failure) -> String {
            try expect("\"")
            var units: [UInt16] = []
            var raw: [UInt8] = []
            func flush() {
                if !raw.isEmpty { units.append(contentsOf: String(decoding: raw, as: UTF8.self).utf16); raw.removeAll() }
            }
            while at < bytes.count {
                let b = bytes[at]
                at += 1
                switch b {
                case UInt8(ascii: "\""):
                    flush()
                    return String(decoding: units, as: UTF16.self)
                case UInt8(ascii: "\\"):
                    flush()
                    guard at < bytes.count else { throw .syntax }
                    let e = bytes[at]
                    at += 1
                    switch e {
                    case UInt8(ascii: "\""): units.append(0x22)
                    case UInt8(ascii: "\\"): units.append(0x5C)
                    case UInt8(ascii: "/"): units.append(0x2F)
                    case UInt8(ascii: "b"): units.append(0x08)
                    case UInt8(ascii: "f"): units.append(0x0C)
                    case UInt8(ascii: "n"): units.append(0x0A)
                    case UInt8(ascii: "r"): units.append(0x0D)
                    case UInt8(ascii: "t"): units.append(0x09)
                    case UInt8(ascii: "u"):
                        guard at + 4 <= bytes.count, let unit = UInt16(String(decoding: bytes[at..<at + 4], as: UTF8.self), radix: 16)
                        else { throw .syntax }
                        units.append(unit)
                        at += 4
                    default: throw .syntax
                    }
                default:
                    guard b >= 0x20 else { throw .syntax }
                    raw.append(b)
                }
            }
            throw .syntax
        }

        mutating func number() throws(Failure) {
            let start = at
            if at < bytes.count, bytes[at] == UInt8(ascii: "-") { at += 1 }
            func digits() -> Int {
                let s = at
                while at < bytes.count, (0x30...0x39).contains(bytes[at]) { at += 1 }
                return at - s
            }
            let whole = digits()
            guard whole > 0, !(whole > 1 && bytes[at - whole] == 0x30) else { throw .syntax }
            if at < bytes.count, bytes[at] == UInt8(ascii: ".") {
                at += 1
                guard digits() > 0 else { throw .syntax }
            }
            if at < bytes.count, bytes[at] == UInt8(ascii: "e") || bytes[at] == UInt8(ascii: "E") {
                at += 1
                if at < bytes.count, bytes[at] == UInt8(ascii: "+") || bytes[at] == UInt8(ascii: "-") { at += 1 }
                guard digits() > 0 else { throw .syntax }
            }
            guard at > start else { throw .syntax }
        }
    }
}
