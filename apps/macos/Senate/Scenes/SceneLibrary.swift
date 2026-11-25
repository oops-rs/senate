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
    /// Installed from an archive the app carries (`Resources/Scenes/<key>.senatescene`): its key.
    var builtIn: String? = nil

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

    /// It asks for something it did not (or its reach changes): the person sees the update first.
    var grows: Bool { !addedCapabilities.isEmpty || !addedReads.isEmpty || !addedWrites.isEmpty || topologyChanged }
}

/// A scene the app carries that is not installed: one the person removed,
/// offered again.
struct BuiltInScene: Identifiable, Hashable {
    /// The archive's name without `.senatescene`.
    let key: String
    let archive: URL
    let name: String
    let version: String
    var id: String { key }
}

/// Semantic-version order for the manifest's `major.minor.patch[-pre]`.
enum SceneVersion {
    static func newer(_ a: String, than b: String) -> Bool {
        func split(_ v: String) -> ([Int], String?) {
            let parts = v.split(separator: "-", maxSplits: 1).map(String.init)
            return (parts[0].split(separator: ".").map { Int($0) ?? 0 }, parts.count > 1 ? parts[1] : nil)
        }
        let (x, xp) = split(a)
        let (y, yp) = split(b)
        if x != y { return x.lexicographicallyPrecedes(y) == false }
        switch (xp, yp) {
        case (nil, nil): return false
        case (nil, _): return true
        case (_, nil): return false
        case let (p?, q?): return p.compare(q, options: .numeric) == .orderedDescending
        }
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
    /// Built-in scenes the person removed, offered again.
    private(set) var offered: [BuiltInScene] = []
    /// Installed built-in scenes whose newer bundled version asks for more:
    /// scene id to the archive, waiting for the person's Update.
    private(set) var waitingUpdates: [String: URL] = [:]

    @ObservationIgnored let root: URL
    @ObservationIgnored private let development: URL?
    /// The built-in keys recorded by every installed scene whose record reads, damaged ones included.
    @ObservationIgnored private var installedBuiltIn: Set<String> = []

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

    /// The app's built-in scenes (`installBuiltIn`), if it carries any.
    @ObservationIgnored private let builtIn: URL?

    init(root: URL = SceneLibrary.defaultRoot(), development: URL? = SceneLibrary.developmentFolder(), builtIn: URL? = nil) {
        self.root = root
        self.development = development
        self.builtIn = builtIn
        reload()
    }

    func scene(_ id: String) -> InstalledScene? { scenes.first { $0.id == id } }

    private func folder(_ id: String) -> URL { root.appendingPathComponent(id, isDirectory: true) }
    func storageFolder(_ id: String) -> URL { folder(id).appendingPathComponent("storage", isDirectory: true) }

    private struct Record: Codable {
        var installed_at: Date
        var digest: String
        var built_in: String?
    }

    /// Read the installed scenes again from disk.
    func reload() {
        var found: [InstalledScene] = []
        var broken: [String] = []
        var builtIn: Set<String> = []
        let names = (try? FileManager.default.contentsOfDirectory(atPath: root.path)) ?? []
        // A development scene's identity: from where it is, so its storage and grant last across runs.
        let developmentID = development.map {
            SHA256.hash(data: Data(("dev:" + $0.path).utf8)).prefix(16).map { String(format: "%02x", $0) }.joined()
        }
        for id in names where InstalledScene.validID(id) && id != developmentID {
            let dir = folder(id)
            let record = readRecord(in: dir)
            if let key = record?.built_in { builtIn.insert(key) }
            guard let record,
                  let bundle = try? SceneBundle.scan(dir.appendingPathComponent("bundle", isDirectory: true)),
                  // Exactly what was installed: a changed file makes it unreadable, not different.
                  bundle.digest == record.digest
            else {
                broken.append(id)
                continue
            }
            found.append(InstalledScene(id: id, manifest: bundle.manifest, root: bundle.root, digest: bundle.digest,
                                        installedAt: record.installed_at, development: false, builtIn: record.built_in))
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
        installedBuiltIn = builtIn
        waitingUpdates = waitingUpdates.filter { id, _ in found.contains { $0.id == id } }
    }

    private func readRecord(in dir: URL) -> Record? {
        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .iso8601
        guard let data = try? Data(contentsOf: dir.appendingPathComponent("scene.json")) else { return nil }
        return try? decoder.decode(Record.self, from: data)
    }

    // MARK: Built-in scenes

    /// Where the app carries its built-in scenes: `Contents/Resources/Scenes`.
    static func builtInFolder() -> URL? {
        Bundle.main.resourceURL?.appendingPathComponent("Scenes", isDirectory: true)
    }

    /// The built-in keys the person removed (`built-in.json` beside the scenes).
    private var removedFile: URL { root.appendingPathComponent("built-in.json") }
    private struct Removed: Codable { var removed: [String] }

    private func removedBuiltIn() -> Set<String> {
        guard let data = try? Data(contentsOf: removedFile), let file = try? JSONDecoder().decode(Removed.self, from: data) else { return [] }
        return Set(file.removed)
    }

    private func setRemoved(_ key: String, _ removed: Bool) {
        var keys = removedBuiltIn()
        guard keys.contains(key) != removed else { return }
        if removed { keys.insert(key) } else { keys.remove(key) }
        try? FileManager.default.createDirectory(at: root, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        try? JSONEncoder().encode(Removed(removed: keys.sorted())).write(to: removedFile, options: .atomic)
    }

    /// At launch: install each built-in scene that is not installed and was
    /// not removed, through the same checks as Add Scene; update an
    /// installed one to a newer bundled version when it asks for nothing
    /// more (otherwise it waits for the person's Update); offer the removed
    /// ones again. An archive that fails the checks is logged and skipped.
    func installBuiltIn() {
        guard let folder = builtIn,
              let names = try? FileManager.default.contentsOfDirectory(atPath: folder.path) else { return }
        let removed = removedBuiltIn()
        var offers: [BuiltInScene] = []
        for name in names.sorted() where name.hasSuffix(".senatescene") {
            let key = String(name.dropLast(".senatescene".count))
            let archive = folder.appendingPathComponent(name, isDirectory: false)
            let candidate: Candidate
            do {
                candidate = try inspect(archive, builtIn: key)
            } catch {
                Log.app.error("scenes: built-in \(name, privacy: .public) refused: \(String(describing: error), privacy: .public)")
                continue
            }
            if let scene = scenes.first(where: { $0.builtIn == key }) {
                let newer = SceneVersion.newer(candidate.manifest.version, than: scene.manifest.version)
                if !newer || candidate.bundle.digest == scene.digest {
                    discard(candidate)
                } else if SceneAsksChange(from: scene.manifest, to: candidate.manifest).grows {
                    discard(candidate)
                    waitingUpdates[scene.id] = archive
                } else {
                    do { try update(scene.id, with: candidate) } catch {
                        Log.app.error("scenes: built-in \(name, privacy: .public) update failed: \(String(describing: error), privacy: .public)")
                    }
                }
            } else if installedBuiltIn.contains(key) {
                discard(candidate) // installed but unreadable: shown as damaged, not installed twice
            } else if removed.contains(key) {
                offers.append(BuiltInScene(key: key, archive: archive, name: candidate.manifest.name, version: candidate.manifest.version))
                discard(candidate)
            } else {
                do { try install(candidate) } catch {
                    Log.app.error("scenes: built-in \(name, privacy: .public) install failed: \(String(describing: error), privacy: .public)")
                }
            }
        }
        offered = offers
    }

    /// The person updated a scene, or chose not to now: its waiting update is done with for this launch.
    func clearWaitingUpdate(_ id: String) {
        waitingUpdates.removeValue(forKey: id)
    }

    // MARK: Adding, updating, removing

    /// A bundle chosen to add: checked, with its files staged where the app
    /// controls them. Nothing of it has run.
    struct Candidate: Identifiable {
        let id = UUID()
        let bundle: SceneBundle
        /// A staging folder of ours to delete afterwards (zips), if any.
        let staging: URL?
        /// One of the app's built-in scenes: its key.
        var builtIn: String? = nil
        var manifest: SceneManifest { bundle.manifest }
    }

    /// Check a folder or a `.senatescene` zip the person chose (or the app
    /// carries, as the built-in scene `builtIn`).
    func inspect(_ url: URL, builtIn: String? = nil) throws -> Candidate {
        var st = stat()
        guard lstat(url.path, &st) == 0 else { throw SceneBundle.Problem.notAFolder }
        if (st.st_mode & S_IFMT) == S_IFDIR {
            return Candidate(bundle: try SceneBundle.scan(url), staging: nil, builtIn: builtIn)
        }
        guard (st.st_mode & S_IFMT) == S_IFREG, ["senatescene", "zip"].contains(url.pathExtension.lowercased()) else {
            throw SceneBundle.Problem.notAFolder
        }
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        let staging = root.appendingPathComponent(".staging-\(UUID().uuidString)", isDirectory: true)
        do {
            let inner = try SceneArchive.unpack(url, into: staging)
            return Candidate(bundle: try SceneBundle.scan(inner), staging: staging, builtIn: builtIn)
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
            try writeRecord(Record(installed_at: now, digest: copied.digest, built_in: candidate.builtIn), in: staged)
            try fm.moveItem(at: staged, to: folder(id))
        } catch {
            try? fm.removeItem(at: staged)
            throw error
        }
        if let key = candidate.builtIn {
            setRemoved(key, false)
            offered.removeAll { $0.key == key }
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
        try writeRecord(Record(installed_at: old.installedAt, digest: candidate.bundle.digest, built_in: old.builtIn), in: dir)
        waitingUpdates.removeValue(forKey: id)
        reload()
    }

    /// Delete an installed scene's files and storage. Its grants are the
    /// caller's to revoke first ([`SceneAccess.removeAll`]). A built-in
    /// scene stays removed, and is offered again.
    func remove(_ id: String) throws {
        guard InstalledScene.validID(id), scene(id)?.development != true else { return }
        let dir = folder(id)
        let key = scene(id)?.builtIn ?? readRecord(in: dir)?.built_in
        if let key { setRemoved(key, true) }
        if FileManager.default.fileExists(atPath: dir.path) { try FileManager.default.removeItem(at: dir) }
        reload()
        if key != nil { installBuiltIn() }
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
