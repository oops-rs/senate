import CryptoKit
import Darwin
import Foundation

/// A scene bundle on disk, checked: a folder of plain files of allowed types,
/// within the size limits, with no links (symbolic or hard) and no path that
/// could leave the folder, and a valid manifest whose entry is among them.
/// What `scan` returns is the complete list of what may ever be served.
nonisolated struct SceneBundle: Sendable {
    static let maxFile = 8 * 1024 * 1024
    static let maxTotal = 64 * 1024 * 1024
    static let maxFiles = 2000

    /// The types a scene may contain, by extension, and how they are served.
    static let types: [String: String] = [
        "html": "text/html", "js": "text/javascript", "mjs": "text/javascript", "css": "text/css",
        "json": "application/json", "ndjson": "application/x-ndjson", "txt": "text/plain", "md": "text/plain",
        "png": "image/png", "jpg": "image/jpeg", "jpeg": "image/jpeg", "gif": "image/gif", "webp": "image/webp",
        "woff2": "font/woff2",
    ]
    /// Licence files without an extension, served as text.
    static let plainNames: Set<String> = ["LICENSE", "NOTICE", "COPYING", "AUTHORS"]

    /// The bundle's folder.
    let root: URL
    let manifest: SceneManifest
    /// Bundle-relative paths and sizes, sorted.
    let files: [(path: String, size: Int)]
    /// SHA-256 over every path and its bytes: what exactly this bundle is.
    let digest: String

    var size: Int { files.reduce(0) { $0 + $1.size } }

    enum Problem: Error, Equatable, CustomStringConvertible {
        case notAFolder
        case link(String)
        case special(String)
        case badName(String)
        case type(String)
        case fileTooLarge(String)
        case tooLarge
        case tooMany
        case noManifest
        case manifest(SceneManifest.Problem)
        case noEntry
        case archive(String)

        var description: String {
            switch self {
            case .notAFolder: L("scenes.problem.folder")
            case let .link(path): L("scenes.problem.link", path)
            case let .special(path): L("scenes.problem.special", path)
            case let .badName(path): L("scenes.problem.name", path)
            case let .type(path): L("scenes.problem.type", path)
            case let .fileTooLarge(path): L("scenes.problem.file_size", path)
            case .tooLarge: L("scenes.problem.size")
            case .tooMany: L("scenes.problem.count")
            case .noManifest: L("scenes.problem.no_manifest")
            case let .manifest(problem): problem.description
            case .noEntry: L("scenes.problem.no_entry")
            case let .archive(why): L("scenes.problem.archive", why)
            }
        }
    }

    /// How a file at `path` (bundle-relative) is served, or nil: not a scene's type.
    static func contentType(_ path: String) -> String? {
        let name = path.split(separator: "/").last.map(String.init) ?? path
        if plainNames.contains(name) { return "text/plain" }
        guard let dot = name.lastIndex(of: "."), dot != name.startIndex else { return nil }
        return types[name[name.index(after: dot)...].lowercased()]
    }

    /// Check the folder at `root` and everything in it.
    static func scan(_ root: URL) throws(Problem) -> SceneBundle {
        var st = stat()
        guard lstat(root.path, &st) == 0, (st.st_mode & S_IFMT) == S_IFDIR else { throw .notAFolder }
        var found: [(String, Int)] = []
        var total = 0
        try walk(root, prefix: "", depth: 0, into: &found, total: &total)
        found.sort { $0.0 < $1.0 }
        guard let manifestFile = found.first(where: { $0.0 == "manifest.json" }) else { throw .noManifest }
        guard manifestFile.1 <= SceneManifest.maxBytes, let data = try? read(root, "manifest.json") else {
            throw .manifest(.tooLarge)
        }
        let manifest: SceneManifest
        do { manifest = try SceneManifest.parse(data) } catch { throw .manifest(error) }
        guard found.contains(where: { $0.0 == manifest.entry }) else { throw .noEntry }
        var hash = SHA256()
        for (path, _) in found {
            guard let bytes = try? read(root, path) else { throw .special(path) }
            hash.update(data: Data(path.utf8))
            hash.update(data: Data([0]))
            hash.update(data: withUnsafeBytes(of: UInt64(bytes.count).littleEndian) { Data($0) })
            hash.update(data: bytes)
        }
        let digest = hash.finalize().map { String(format: "%02x", $0) }.joined()
        return SceneBundle(root: root, manifest: manifest, files: found.map { (path: $0.0, size: $0.1) }, digest: digest)
    }

    /// Files a Finder copy or a zip leaves that are not the scene's: skipped, never served.
    static func ignored(_ name: String, atRoot: Bool) -> Bool {
        name == ".DS_Store" || (atRoot && name == "__MACOSX")
    }

    private static func walk(_ dir: URL, prefix: String, depth: Int, into found: inout [(String, Int)], total: inout Int) throws(Problem) {
        guard depth < 16 else { throw .badName(prefix) }
        guard let names = try? FileManager.default.contentsOfDirectory(atPath: dir.path) else { throw .special(prefix.isEmpty ? "." : prefix) }
        for name in names.sorted() {
            if ignored(name, atRoot: depth == 0) { continue }
            let path = prefix.isEmpty ? name : "\(prefix)/\(name)"
            let shown = String(path.prefix(200))
            guard ScenePath.validSegment(Substring(name)) else { throw .badName(shown) }
            let url = dir.appendingPathComponent(name, isDirectory: false)
            var st = stat()
            guard lstat(url.path, &st) == 0 else { throw .special(shown) }
            switch st.st_mode & S_IFMT {
            case S_IFLNK: throw .link(shown)
            case S_IFDIR: try walk(url, prefix: path, depth: depth + 1, into: &found, total: &total)
            case S_IFREG:
                guard st.st_nlink == 1 else { throw .link(shown) }
                guard contentType(path) != nil else { throw .type(shown) }
                guard st.st_size <= maxFile else { throw .fileTooLarge(shown) }
                total += Int(st.st_size)
                guard total <= maxTotal else { throw .tooLarge }
                found.append((path, Int(st.st_size)))
                guard found.count <= maxFiles else { throw .tooMany }
            default: throw .special(shown)
            }
        }
    }

    enum ReadFailure: Error { case path, notFound, link, notPlain, tooLarge }

    /// One file's bytes, opened without following any link at any level,
    /// checked to be a plain unshared file within the size limit.
    static func read(_ root: URL, _ path: String) throws(ReadFailure) -> Data {
        guard ScenePath.validRelative(path) else { throw .path }
        // Walk down from the root one component at a time, never through a link.
        var fd = open(root.path, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
        guard fd >= 0 else { throw .notFound }
        let parts = path.split(separator: "/").map(String.init)
        for (i, part) in parts.enumerated() {
            let last = i == parts.count - 1
            let next = openat(fd, part, O_RDONLY | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK | (last ? 0 : O_DIRECTORY))
            if next < 0 {
                var st = stat()
                let isLink = fstatat(fd, part, &st, AT_SYMLINK_NOFOLLOW) == 0 && (st.st_mode & S_IFMT) == S_IFLNK
                close(fd)
                throw isLink ? .link : .notFound
            }
            close(fd)
            fd = next
        }
        defer { close(fd) }
        var st = stat()
        guard fstat(fd, &st) == 0, (st.st_mode & S_IFMT) == S_IFREG else { throw .notPlain }
        guard st.st_nlink == 1 else { throw .link }
        guard st.st_size <= maxFile else { throw .tooLarge }
        var data = Data(count: Int(st.st_size))
        var got = 0
        let ok = data.withUnsafeMutableBytes { buffer -> Bool in
            while got < buffer.count {
                let n = Darwin.read(fd, buffer.baseAddress! + got, buffer.count - got)
                if n <= 0 { return n == 0 }
                got += n
            }
            return true
        }
        guard ok else { throw .notFound }
        return data.prefix(got)
    }

    /// Copy exactly the scanned files (their bytes, nothing else: no links,
    /// modes or extended attributes) into `destination`, which must not exist.
    func copy(to destination: URL) throws {
        let fm = FileManager.default
        try fm.createDirectory(at: destination, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
        for (path, _) in files {
            let data = try Self.read(root, path)
            let target = destination.appendingPathComponent(path, isDirectory: false)
            try fm.createDirectory(at: target.deletingLastPathComponent(), withIntermediateDirectories: true,
                                   attributes: [.posixPermissions: 0o700])
            try data.write(to: target, options: .withoutOverwriting)
        }
    }
}

/// A `.senatescene` file: a zip of a scene bundle. Its central directory is
/// read first (sizes, count, names, no links, no encryption, no zip64), so an
/// archive that would unpack into something unacceptable is refused before a
/// byte of it is written; then it is unpacked into an empty staging folder
/// and the result is scanned like any folder.
nonisolated enum SceneArchive {
    static let maxArchive = 64 * 1024 * 1024
    static let maxEntries = 4000

    struct Entry { let name: String; let size: UInt64; let directory: Bool }

    static func entries(_ data: Data) throws(SceneBundle.Problem) -> [Entry] {
        guard data.count <= maxArchive else { throw .archive(L("scenes.problem.archive_size")) }
        let b = [UInt8](data)
        func u16(_ i: Int) -> Int { Int(b[i]) | Int(b[i + 1]) << 8 }
        func u32(_ i: Int) -> UInt64 { UInt64(b[i]) | UInt64(b[i + 1]) << 8 | UInt64(b[i + 2]) << 16 | UInt64(b[i + 3]) << 24 }
        // The end of central directory record, searched for from the end.
        guard b.count >= 22 else { throw .archive(L("scenes.problem.archive_zip")) }
        var eocd = -1
        var i = b.count - 22
        while i >= max(0, b.count - 22 - 65535) {
            if u32(i) == 0x0605_4B50 { eocd = i; break }
            i -= 1
        }
        guard eocd >= 0 else { throw .archive(L("scenes.problem.archive_zip")) }
        let count = u16(eocd + 10)
        let cdSize = u32(eocd + 12), cdOffset = u32(eocd + 16)
        guard count != 0xFFFF, cdSize != 0xFFFF_FFFF, cdOffset != 0xFFFF_FFFF else { throw .archive(L("scenes.problem.archive_zip64")) }
        guard count <= maxEntries else { throw .tooMany }
        guard cdOffset + cdSize <= UInt64(eocd) else { throw .archive(L("scenes.problem.archive_zip")) }
        var at = Int(cdOffset)
        var out: [Entry] = []
        var total: UInt64 = 0
        for _ in 0..<count {
            guard at + 46 <= eocd, u32(at) == 0x0201_4B50 else { throw .archive(L("scenes.problem.archive_zip")) }
            let madeBy = u16(at + 4) >> 8
            let flags = u16(at + 8)
            let compressed = u32(at + 20), size = u32(at + 24)
            let nameLength = u16(at + 28), extra = u16(at + 30), comment = u16(at + 32)
            let external = u32(at + 38)
            guard at + 46 + nameLength <= eocd else { throw .archive(L("scenes.problem.archive_zip")) }
            let name = String(decoding: b[(at + 46)..<(at + 46 + nameLength)], as: UTF8.self)
            at += 46 + nameLength + extra + comment
            guard flags & 1 == 0 else { throw .archive(L("scenes.problem.archive_encrypted")) }
            guard compressed != 0xFFFF_FFFF, size != 0xFFFF_FFFF else { throw .archive(L("scenes.problem.archive_zip64")) }
            // Unix mode in the high half of the external attributes.
            let mode = (external >> 16) & 0xF000
            if madeBy == 3, mode == 0xA000 { throw .link(String(name.prefix(200))) }
            let directory = name.hasSuffix("/")
            let path = directory ? String(name.dropLast()) : name
            // Finder's resource forks: skipped when scanning, but never outside the folder.
            let segments = path.split(separator: "/", omittingEmptySubsequences: false)
            let sidecar = segments.first == "__MACOSX" || segments.last == ".DS_Store"
            if !sidecar {
                guard !path.isEmpty, !name.hasPrefix("/"), !name.contains("\\"), ScenePath.validRelative(path) else {
                    throw .badName(String(name.prefix(200)))
                }
            } else {
                guard !name.hasPrefix("/"), !name.contains("\\"), !segments.contains(".."), !segments.contains(".") else {
                    throw .badName(String(name.prefix(200)))
                }
            }
            if !directory, !sidecar {
                guard SceneBundle.contentType(path) != nil else { throw .type(String(path.prefix(200))) }
                guard size <= UInt64(SceneBundle.maxFile) else { throw .fileTooLarge(String(path.prefix(200))) }
            }
            total += size
            guard total <= UInt64(SceneBundle.maxTotal) else { throw .tooLarge }
            out.append(Entry(name: path, size: size, directory: directory))
        }
        return out
    }

    /// Unpack `archive` into the new folder `staging` (it must not exist) and
    /// return the bundle's folder within it: the staging folder, or its one
    /// folder when everything was zipped inside one.
    static func unpack(_ archive: URL, into staging: URL) throws -> URL {
        guard let data = try? Data(contentsOf: archive, options: .mappedIfSafe) else {
            throw SceneBundle.Problem.archive(L("scenes.problem.archive_zip"))
        }
        _ = try entries(data)
        try FileManager.default.createDirectory(at: staging, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
        let ditto = Process()
        ditto.executableURL = URL(fileURLWithPath: "/usr/bin/ditto")
        // -x -k: extract a zip; no resource forks or extended attributes kept.
        ditto.arguments = ["-x", "-k", "--norsrc", "--noextattr", "--noqtn", "--noacl", archive.path, staging.path]
        ditto.standardOutput = FileHandle.nullDevice
        ditto.standardError = FileHandle.nullDevice
        try ditto.run()
        let deadline = Date().addingTimeInterval(60)
        while ditto.isRunning, Date() < deadline { usleep(20_000) }
        if ditto.isRunning {
            ditto.terminate()
            throw SceneBundle.Problem.archive(L("scenes.problem.archive_zip"))
        }
        guard ditto.terminationStatus == 0 else { throw SceneBundle.Problem.archive(L("scenes.problem.archive_zip")) }
        let names = ((try? FileManager.default.contentsOfDirectory(atPath: staging.path)) ?? [])
            .filter { !SceneBundle.ignored($0, atRoot: true) }
        if !names.contains("manifest.json"), names.count == 1 {
            let inner = staging.appendingPathComponent(names[0], isDirectory: true)
            var st = stat()
            if lstat(inner.path, &st) == 0, (st.st_mode & S_IFMT) == S_IFDIR { return inner }
        }
        return staging
    }
}
