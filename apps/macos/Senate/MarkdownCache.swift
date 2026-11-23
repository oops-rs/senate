import Foundation
import SwiftUI

/// Rendered messages, kept: by the text and its committee (the names its
/// mentions take), so a row that scrolls back into view, or a watch update
/// that touches another item, renders nothing again. Filled off the main
/// thread ahead of need (`prepare`), on the main thread only on a miss.
@MainActor
final class MarkdownCache {
    static let shared = MarkdownCache()

    private let rendered = NSCache<NSString, NSAttributedString>()
    /// Laid-out heights, by the text's key and the width.
    private var heights: [String: CGFloat] = [:]
    private var style: MarkdownRenderer.Style?
    /// For `SENATE_APP_PERF=session`: what ran on the main thread.
    var counters = (renders: 0, renderTime: Duration.zero, measures: 0, measureTime: Duration.zero)

    init() {
        rendered.countLimit = 4000
    }

    private func key(_ text: String, _ committee: String?) -> NSString {
        "\(committee ?? "")\u{1}\(text)" as NSString
    }

    var currentStyle: MarkdownRenderer.Style {
        if let style { return style }
        let s = MarkdownRenderer.Style(
            text: PlatformColor(Theme.onSurface), secondary: PlatformColor(Theme.onSurfaceVariant),
            accent: PlatformColor(Theme.primary), agent: PlatformColor(Theme.agent),
            codeBackground: PlatformColor(Theme.containerHigh))
        style = s
        return s
    }

    /// The message rendered, from the cache or now.
    func attributed(_ text: String, committee: String?) -> NSAttributedString {
        let k = key(text, committee)
        if let hit = rendered.object(forKey: k) { return hit }
        let start = ContinuousClock.now
        let made = MarkdownRenderer(style: currentStyle, committee: committee).render(text)
        counters.renders += 1
        counters.renderTime += ContinuousClock.now - start
        rendered.setObject(made, forKey: k)
        return made
    }

    /// Render these off the main thread, so they are ready when drawn.
    func prepare(_ texts: [String], committee: String?) {
        let missing = texts.filter { rendered.object(forKey: key($0, committee)) == nil }
        guard !missing.isEmpty else { return }
        let renderer = MarkdownRenderer(style: currentStyle, committee: committee)
        Task.detached(priority: .utility) {
            var done: [(String, NSAttributedString)] = []
            for text in missing { done.append((text, renderer.render(text))) }
            let ready = UncheckedPairs(done)
            await MainActor.run {
                for (text, made) in ready.pairs { self.rendered.setObject(made, forKey: self.key(text, committee)) }
            }
        }
    }

    func height(_ text: String, committee: String?, width: CGFloat) -> CGFloat? {
        heights["\(key(text, committee))\u{2}\(Int(width))"]
    }

    func setHeight(_ h: CGFloat, _ text: String, committee: String?, width: CGFloat) {
        if heights.count > 20_000 { heights.removeAll() }
        heights["\(key(text, committee))\u{2}\(Int(width))"] = h
    }
}

nonisolated private struct UncheckedPairs: @unchecked Sendable {
    let pairs: [(String, NSAttributedString)]
    init(_ pairs: [(String, NSAttributedString)]) { self.pairs = pairs }
}
