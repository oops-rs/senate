import CryptoKit
import Foundation
import Observation
import SenateCore

/// A scene the person installed (or, in a Debug build, the one
/// `SENATE_APP_SCENE_DEV` points at), as the app knows it.
struct InstalledScene: Identifiable, Hashable, Sendable {
    /// The identity the app gave it when it was installed: 32 lowercase hex
    /// digits, random, kept across updates. Its origin, its storage and its
    /// grant are all named by this, never by the manifest's name.
    let id: String
    let manifest: SceneManifest
    /// The folder its files are served from.
    let root: URL
    let digest: String
    let installedAt: Date
    /// Loaded straight from a build folder (`SENATE_APP_SCENE_DEV`).
    let development: Bool

    static func == (a: Self, b: Self) -> Bool { a.id == b.id && a.digest == b.digest && a.manifest == b.manifest }
    func hash(into h: inout Hasher) { h.combine(id) }

    /// The tag its observer grants carry on the daemon, from its identity.
    var grantTag: String { Self.grantTag(id) }
    static func grantTag(_ id: String) -> String { "scene \(id.prefix(12))" }
    /// The name its grant is issued under: the scene's label, then its tag.
    var grantName: String { "\(String(manifest.name.prefix(48))) · \(grantTag)" }

    static func validID(_ id: String) -> Bool {
        id.utf8.count == 32 && id.unicodeScalars.allSatisfy { ("0"..."9").contains($0) || ("a"..."f").contains($0) }
    }
}

/// What changes in what a scene asks for, from one bundle to the next
/// (shown before an update replaces the installed one).
struct SceneAsksChange: Equatable {
    var addedCapabilities: [String] = []
    var removedCapabilities: [String] = []
    var addedReads: [String] = []
    var removedReads: [String] = []
    var addedWrites: [String] = []
    var removedWrites: [String] = []
    var topologyChanged = false

    init(from old: SceneManifest, to new: SceneManifest) {
        addedCapabilities = new.capabilities.filter { !old.capabilities.contains($0) }
        removedCapabilities = old.capabilities.filter { !new.capabilities.contains($0) }
        addedReads = new.cardReads.filter { !old.cardReads.contains($0) }
        removedReads = old.cardReads.filter { !new.cardReads.contains($0) }
        addedWrites = new.cardWrites.filter { !old.cardWrites.contains($0) }
        removedWrites = old.cardWrites.filter { !new.cardWrites.contains($0) }
        topologyChanged = old.wantsTopology != new.wantsTopology
    }

    var isEmpty: Bool {
        addedCapabilities.isEmpty && removedCapabilities.isEmpty && addedReads.isEmpty && removedReads.isEmpty
            && addedWrites.isEmpty && removedWrites.isEmpty && !topologyChanged
    }
}

/// The scenes installed for this person, in the app's Application Support
/// folder (`Scenes/<id>/`: `bundle/` the files as checked and copied,
/// `scene.json` when and what, `storage/` what it keeps per Senate). Adding
/// one checks it completely first and runs nothing; removing one revokes
/// its grants and deletes its storage with it.
@MainActor
@Observable
final class SceneLibrary {
    /// The scenes, oldest first (the sidebar's order).
    private(set) var scenes: [InstalledScene] = []
    /// A scene that could not be read back (shown in the Scenes place).
    private(set) var damaged: [String] = []

    @ObservationIgnored let root: URL
    @ObservationIgnored private let development: URL?

    /// Where scenes live: `SENATE_APP_SCENES_DIR` in a Debug build (tests,
    /// snapshots), otherwise Application Support/<bundle id>/Scenes.
    static func defaultRoot() -> URL {
        #if DEBUG
        if let dir = ProcessInfo.processInfo.environment["SENATE_APP_SCENES_DIR"], !dir.isEmpty {
            return URL(fileURLWithPath: dir, isDirectory: true)
        }
        #endif
        let support = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        return support.appendingPathComponent(Bundle.main.bundleIdentifier ?? "dev.senate.app", isDirectory: true)
            .appendingPathComponent("Scenes", isDirectory: true)
    }

    /// `SENATE_APP_SCENE_DEV=<folder>` (Debug builds only): that build folder,
    /// loaded as it is, re-checked at every load.
    static func developmentFolder() -> URL? {
        #if DEBUG
        if let dir = ProcessInfo.processInfo.environment["SENATE_APP_SCENE_DEV"], !dir.isEmpty {
            return URL(fileURLWithPath: dir, isDirectory: true).standardizedFileURL
        }
        #endif
        return nil
    }

