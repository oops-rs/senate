import Foundation
import Security

/// A native client of the Senate's public Observe API (docs/observe.md): the
/// state snapshot, then the event stream (SSE) resumed from the last cursor,
/// a fresh snapshot after `409 resync_required` or `stream.reset`, and
/// jittered reconnects. It holds the bearer; nothing it hands on carries it.
/// Snapshots and frames are passed on as the JSON objects they are (checked
/// for version, shape and size, not interpreted): the scene reads them.
@MainActor
final class ObserveClient {
    enum Status { case ready, accessNeeded, unavailable, versionMismatch }

    var onSnapshot: ([String: Any]) -> Void = { _ in }
    var onFrame: ([String: Any]) -> Void = { _ in }
    var onStatus: (Status) -> Void = { _ in }

    private let grant: ObserveAccess.Grant
    private var task: Task<Void, Never>?
    private let session: URLSession

    init(grant: ObserveAccess.Grant) {
        self.grant = grant
        let config = URLSessionConfiguration.ephemeral
        config.httpCookieStorage = nil
        config.urlCache = nil
        config.requestCachePolicy = .reloadIgnoringLocalCacheData
        config.timeoutIntervalForRequest = 30
        config.timeoutIntervalForResource = .infinity
        session = URLSession(configuration: config)
    }

    var running: Bool { task != nil }

    func start() {
        guard task == nil else { return }
        task = Task { [weak self] in await self?.run() }
    }

    func stop() {
        task?.cancel()
        task = nil
    }

    private static let cursor = try? NSRegularExpression(pattern: "^[A-Za-z0-9_-]{21}[AQgw]$")
    private static func validCursor(_ value: Any?) -> String? {
        guard let s = value as? String, let re = cursor,
              re.firstMatch(in: s, range: NSRange(s.startIndex..., in: s)) != nil else { return nil }
        return s
    }

    private func request(_ path: String, cursor: String? = nil) -> URLRequest? {
        guard let base = URL(string: grant.url), let url = URL(string: path, relativeTo: base) else { return nil }
        var r = URLRequest(url: url)
        r.setValue("Bearer \(grant.token)", forHTTPHeaderField: "Authorization")
        if let cursor { r.setValue(cursor, forHTTPHeaderField: "Last-Event-ID") }
        return r
    }

    private func run() async {
        var cursor: String?
        var delay = 0.2
        while !Task.isCancelled {
            do {
                if cursor == nil {
                    guard let r = request("/v1/observe/state") else { onStatus(.accessNeeded); return }
                    let (data, response) = try await session.data(for: r)
                    let code = (response as? HTTPURLResponse)?.statusCode ?? 0
                    if code == 401 || code == 403 { onStatus(.accessNeeded); task = nil; return }
                    guard code == 200, data.count <= 4 * 1024 * 1024,
                          let snapshot = try JSONSerialization.jsonObject(with: data) as? [String: Any]
                    else { throw URLError(.badServerResponse) }
                    guard snapshot["schema_version"] as? Int == 1 else { onStatus(.versionMismatch); task = nil; return }
                    guard let c = Self.validCursor(snapshot["cursor"]) else { throw URLError(.cannotParseResponse) }
                    cursor = c
                    onSnapshot(snapshot)
                    onStatus(.ready)
                }
                guard let r = request("/v1/observe/events", cursor: cursor) else { return }
                let (bytes, response) = try await session.bytes(for: r)
                let code = (response as? HTTPURLResponse)?.statusCode ?? 0
                if code == 409 { cursor = nil; continue }
                if code == 401 || code == 403 { onStatus(.accessNeeded); task = nil; return }
                guard code == 200 else { throw URLError(.badServerResponse) }
                delay = 0.2
                // One JSON object on a `data:` line, after its `id:` line; comment
                // heartbeats (`:`) and blank lines carry nothing.
                var id: String?
                for try await line in bytes.lines {
                    if Task.isCancelled { return }
                    if line.hasPrefix("id:") {
                        id = String(line.dropFirst(3)).trimmingCharacters(in: .whitespaces)
                    } else if line.hasPrefix("data:") {
                        let payload = Data(line.dropFirst(5).utf8)
                        guard payload.count <= 64 * 1024,
                              let frame = try JSONSerialization.jsonObject(with: payload) as? [String: Any],
                              frame["schema_version"] as? Int == 1
                        else { throw URLError(.cannotParseResponse) }
                        if frame["kind"] as? String == "stream.reset" {
                            cursor = nil
                            break
                        }
                        guard let c = Self.validCursor(frame["cursor"]), c == id else { throw URLError(.cannotParseResponse) }
                        id = nil
                        if c == cursor { continue }
                        cursor = c
                        onFrame(frame)
                    }
                }
            } catch {
                if Task.isCancelled { return }
                if error is DecodingError || (error as? URLError)?.code == .cannotParseResponse { cursor = nil }
                onStatus(.unavailable)
            }
            try? await Task.sleep(for: .seconds(delay * Double.random(in: 0.75...1.25)))
            delay = min(delay * 2, 2)
        }
    }
}

/// The Town's observer grant: created by one click in the app (metadata
/// only, every committee, this Senate's direct links), kept in the Keychain,
/// never handed to the page.
enum ObserveAccess {
    struct Grant: Codable, Equatable {
        var url: String
        var token: String
        var id: String
        var expiresAt: String

        var expired: Bool {
            (ISO8601DateFormatter.withFractions.date(from: expiresAt) ?? ISO8601DateFormatter().date(from: expiresAt))
                .map { $0 <= Date() } ?? false
        }
    }

    private static let service = "dev.senate.app.town.observe"

    /// The grant kept for this Senate (`key` names it), if any and still valid.
    static func load(_ key: String) -> Grant? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service,
            kSecAttrAccount as String: key, kSecReturnData as String: true, kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var out: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &out) == errSecSuccess, let data = out as? Data,
              let grant = try? JSONDecoder().decode(Grant.self, from: data), !grant.expired else { return nil }
        return grant
    }

    static func save(_ grant: Grant, _ key: String) {
        guard let data = try? JSONEncoder().encode(grant) else { return }
        let match: [String: Any] = [kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service, kSecAttrAccount as String: key]
        SecItemDelete(match as CFDictionary)
        var add = match
        add[kSecValueData as String] = data
        add[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlockedThisDeviceOnly
        SecItemAdd(add as CFDictionary, nil)
    }

    static func forget(_ key: String) {
        let match: [String: Any] = [kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service, kSecAttrAccount as String: key]
        SecItemDelete(match as CFDictionary)
    }
}

extension ISO8601DateFormatter {
    static let withFractions: ISO8601DateFormatter = {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return f
    }()
}
