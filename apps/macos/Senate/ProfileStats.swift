import Charts
import SenateCore
import SwiftUI

/// The person's activity on this Mac, under "This Senate" on the profile:
/// a heatmap of days, the range's headline numbers, the hours of the day
/// they work, the committees and repos they spend it on, their streaks.
struct ProfileStats: View {
    let store: CoreStore
    @AppStorage("profile.statsRange") private var range = StatsRange.month
    @State private var year: StatsReport?
    @State private var report: StatsReport?
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.l) {
            HStack {
                Text(verbatim: L("stats.title"))
                    .font(Theme.Fonts.heading)
                    .accessibilityAddTraits(.isHeader)
                Spacer()
                Picker(L("stats.range"), selection: $range) {
                    ForEach(StatsRange.allCases) { Text(verbatim: $0.title).tag($0) }
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .fixedSize()
            }
            if let error {
                Text(verbatim: error).foregroundStyle(Theme.onSurfaceVariant)
            } else if let year, let report {
                if year.indexing || report.indexing {
                    Indexing(report: year.indexing ? year : report)
                }
                Heatmap(days: year.days, through: Date())
                Text(verbatim: L("stats.streaks", Int64(year.currentStreak), Int64(year.longestStreak)))
                    .font(Theme.Fonts.meta)
                    .foregroundStyle(Theme.onSurfaceVariant)
                if report.days.allSatisfy({ $0.activeSecs == 0 && $0.prompts == 0 && $0.messages == 0 }) {
                    Text(verbatim: L("stats.empty")).foregroundStyle(Theme.onSurfaceVariant)
                } else {
                    Headline(days: report.days)
                    Hours(hours: report.hours)
                    HStack(alignment: .top, spacing: Theme.Space.xl) {
                        Ranked(title: L("stats.top_committees"), ranks: report.topCommittees)
                            .frame(maxWidth: .infinity, alignment: .leading)
                        Ranked(title: L("stats.top_repos"), ranks: report.topRepos)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }
                ForEach(report.notes, id: \.self) { note in
                    Text(verbatim: note).font(Theme.Fonts.meta).foregroundStyle(Theme.onSurfaceVariant)
                }
            } else {
                ProgressView().controlSize(.small)
            }
        }
        .font(Theme.Fonts.text)
        .foregroundStyle(Theme.onSurface)
        .task(id: range) { await load() }
    }

    private func load() async {
        guard let core = store.core else { return }
        let tz = TimeZone.current.identifier
        let today = Date()
        do {
            // The year once (the heatmap and streaks), the range for the rest.
            if year == nil {
                year = try await core.stats(from: StatsRange.day(today, minus: 364), to: StatsRange.day(today), tz: tz)
            }
            report = range == .year
                ? year
                : try await core.stats(from: StatsRange.day(today, minus: range.days - 1), to: StatsRange.day(today), tz: tz)
            error = nil
        } catch {
            self.error = describe(error)
        }
        // Still reading the history: read again in a moment.
        if year?.indexing == true || report?.indexing == true {
            try? await Task.sleep(for: .seconds(3))
            year = nil
            if !Task.isCancelled { await load() }
        }
    }
}

enum StatsRange: String, CaseIterable, Identifiable {
    case week, month, year
    var id: String { rawValue }
    var days: Int { switch self { case .week: 7; case .month: 30; case .year: 365 } }
    var title: String { L("stats.range_\(rawValue)") }

    nonisolated static func day(_ date: Date, minus days: Int = 0) -> String {
        let cal = Calendar.current
        let d = cal.date(byAdding: .day, value: -days, to: date) ?? date
        let c = cal.dateComponents([.year, .month, .day], from: d)
        return String(format: "%04d-%02d-%02d", c.year ?? 0, c.month ?? 0, c.day ?? 0)
    }
}

/// Five numbers for the range, and the streaks.
struct Headline: View {
    let days: [StatsDay]

    var body: some View {
        let sum = { (k: KeyPath<StatsDay, UInt64>) in days.reduce(0) { $0 + $1[keyPath: k] } }
        let numbers: [(String, String)] = [
            (Self.duration(sum(\.activeSecs)), L("stats.active")),
            (String(sum(\.sessions)), L("stats.sessions")),
            (String(sum(\.prompts)), L("stats.prompts")),
            (String(sum(\.toolCalls)), L("stats.tool_calls")),
            (String(sum(\.messages)), L("stats.messages")),
        ]
        LazyVGrid(columns: [GridItem(.adaptive(minimum: 110), alignment: .leading)], alignment: .leading,
                  spacing: Theme.Space.m) {
            ForEach(numbers, id: \.1) { value, label in
                VStack(alignment: .leading, spacing: 2) {
                    Text(verbatim: value)
                        .font(.system(size: 22, weight: .semibold, design: .rounded))
                        .monospacedDigit()
                    Text(verbatim: label)
                        .font(Theme.Fonts.meta)
                        .foregroundStyle(Theme.onSurfaceVariant)
                }
                .accessibilityElement(children: .combine)
            }
        }
    }

    static func duration(_ secs: UInt64) -> String {
        let h = secs / 3600, m = (secs % 3600) / 60
        return h > 0 ? "\(h)h \(m)m" : "\(m)m"
    }
}

/// A day a square, a week a column (Monday at the top), darker the longer
/// the person was active; a year, or half of one when narrow.
struct Heatmap: View {
    let days: [StatsDay]
    /// The last day shown.
    let through: Date