    init(root: URL = SceneLibrary.defaultRoot(), development: URL? = SceneLibrary.developmentFolder()) {
        self.root = root
        self.development = development
        reload()
    }

    func scene(_ id: String) -> InstalledScene? { scenes.first { $0.id == id } }

    private func folder(_ id: String) -> URL { root.appendingPathComponent(id, isDirectory: true) }
    func storageFolder(_ id: String) -> URL { folder(id).appendingPathComponent("storage", isDirectory: true) }

    private struct Record: Codable {
        var installed_at: Date
        var digest: String
    }

    /// Read the installed scenes again from disk.
    func reload() {
        var found: [InstalledScene] = []
        var broken: [String] = []
        let names = (try? FileManager.default.contentsOfDirectory(atPath: root.path)) ?? []
        // A development scene's identity: from where it is, so its storage and grant last across runs.
        let developmentID = development.map {
            SHA256.hash(data: Data(("dev:" + $0.path).utf8)).prefix(16).map { String(format: "%02x", $0) }.joined()
        }
        for id in names where InstalledScene.validID(id) && id != developmentID {
            let dir = folder(id)
            let decoder = JSONDecoder()
            decoder.dateDecodingStrategy = .iso8601
            guard let data = try? Data(contentsOf: dir.appendingPathComponent("scene.json")),
                  let record = try? decoder.decode(Record.self, from: data),
                  let bundle = try? SceneBundle.scan(dir.appendingPathComponent("bundle", isDirectory: true)),
                  // Exactly what was installed: a changed file makes it unreadable, not different.
                  bundle.digest == record.digest
            else {
                broken.append(id)
                continue
            }
            found.append(InstalledScene(id: id, manifest: bundle.manifest, root: bundle.root, digest: bundle.digest,
                                        installedAt: record.installed_at, development: false))
        }
        found.sort { ($0.installedAt, $0.id) < ($1.installedAt, $1.id) }
        if let development, let id = developmentID {
            if let bundle = try? SceneBundle.scan(development) {
                found.append(InstalledScene(id: id, manifest: bundle.manifest, root: development, digest: bundle.digest,
                                            installedAt: .distantFuture, development: true))
            } else {
                Log.app.error("scenes: SENATE_APP_SCENE_DEV is not a valid scene bundle")
            }
        }
        scenes = found
        damaged = broken
    }

    // MARK: Adding, updating, removing

    /// A bundle chosen to add: checked, with its files staged where the app
    /// controls them. Nothing of it has run.
    struct Candidate: Identifiable {
        let id = UUID()
        let bundle: SceneBundle
        /// A staging folder of ours to delete afterwards (zips), if any.
        let staging: URL?
        var manifest: SceneManifest { bundle.manifest }
    }

    /// Check a folder or a `.senatescene` zip the person chose.
    func inspect(_ url: URL) throws -> Candidate {
        var st = stat()
        guard lstat(url.path, &st) == 0 else { throw SceneBundle.Problem.notAFolder }
        if (st.st_mode & S_IFMT) == S_IFDIR {
            return Candidate(bundle: try SceneBundle.scan(url), staging: nil)
        }
        guard (st.st_mode & S_IFMT) == S_IFREG, ["senatescene", "zip"].contains(url.pathExtension.lowercased()) else {
            throw SceneBundle.Problem.notAFolder
        }
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        let staging = root.appendingPathComponent(".staging-\(UUID().uuidString)", isDirectory: true)
        do {
            let inner = try SceneArchive.unpack(url, into: staging)
            return Candidate(bundle: try SceneBundle.scan(inner), staging: staging)
        } catch {
            try? FileManager.default.removeItem(at: staging)
            throw error
        }
    }

    /// The person chose not to add it.
    func discard(_ candidate: Candidate) {
        if let staging = candidate.staging { try? FileManager.default.removeItem(at: staging) }
    }

