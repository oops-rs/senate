import AppKit
import Foundation
import ImageIO
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
    nonisolated static let limit = 20 * 1024 * 1024

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
/// drops under the Senate home (`hosts/<host>/drops/<drop>/<file>`).
/// Nothing else is read. Only small PNGs are kept, at most `capacity`, the
/// least recently used going first; a dropped image is made again from its
/// file when asked for.
nonisolated enum Thumbnails {
    static let capacity = 200
    /// 320 points on the longer side, at 2x.
    static let maxPixels = 640
    /// Larger than this is not an image worth reading.
    static let maxSourcePixels = 400_000_000

    private struct Entry {
        let token: String
        let png: Data
    }
    nonisolated(unsafe) private static var byPath: [String: Entry] = [:]
    nonisolated(unsafe) private static var byToken: [String: String] = [:]
    nonisolated(unsafe) private static var order: [String] = []
    private static let lock = NSLock()

    /// A file the app staged: its small picture, made now from its bytes
    /// (the bytes themselves are not kept).
    static func remember(path: String, data: Data) {
        guard let source = CGImageSourceCreateWithData(data as CFData, nil), let png = small(source) else { return }
        store(path: path, png: png)
    }

    /// The page's URL for a small picture of the image at `path`, when it
    /// is one the app sent or one in a session's drops.
    static func url(for path: String, home: String?) -> String? {
        if let entry = lock.withLock({ touch(path) }) { return "senate-app://thumb/\(entry.token)" }
        guard let file = drop(path, home: home), let source = CGImageSourceCreateWithURL(file as CFURL, nil),
              let png = small(source)
        else { return nil }
        return "senate-app://thumb/\(store(path: path, png: png).token)"
    }

    /// The scheme handler's read: what a token serves.
    static func data(token: String) -> Data? {
        lock.withLock { byToken[token].flatMap { touch($0) }?.png }
    }

    /// The file at `path` when it is a session's drop under `home`:
    /// `<home>/hosts/<host>/drops/<drop>/<file>`, exactly, symlinks
    /// followed, a regular file of at most 20 MiB.
    static func drop(_ path: String, home: String?) -> URL? {
        guard let home else { return nil }
        let hosts = URL(fileURLWithPath: home).appendingPathComponent("hosts").resolvingSymlinksInPath()
        let real = URL(fileURLWithPath: path).resolvingSymlinksInPath()
        let base = hosts.pathComponents
        let parts = real.pathComponents
        guard parts.count == base.count + 4, Array(parts.prefix(base.count)) == base,
              parts[base.count + 1] == "drops",
              let values = try? real.resourceValues(forKeys: [.isRegularFileKey, .fileSizeKey]),
              values.isRegularFile == true, (values.fileSize ?? .max) <= Attachment.limit
        else { return nil }
        return real
    }

    /// A small PNG of the image, read with ImageIO (never decoded whole);
    /// nil for what is not an image or is absurdly large.
    static func small(_ source: CGImageSource) -> Data? {
        guard let props = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any],
              let w = props[kCGImagePropertyPixelWidth] as? Int, let h = props[kCGImagePropertyPixelHeight] as? Int,
              w > 0, h > 0, w * h <= maxSourcePixels
        else { return nil }
        let options: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceThumbnailMaxPixelSize: maxPixels,
            kCGImageSourceShouldCacheImmediately: true,
        ]
        guard let image = CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary) else { return nil }
        let out = NSMutableData()
        guard let dest = CGImageDestinationCreateWithData(out, UTType.png.identifier as CFString, 1, nil) else { return nil }
        CGImageDestinationAddImage(dest, image, nil)
        return CGImageDestinationFinalize(dest) ? out as Data : nil
    }

    /// The composer's chip: a small picture of what is about to be sent.
    static func image(_ data: Data) -> NSImage? {
        guard let source = CGImageSourceCreateWithData(data as CFData, nil), let png = small(source) else { return nil }
        return NSImage(data: png)
    }

    @discardableResult
    private static func store(path: String, png: Data) -> Entry {
        lock.withLock {
            if let entry = touch(path) { return entry }
            let entry = Entry(token: UUID().uuidString.lowercased(), png: png)
            byPath[path] = entry
            byToken[entry.token] = path
            order.append(path)
            while order.count > capacity {
                let old = order.removeFirst()
                if let gone = byPath.removeValue(forKey: old) { byToken[gone.token] = nil }
            }
            return entry
        }
    }

    /// Mark `path` as just used (under the lock).
    private static func touch(_ path: String) -> Entry? {
        guard let entry = byPath[path] else { return nil }
        if let i = order.firstIndex(of: path) { order.remove(at: i); order.append(path) }
        return entry
    }

    #if DEBUG
    static var count: Int { lock.withLock { byPath.count } }
    #endif
}
