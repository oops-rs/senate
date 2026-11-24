import AppKit
import Foundation
import UniformTypeIdentifiers

/// A file or picture to send with a note to a session: dropped on its
/// composer or pasted into it. It is staged in the session's drops on send
/// (`sessionDrop`), and the note carries the path the agent can read.
struct Attachment: Identifiable, Equatable {
    let id = UUID()
    let name: String
    let data: Data
    /// A small picture of it, when it is an image.
    let thumbnail: NSImage?

    static func == (a: Attachment, b: Attachment) -> Bool { a.id == b.id }

    /// The daemon takes up to 20 MiB a file.
    static let limit = 20 * 1024 * 1024

    /// A file dropped or pasted by its URL.
    static func file(_ url: URL) throws -> Attachment {
        let size = (try? url.resourceValues(forKeys: [.fileSizeKey]).fileSize) ?? 0
        guard size <= limit else { throw CocoaError(.fileReadTooLarge) }
        let data = try Data(contentsOf: url)
        guard data.count <= limit else { throw CocoaError(.fileReadTooLarge) }
        return Attachment(name: url.lastPathComponent, data: data, thumbnail: Thumbnails.image(data))
    }

    /// A picture pasted as such (a screenshot on the clipboard): a PNG.
    static func picture(_ image: NSImage, at date: Date = Date()) -> Attachment? {
        guard let tiff = image.tiffRepresentation, let rep = NSBitmapImageRep(data: tiff),
              let png = rep.representation(using: .png, properties: [:]), png.count <= limit
        else { return nil }
        let f = DateFormatter()
        f.locale = Locale(identifier: "en_US_POSIX")
        f.dateFormat = "yyyyMMdd-HHmmss"
        return Attachment(name: "paste-\(f.string(from: date)).png", data: png, thumbnail: Thumbnails.image(png))
    }

    /// The note as sent: what was written, then each staged file's path on
    /// a line of its own, shell-quoted when it has spaces or quotes (the
    /// terminal relay's form), so the agent can open it.
    nonisolated static func noteBody(_ text: String, paths: [String]) -> String {
        let quoted = paths.map { path in
            path.contains(where: { $0.isWhitespace || $0 == "'" || $0 == "\"" })
                ? "'" + path.replacingOccurrences(of: "'", with: "'\\''") + "'"
                : path
        }
        let written = text.trimmingCharacters(in: .whitespacesAndNewlines)
        return ([written].filter { !$0.isEmpty } + [quoted.joined(separator: "\n")].filter { !$0.isEmpty })
            .joined(separator: "\n\n")
    }

    /// The image paths a note names (one a line, bare or quoted as above).
    nonisolated static func imagePaths(in text: String) -> [String] {
        let images: Set<String> = ["png", "jpg", "jpeg", "gif", "heic", "webp", "tiff", "tif"]
        return text.split(whereSeparator: \.isNewline).compactMap { line in
            var s = line.trimmingCharacters(in: .whitespaces)
            if s.count >= 2, s.hasPrefix("'"), s.hasSuffix("'") {
                s = String(s.dropFirst().dropLast()).replacingOccurrences(of: "'\\''", with: "'")
            }
            guard s.hasPrefix("/"), images.contains((s as NSString).pathExtension.lowercased()) else { return nil }
            return s
        }
    }
}

/// Small pictures of images sent to sessions, for the conversation's
/// cards: served to the page as `senate-app://thumb/<token>`, a token it
/// cannot guess, for images the app sent itself or that sit in a session's
/// drops under the Senate home. Nothing else is read.
@MainActor
enum Thumbnails {
    /// What this app staged, by the path the daemon gave back.
    private static var sent: [String: Data] = [:]
    /// Tokens the page may load, and the small PNG each serves.
    nonisolated(unsafe) private static var served: [String: Data] = [:]
    private static var tokens: [String: String] = [:]
    nonisolated private static let lock = NSLock()

    static func remember(path: String, data: Data) { sent[path] = data }

    /// The page's URL for a small picture of the image at `path`, when it
    /// is one the app sent or one in a session's drops.
    static func url(for path: String, home: String?) -> String? {
        if let token = tokens[path] { return "senate-app://thumb/\(token)" }
        guard let data = sent[path] ?? dropped(path, home: home), let png = small(data) else { return nil }
        let token = UUID().uuidString.lowercased()
        tokens[path] = token
        lock.withLock { served[token] = png }
        return "senate-app://thumb/\(token)"
    }

    /// The scheme handler's read: what a token serves.
    nonisolated static func data(token: String) -> Data? { lock.withLock { served[token] } }

    /// A file under `<home>/hosts/*/drops/`, followed to where it really is.
    private static func dropped(_ path: String, home: String?) -> Data? {
        guard let home else { return nil }
        let hosts = URL(fileURLWithPath: home).appendingPathComponent("hosts").resolvingSymlinksInPath().path
        let real = URL(fileURLWithPath: path).resolvingSymlinksInPath()
        let parts = real.pathComponents
        guard real.path.hasPrefix(hosts + "/"), parts.count >= 3, parts[parts.count - 2] == "drops",
              let size = try? real.resourceValues(forKeys: [.fileSizeKey]).fileSize, size <= Attachment.limit
        else { return nil }
        return try? Data(contentsOf: real)
    }

    nonisolated static func image(_ data: Data) -> NSImage? {
        guard let image = NSImage(data: data), image.isValid else { return nil }
        return image
    }

    /// At most 320 points on its longer side, as PNG.
    private static func small(_ data: Data) -> Data? {
        guard let image = image(data) else { return nil }
        let side: CGFloat = 320
        let scale = min(1, side / max(image.size.width, image.size.height, 1))
        let size = NSSize(width: max(1, image.size.width * scale), height: max(1, image.size.height * scale))
        let out = NSImage(size: size)
        out.lockFocus()
        NSGraphicsContext.current?.imageInterpolation = .high
        image.draw(in: NSRect(origin: .zero, size: size))
        out.unlockFocus()
        guard let tiff = out.tiffRepresentation, let rep = NSBitmapImageRep(data: tiff) else { return nil }
        return rep.representation(using: .png, properties: [:])
    }
}