    /// Install a checked candidate as a new scene, under a new identity.
    @discardableResult
    func install(_ candidate: Candidate, at now: Date = Date()) throws -> InstalledScene {
        defer { discard(candidate) }
        var bytes = [UInt8](repeating: 0, count: 16)
        guard SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes) == errSecSuccess else { throw CocoaError(.fileWriteUnknown) }
        let id = bytes.map { String(format: "%02x", $0) }.joined()
        let fm = FileManager.default
        try fm.createDirectory(at: root, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        let staged = root.appendingPathComponent(".install-\(id)", isDirectory: true)
        try fm.createDirectory(at: staged, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
        do {
            try candidate.bundle.copy(to: staged.appendingPathComponent("bundle", isDirectory: true))
            let copied = try SceneBundle.scan(staged.appendingPathComponent("bundle", isDirectory: true))
            guard copied.digest == candidate.bundle.digest else { throw CocoaError(.fileWriteUnknown) }
            try writeRecord(Record(installed_at: now, digest: copied.digest), in: staged)
            try fm.moveItem(at: staged, to: folder(id))
        } catch {
            try? fm.removeItem(at: staged)
            throw error
        }
        reload()
        guard let scene = scene(id) else { throw CocoaError(.fileReadCorruptFile) }
        return scene
    }

    /// Replace an installed scene's bundle with a checked candidate, keeping
    /// its identity and storage.
    func update(_ id: String, with candidate: Candidate) throws {
        defer { discard(candidate) }
        guard let old = scene(id), !old.development else { throw CocoaError(.fileNoSuchFile) }
        let fm = FileManager.default
        let dir = folder(id)
        let next = dir.appendingPathComponent(".bundle-next", isDirectory: true)
        try? fm.removeItem(at: next)
        try candidate.bundle.copy(to: next)
        guard (try SceneBundle.scan(next)).digest == candidate.bundle.digest else {
            try? fm.removeItem(at: next)
            throw CocoaError(.fileWriteUnknown)
        }
        let current = dir.appendingPathComponent("bundle", isDirectory: true)
        _ = try fm.replaceItemAt(current, withItemAt: next)
        try writeRecord(Record(installed_at: old.installedAt, digest: candidate.bundle.digest), in: dir)
        reload()
    }

    /// Delete an installed scene's files and storage. Its grants are the
    /// caller's to revoke first ([`SceneAccess.removeAll`]).
    func remove(_ id: String) throws {
        guard InstalledScene.validID(id), scene(id)?.development != true else { return }
        let dir = folder(id)
        if FileManager.default.fileExists(atPath: dir.path) { try FileManager.default.removeItem(at: dir) }
        reload()
    }

    private func writeRecord(_ record: Record, in dir: URL) throws {
        let encoder = JSONEncoder()
        encoder.dateEncodingStrategy = .iso8601
        try encoder.encode(record).write(to: dir.appendingPathComponent("scene.json"), options: .atomic)
    }
}

/// What one scene keeps for one Senate (`storage.*`): JSON values by key,
/// 16 KiB a value and 64 KiB in all (keys counted), in a file of its own
/// under the scene's folder. Another scene, or the same scene for another
/// Senate, has another file.
@MainActor
final class SceneStorage {
    static let maxValue = 16 * 1024
    static let maxTotal = 64 * 1024

    private let file: URL
    private var values: [String: String]

    init(folder: URL, senate: String) {
        let name = SHA256.hash(data: Data(senate.utf8)).prefix(16).map { String(format: "%02x", $0) }.joined()
        file = folder.appendingPathComponent("\(name).json")
        values = (try? Data(contentsOf: file)).flatMap { try? JSONDecoder().decode([String: String].self, from: $0) } ?? [:]
    }

    /// Key names: 1–64 URI-unreserved characters.
    static func validKey(_ key: String) -> Bool {
        (1...64).contains(key.utf8.count)
            && key.unicodeScalars.allSatisfy { $0.isASCII && (CharacterSet.alphanumerics.contains($0) || "-._~".unicodeScalars.contains($0)) }
    }

    func get(_ key: String) -> String? { values[key] }

    /// False: over a limit (nothing changed).
    func set(_ key: String, json: String) -> Bool {
        guard json.utf8.count <= Self.maxValue else { return false }
        var next = values
        next[key] = json
        guard next.reduce(0, { $0 + $1.key.utf8.count + $1.value.utf8.count }) <= Self.maxTotal else { return false }
        values = next
        save()
        return true
    }

    func delete(_ key: String) {
        guard values.removeValue(forKey: key) != nil else { return }
        save()
    }

    private func save() {
        let fm = FileManager.default
        try? fm.createDirectory(at: file.deletingLastPathComponent(), withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        guard let data = try? JSONEncoder().encode(values) else { return }
        try? data.write(to: file, options: [.atomic])
    }
}