    var body: some View {
        let cells = Self.cells(days, through: through)
        let top = cells.map(\.secs).max() ?? 0
        ViewThatFits(in: .horizontal) {
            grid(cells, top: top, weeks: 53)
            grid(cells, top: top, weeks: 26)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(L("stats.heatmap"))
        .accessibilityValue(summary(cells))
    }

    private func grid(_ cells: [Cell], top: UInt64, weeks: Int) -> some View {
        let last = cells.last?.week ?? 0
        let shown = cells.filter { $0.week > last - weeks }
        let side: CGFloat = 11, gap: CGFloat = 3
        return Canvas { context, _ in
            for cell in shown {
                let x = CGFloat(cell.week - (last - weeks + 1)) * (side + gap)
                let y = CGFloat(cell.weekday) * (side + gap)
                let rect = CGRect(x: x, y: y, width: side, height: side)
                let color = cell.secs == 0 ? Theme.containerHigh : Theme.primary.opacity(Self.level(cell.secs, top))
                context.fill(Path(roundedRect: rect, cornerRadius: 2.5), with: .color(color))
            }
        }
        .frame(width: CGFloat(weeks) * (side + gap) - gap, height: 7 * (side + gap) - gap)
    }

    /// Four steps of colour, by how the day compares with the busiest.
    static func level(_ secs: UInt64, _ top: UInt64) -> Double {
        guard top > 0 else { return 0 }
        let r = Double(secs) / Double(top)
        return r > 0.75 ? 1 : r > 0.5 ? 0.75 : r > 0.25 ? 0.5 : 0.3
    }

    struct Cell: Identifiable {
        let id: String
        let week: Int
        let weekday: Int
        let secs: UInt64
    }

    /// Every day of the year through `through`, those without activity
    /// at zero; weeks counted from the Monday of the first.
    static func cells(_ days: [StatsDay], through: Date) -> [Cell] {
        var cal = Calendar(identifier: .gregorian)
        cal.firstWeekday = 2
        let end = cal.startOfDay(for: through)
        guard let first = cal.date(byAdding: .day, value: -364, to: end),
              let start = cal.dateInterval(of: .weekOfYear, for: first)?.start else { return [] }
        let secs = Dictionary(days.map { ($0.date, $0.activeSecs) }, uniquingKeysWith: +)
        var out: [Cell] = []
        var day = first
        while day <= end {
            let key = StatsRange.day(day)
            let offset = cal.dateComponents([.day], from: start, to: day).day ?? 0
            out.append(Cell(id: key, week: offset / 7, weekday: offset % 7, secs: secs[key] ?? 0))
            guard let next = cal.date(byAdding: .day, value: 1, to: day) else { break }
            day = next
        }
        return out
    }

    private func summary(_ cells: [Cell]) -> String {
        let active = cells.filter { $0.secs > 0 }
        guard let best = active.max(by: { $0.secs < $1.secs }) else { return L("stats.empty") }
        return L("stats.heatmap_summary", Int64(active.count), Int64(cells.count), best.id,
                 Headline.duration(best.secs))
    }
}

/// When in the day the person works: a bar an hour.
private struct Hours: View {
    let hours: [UInt64]

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.s) {
            Text(verbatim: L("stats.hours")).font(Theme.Fonts.label).foregroundStyle(Theme.onSurfaceVariant)
            Chart(Array(hours.enumerated()), id: \.offset) { hour, count in
                BarMark(x: .value("hour", hour), y: .value("count", count), width: .fixed(7))
                    .foregroundStyle(Theme.primary.opacity(0.8))
                    .cornerRadius(2)
            }
            .chartXScale(domain: 0...23)
            .chartXAxis {
                AxisMarks(values: [0, 6, 12, 18, 23]) { v in
                    AxisValueLabel { Text(verbatim: "\(v.as(Int.self) ?? 0)") }
                }
            }
            .chartYAxis(.hidden)
            .frame(maxWidth: .infinity)
            .frame(height: 90)
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(L("stats.hours"))
            .accessibilityValue(summary)
        }
    }

    private var summary: String {
        guard let top = hours.enumerated().max(by: { $0.element < $1.element }), top.element > 0 else {
            return L("stats.empty")
        }
        return L("stats.hours_summary", String(format: "%02d:00", top.offset))
    }
}

/// The few things the person spent the most time on.
private struct Ranked: View {
    let title: String
    let ranks: [StatsRank]

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.s) {
            Text(verbatim: title).font(Theme.Fonts.label).foregroundStyle(Theme.onSurfaceVariant)
            if ranks.isEmpty {
                Text(verbatim: "—").foregroundStyle(Theme.onSurfaceVariant)
            }
            Grid(alignment: .leadingFirstTextBaseline, horizontalSpacing: Theme.Space.m, verticalSpacing: 6) {
                ForEach(Array(ranks.prefix(5).enumerated()), id: \.offset) { i, r in
                    GridRow {
                        Text(verbatim: "\(i + 1)").foregroundStyle(Theme.onSurfaceVariant).monospacedDigit()
                        Text(verbatim: r.name).lineLimit(1).truncationMode(.middle)
                        Text(verbatim: Headline.duration(r.count))
                            .foregroundStyle(Theme.onSurfaceVariant)
                            .monospacedDigit()
                            .gridColumnAlignment(.trailing)
                    }
                }
            }
        }
    }
}

/// The history is still being read: what is shown is partial.
private struct Indexing: View {
    let report: StatsReport

    var body: some View {
        HStack(spacing: Theme.Space.s) {
            if let done = report.indexed, let total = report.total, total > 0 {
                ProgressView(value: Double(done), total: Double(total)).frame(width: 120)
                Text(verbatim: L("stats.indexing_progress", Int64(done), Int64(total)))
            } else {
                ProgressView().controlSize(.small)
                Text(verbatim: L("stats.indexing"))
            }
        }
        .font(Theme.Fonts.meta)
        .foregroundStyle(Theme.onSurfaceVariant)
    }
}
