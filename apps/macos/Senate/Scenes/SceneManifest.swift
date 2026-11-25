import Foundation
import SenateCore

/// A scene's `manifest.json` (docs/observe.md, "Scene host contract"),
/// validated strictly by the shared rules (`crates/senate-scene`, the same
/// ones `senate scene inspect` applies): exactly the contract's fields,
/// each of its type and within bounds, no duplicate keys, nothing else. A
/// scene is code from somewhere else; its name is only a label (a scene is
/// known by the identity the app gives it when it is installed, never by
/// what it calls itself).
nonisolated struct SceneManifest: Equatable, Sendable {
    /// The scene host contract this app speaks.
    static let hostVersion = Int(sceneHostVersion())

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

        /// The manifest's part of a shared-rules refusal, if it is one.
        init?(_ problem: SceneProblem) {
            switch problem {
            case .ManifestTooLarge: self = .tooLarge
            case .NotJson: self = .notJSON
            case let .DuplicateKey(key): self = .duplicateKey(key)
            case let .UnknownField(field): self = .unknownField(field)
            case let .MissingField(field): self = .missingField(field)
            case let .Invalid(field): self = .invalid(field)
            case let .NeedsNewerHost(version): self = .needsNewerApp(Int(version))
            default: return nil
            }
        }
    }

    init(_ info: SceneManifestInfo) {
        name = info.name
        version = info.version
        entry = info.entry
        observeMajor = Int(info.observeMajor)
        required = info.required
        optional = info.optional
        cardReads = info.cardReads
        cardWrites = info.cardWrites
        minHostVersion = Int(info.minHostVersion)
    }

    static func parse(_ data: Data) throws(Problem) -> SceneManifest {
        do {
            return SceneManifest(try sceneParseManifest(bytes: data))
        } catch let problem as SceneProblem {
            throw Problem(problem) ?? .notJSON
        } catch {
            throw .notJSON
        }
    }
}

/// Paths inside a scene bundle, by the shared rules: segments of
/// `[A-Za-z0-9_][A-Za-z0-9._-]*` (no dot files, no `.` or `..`, ASCII
/// only), at most 16 deep.
nonisolated enum ScenePath {
    static func validRelative(_ path: String) -> Bool { sceneValidPath(path: path) }
}
