import AppKit
import Foundation
import Testing
@testable import Senate

/// The name and picture set in the app: kept on this Mac, the Senate
/// handle untouched.
@MainActor
struct LocalProfileTests {
    private func fresh() -> (LocalProfile, UserDefaults, URL) {
        let suite = "senate.tests.profile.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        let folder = FileManager.default.temporaryDirectory.appendingPathComponent(suite, isDirectory: true)
        return (LocalProfile(defaults: defaults, folder: folder), defaults, folder)
    }

    @Test func theNameFallsBackToTheHandleAndIsKept() {
        let (profile, defaults, folder) = fresh()
        #expect(profile.name("wendell") == "wendell")
        profile.setName("  Wendell Q  ")
        #expect(profile.name("wendell") == "Wendell Q")
        #expect(LocalProfile(defaults: defaults, folder: folder).name("wendell") == "Wendell Q")
        profile.setName("   ")
        #expect(profile.displayName == nil)
        #expect(profile.name("wendell") == "wendell")
    }

    @Test func aPictureIsCroppedSquareStoredAndRemoved() throws {
        let (profile, defaults, folder) = fresh()
        // A wide picture: 300 × 120.
        let wide = NSImage(size: NSSize(width: 300, height: 120))
        wide.lockFocus()
        NSColor.systemRed.setFill()
        NSRect(x: 0, y: 0, width: 300, height: 120).fill()
        wide.unlockFocus()
        let source = folder.appendingPathComponent("wide.png")
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        let rep = NSBitmapImageRep(data: wide.tiffRepresentation!)!
        try rep.representation(using: .png, properties: [:])!.write(to: source)

        try profile.setPicture(from: source)
        #expect(profile.picture?.size == NSSize(width: LocalProfile.side, height: LocalProfile.side))
        #expect(LocalProfile(defaults: defaults, folder: folder).picture != nil)

        profile.removePicture()
        #expect(profile.picture == nil)
        #expect(LocalProfile(defaults: defaults, folder: folder).picture == nil)
    }

    @Test func aFileThatIsNotAPictureIsRefused() throws {
        let (profile, _, folder) = fresh()
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        let text = folder.appendingPathComponent("note.png")
        try Data("not a picture".utf8).write(to: text)
        #expect(throws: (any Error).self) { try profile.setPicture(from: text) }
        #expect(profile.picture == nil)
    }
}
