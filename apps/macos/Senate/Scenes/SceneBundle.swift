import Foundation
import SenateCore

/// A scene bundle on disk, checked by the shared rules (`crates/senate-scene`,
/// the same ones `senate scene inspect` applies): a folder of plain files of
/// allowed types, within the size limits, with no links (symbolic or hard)
/// and no path that could leave the folder, and a valid manifest whose entry
/// is among them. What `scan` returns is the complete list of what may ever
/// be served.
nonisolated struct SceneBundle: Sendable {
    /// The bundle's folder.
    let root: URL
    let manifest: SceneManifest
    /// Bundle-relative paths and sizes, sorted.
    let files: [(path: String, size: Int)]
    /// SHA-256 over every path and its bytes: what exactly this bundle is.
    let digest: String

    var size: Int { files.reduce(0) { $0 + $1.size } }

    /// The largest file a bundle may hold (the shared rules' limit).
    static let maxFile = Int(sceneMaxFile())

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
        case io(String)

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
            case let .io(path): L("scenes.problem.io", path)
            }
        }

        init(_ problem: SceneProblem) {
            if let manifest = SceneManifest.Problem(problem) {
                self = .manifest(manifest)
                return
            }
            switch problem {
            case let .Link(path): self = .link(path)
            case let .Special(path): self = .special(path)
            case let .BadName(path): self = .badName(path)
            case let .Type(path): self = .type(path)
            case let .FileTooLarge(path): self = .fileTooLarge(path)
            case .TooLarge: self = .tooLarge
            case .TooMany: self = .tooMany
            case .NoManifest: self = .noManifest
            case .NoEntry: self = .noEntry
            case let .Io(path): self = .io(path)
            case let .Archive(why):
                let text = switch why {
                case .notZip: L("scenes.problem.archive_zip")
                case .size: L("scenes.problem.archive_size")
                case .zip64: L("scenes.problem.archive_zip64")
                case .encrypted: L("scenes.problem.archive_encrypted")
                case .compressed: L("scenes.problem.archive_compressed")
                case .corrupt: L("scenes.problem.archive_corrupt")
                }
                self = .archive(text)
            default: self = .notAFolder
            }
        }
    }

    /// How a file at `path` (bundle-relative) is served, or nil: not a scene's type.
    static func contentType(_ path: String) -> String? { sceneContentType(path: path) }

    /// Check the folder at `root` and everything in it.
    static func scan(_ root: URL) throws(Problem) -> SceneBundle {
        do {
            let info = try sceneScan(root: root.path)
            return SceneBundle(root: root, manifest: SceneManifest(info.manifest),
                               files: info.files.map { (path: $0.path, size: Int($0.size)) }, digest: info.digest)
        } catch let problem as SceneProblem {
            throw Problem(problem)
        } catch {
            throw .notAFolder
        }
    }

    enum ReadFailure: Error { case path, notFound, link, notPlain, tooLarge }

    /// One file's bytes, opened without following any link at any level,
    /// checked to be a plain unshared file within the size limit.
    static func read(_ root: URL, _ path: String) throws(ReadFailure) -> Data {
        do {
            return try sceneRead(root: root.path, path: path)
        } catch let failure as SceneReadError {
            switch failure {
            case .Path: throw .path
            case .NotFound: throw .notFound
            case .Link: throw .link
            case .NotPlain: throw .notPlain
            case .TooLarge: throw .tooLarge
            }
        } catch {
            throw .notFound
        }
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

/// A `.senatescene` file: a zip of a scene bundle's files, stored
/// uncompressed (as `senate scene pack` makes it). Its central directory is
/// checked first, then each file is taken from its local header (which must
/// agree, CRC included) into an empty staging folder of the app's own, and
/// the result is scanned like any folder.
nonisolated enum SceneArchive {
    /// Check an archive's directory alone.
    static func check(_ data: Data) throws(SceneBundle.Problem) {
        do {
            try sceneCheckArchive(bytes: data)
        } catch let problem as SceneProblem {
            throw SceneBundle.Problem(problem)
        } catch {
            throw .archive(L("scenes.problem.archive_zip"))
        }
    }

    /// Unpack `archive` into the new folder `staging` and return the
    /// bundle's folder within it: the staging folder, or its one folder when
    /// everything was zipped inside one.
    static func unpack(_ archive: URL, into staging: URL) throws(SceneBundle.Problem) -> URL {
        do {
            return URL(fileURLWithPath: try sceneUnpack(archive: archive.path, staging: staging.path), isDirectory: true)
        } catch let problem as SceneProblem {
            throw SceneBundle.Problem(problem)
        } catch {
            throw .archive(L("scenes.problem.archive_zip"))
        }
    }
}
