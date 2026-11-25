import Foundation
@testable import Senate

/// The fixture scene (SenateTests/Fixtures/scene), scratch folders, and
/// bundles made to measure for the scene host's tests.
enum SceneFixture {
    /// The fixture scene's folder in the source tree.
    static var folder: URL {
        URL(fileURLWithPath: #filePath).deletingLastPathComponent().appendingPathComponent("Fixtures/scene", isDirectory: true)
    }

    static var manifest: Data { (try? Data(contentsOf: folder.appendingPathComponent("manifest.json"))) ?? Data() }

    /// A new empty scratch folder, removed with the test's process temp.
    static func scratch(_ name: String = "scene") -> URL {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("senate-scene-tests-\(UUID().uuidString)", isDirectory: true)
            .appendingPathComponent(name, isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    /// A copy of the fixture scene in a scratch folder, to change.
    static func copy() throws -> URL {
        let dir = scratch()
        for name in try FileManager.default.contentsOfDirectory(atPath: folder.path) {
            try FileManager.default.copyItem(at: folder.appendingPathComponent(name), to: dir.appendingPathComponent(name))
        }
        return dir
    }

    /// A bundle with this manifest (JSON text) and an entry page.
    static func bundle(manifest: String, files: [String: String] = ["index.html": "<!doctype html><p>hi</p>"]) throws -> URL {
        let dir = scratch()
        try Data(manifest.utf8).write(to: dir.appendingPathComponent("manifest.json"))
        for (path, text) in files {
            let url = dir.appendingPathComponent(path)
            try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
            try Data(text.utf8).write(to: url)
        }
        return dir
    }

    /// A manifest's JSON text with fields replaced (or removed, as `nil`).
    static func manifest(_ changes: [String: Any?] = [:]) -> String {
        var object: [String: Any] = [
            "name": "Fixture", "version": "1.0.0", "entry": "index.html",
            "observe": ["major": 1, "required": ["state_patch"], "optional": ["app_links"]],
            "cards": ["reads": [], "writes": ["fixture.badge/v1"]], "min_host_version": 1,
        ]
        for (key, value) in changes {
            if let value { object[key] = value } else { object.removeValue(forKey: key) }
        }
        let data = try! JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])
        return String(decoding: data, as: UTF8.self)
    }

    /// A library in a scratch folder, with no development scene.
    @MainActor
    static func library() -> SceneLibrary {
        SceneLibrary(root: scratch("library"), development: nil)
    }
}

/// A zip archive made by hand (stored, not compressed), so the tests can
/// write the entries no zip tool would: links, escapes, lies about size.
struct ZipBuilder {
    struct Entry {
        var name: String
        var data: Data
        /// Unix mode bits for the external attributes (0o120777: a link).
        var mode: UInt32 = 0o100644
        var flags: UInt16 = 0
        /// The size the central directory claims (the real one if nil).
        var claimedSize: UInt32?
    }

    var entries: [Entry] = []

    mutating func add(_ name: String, _ text: String, mode: UInt32 = 0o100644, flags: UInt16 = 0, claimedSize: UInt32? = nil) {
        entries.append(Entry(name: name, data: Data(text.utf8), mode: mode, flags: flags, claimedSize: claimedSize))
    }

    static func crc32(_ data: Data) -> UInt32 {
        var crc: UInt32 = 0xFFFF_FFFF
        for byte in data {
            crc ^= UInt32(byte)
            for _ in 0..<8 { crc = (crc >> 1) ^ (0xEDB8_8320 & (0 &- (crc & 1))) }
        }
        return ~crc
    }

    func build() -> Data {
        var out = Data()
        var central = Data()
        func u16(_ v: UInt16, _ d: inout Data) { withUnsafeBytes(of: v.littleEndian) { d.append(contentsOf: $0) } }
        func u32(_ v: UInt32, _ d: inout Data) { withUnsafeBytes(of: v.littleEndian) { d.append(contentsOf: $0) } }
        for entry in entries {
            let offset = UInt32(out.count)
            let name = Data(entry.name.utf8)
            let crc = Self.crc32(entry.data)
            let size = UInt32(entry.data.count)
            u32(0x0403_4B50, &out); u16(20, &out); u16(entry.flags, &out); u16(0, &out); u16(0, &out); u16(0, &out)
            u32(crc, &out); u32(size, &out); u32(size, &out); u16(UInt16(name.count), &out); u16(0, &out)
            out.append(name); out.append(entry.data)
            u32(0x0201_4B50, &central); u16(0x031E, &central); u16(20, &central); u16(entry.flags, &central)
            u16(0, &central); u16(0, &central); u16(0, &central); u32(crc, &central)
            u32(size, &central); u32(entry.claimedSize ?? size, &central)
            u16(UInt16(name.count), &central); u16(0, &central); u16(0, &central); u16(0, &central); u16(0, &central)
            u32(entry.mode << 16, &central); u32(offset, &central)
            central.append(name)
        }
        let start = UInt32(out.count)
        out.append(central)
        u32(0x0605_4B50, &out); u16(0, &out); u16(0, &out)
        u16(UInt16(entries.count), &out); u16(UInt16(entries.count), &out)
        u32(UInt32(central.count), &out); u32(start, &out); u16(0, &out)
        return out
    }
}
