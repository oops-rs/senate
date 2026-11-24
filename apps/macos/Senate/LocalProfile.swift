import AppKit
import SwiftUI

/// The name and picture the person sets in this app: kept by the app on
/// this Mac (the name in its settings, the picture as a small round PNG in
/// Application Support), shown in the sidebar's foot, the account popover
/// and the profile page. Their Senate handle is not changed by it.
@MainActor
@Observable
final class LocalProfile {
    /// What to call them here; nil: their Senate handle.
    private(set) var displayName: String?
    /// Their picture, already round; nil: their initial.
    private(set) var picture: NSImage?

    @ObservationIgnored private let defaults: UserDefaults
    @ObservationIgnored private let folder: URL
    private static let nameKey = "profile.displayName"
    static let side: CGFloat = 256

    init(defaults: UserDefaults = .standard, folder: URL? = nil) {
        self.defaults = defaults
        self.folder = folder ?? FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("Senate", isDirectory: true)
        displayName = defaults.string(forKey: Self.nameKey).flatMap { $0.isEmpty ? nil : $0 }
        picture = NSImage(contentsOf: pictureURL)
    }

    private var pictureURL: URL { folder.appendingPathComponent("avatar.png") }

    /// The name shown: theirs here, else the Senate's.
    func name(_ handle: String?) -> String { displayName ?? handle ?? "Senate" }

    func setName(_ name: String) {
        let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
        displayName = trimmed.isEmpty ? nil : String(trimmed.prefix(60))
        defaults.set(displayName, forKey: Self.nameKey)
    }

    /// Take a picture from a file: cropped to its centre square, drawn in
    /// a circle, at 256 points; stored and shown.
    func setPicture(from url: URL) throws {
        guard let source = NSImage(contentsOf: url) else { throw CocoaError(.fileReadCorruptFile) }
        let round = Self.round(source)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        guard let tiff = round.tiffRepresentation, let bitmap = NSBitmapImageRep(data: tiff),
              let png = bitmap.representation(using: .png, properties: [:]) else {
            throw CocoaError(.fileWriteUnknown)
        }
        try png.write(to: pictureURL, options: .atomic)
        picture = round
    }

    func removePicture() {
        try? FileManager.default.removeItem(at: pictureURL)
        picture = nil
    }

    static func round(_ source: NSImage) -> NSImage {
        let size = source.size
        let edge = min(size.width, size.height)
        let crop = NSRect(x: (size.width - edge) / 2, y: (size.height - edge) / 2, width: edge, height: edge)
        let out = NSImage(size: NSSize(width: side, height: side))
        out.lockFocus()
        NSGraphicsContext.current?.imageInterpolation = .high
        NSBezierPath(ovalIn: NSRect(x: 0, y: 0, width: side, height: side)).addClip()
        source.draw(in: NSRect(x: 0, y: 0, width: side, height: side), from: crop, operation: .copy, fraction: 1)
        out.unlockFocus()
        return out
    }
}

/// Edit the name and picture this app shows for the person.
struct ProfileEditor: View {
    let profile: LocalProfile
    let handle: String
    let done: () -> Void
    @State private var name = ""
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.l) {
            Text(verbatim: L("profile.edit_title"))
                .font(Theme.Fonts.heading)
                .accessibilityAddTraits(.isHeader)
            HStack(alignment: .center, spacing: Theme.Space.l) {
                Avatar(name: name.isEmpty ? handle : name, online: nil, size: 72, picture: profile.picture)
                VStack(alignment: .leading, spacing: Theme.Space.s) {
                    Button(L("profile.picture_choose")) { choose() }
                    if profile.picture != nil {
                        Button(L("profile.picture_remove"), role: .destructive) { profile.removePicture() }
                    }
                }
            }
            VStack(alignment: .leading, spacing: Theme.Space.xs) {
                Text(verbatim: L("profile.display_name")).font(Theme.Fonts.label)
                TextField(handle, text: $name)
                    .textFieldStyle(.roundedBorder)
                    .onSubmit { save() }
            }
            Text(verbatim: L("profile.local_note", handle))
                .font(Theme.Fonts.meta)
                .foregroundStyle(Theme.onSurfaceVariant)
                .fixedSize(horizontal: false, vertical: true)
            if let error {
                Text(verbatim: error).font(Theme.Fonts.meta).foregroundStyle(Theme.danger)
            }
            HStack {
                Spacer()
                Button(L("desk.cancel")) { done() }
                    .keyboardShortcut(.cancelAction)
                Button(L("profile.done")) { save() }
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(Theme.Space.xl)
        .frame(width: 400)
        .onAppear { name = profile.displayName ?? "" }
    }

    private func save() {
        profile.setName(name)
        done()
    }

    private func choose() {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = [.image]
        panel.allowsMultipleSelection = false
        guard panel.runModal() == .OK, let url = panel.url else { return }
        do {
            try profile.setPicture(from: url)
            error = nil
        } catch {
            self.error = describe(error)
        }
    }
}
