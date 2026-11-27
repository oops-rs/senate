import SwiftUI
import SenateCore

/// Cross-committee multi-party conferences visible to you.
struct ConferencesView: View {
    let store: CoreStore

    private var filtered: [ConferenceSummaryView] {
        let q = store.query
        guard !q.isEmpty else { return store.state.conferences }
        return store.state.conferences.filter { item in
            item.topic.localizedCaseInsensitiveContains(q)
                || item.id.localizedCaseInsensitiveContains(q)
                || item.convener.localizedCaseInsensitiveContains(q)
                || item.seats.contains {
                    $0.target.localizedCaseInsensitiveContains(q)
                        || ($0.participant?.localizedCaseInsensitiveContains(q) ?? false)
                }
        }
    }

    var body: some View {
        content
            .toolbar {
                ToolbarSpacer(.flexible)
                ToolbarItem(placement: .primaryAction) { RefreshButton(store: store) }
            }
    }

    @ViewBuilder private var content: some View {
        let items = filtered
        if items.isEmpty {
            EmptyState(
                text: store.query.isEmpty ? L("conferences.empty") : L("sidebar.no_matches"),
                detail: store.query.isEmpty ? L("conferences.empty_detail") : nil
            )
        } else {
            ReadingColumn(spacing: Theme.Space.xs) {
                HStack(alignment: .firstTextBaseline) {
                    Text(verbatim: L("conferences.heading"))
                        .font(Theme.Fonts.display)
                        .foregroundStyle(Theme.onSurface)
                        .accessibilityAddTraits(.isHeader)
                    Spacer(minLength: Theme.Space.m)
                }
                .padding(.top, Theme.Space.xxxl)
                .padding(.leading, Theme.Space.xs)
                Color.clear.frame(height: Theme.Space.s)
                LazyVStack(alignment: .leading, spacing: Theme.Space.xs) {
                    ForEach(items) { item in
                        ConferenceRow(store: store, item: item)
                    }
                }
            }
        }
    }
}

private struct ConferenceRow: View {
    let store: CoreStore
    let item: ConferenceSummaryView
    @State private var hovered = false

    private var badgeColor: Color {
        switch item.state {
        case "in_session": Theme.primary
        case "convening": Theme.attention
        default: Theme.onSurfaceVariant
        }
    }

    private var symbol: String {
        switch item.state {
        case "in_session": "person.3.fill"
        case "convening": "person.3"
        default: "archivebox"
        }
    }

    var body: some View {
        Button {
            store.select(.conference(item.id))
        } label: {
            HStack(alignment: .top, spacing: Theme.Space.m) {
                Image(systemName: symbol)
                    .font(.system(size: 14))
                    .foregroundStyle(badgeColor)
                    .frame(width: 20, height: 20)
                    .accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 4) {
                    HStack(alignment: .firstTextBaseline, spacing: 6) {
                        Text(verbatim: item.stateLabel)
                            .font(Theme.Fonts.meta.weight(.medium))
                            .foregroundStyle(badgeColor)
                            .padding(.horizontal, 6)
                            .padding(.vertical, 1)
                            .background(badgeColor.opacity(0.12), in: Capsule())
                        Text(verbatim: item.topic)
                            .font(Theme.Fonts.text.weight(.medium))
                            .foregroundStyle(Theme.onSurface)
                            .lineLimit(1)
                    }
                    HStack(spacing: 8) {
                        Text(verbatim: item.id)
                            .font(Theme.Fonts.code)
                            .foregroundStyle(Theme.onSurfaceVariant)
                        Text(verbatim: L("conferences.seat_count", String(item.seats.count)))
                            .font(Theme.Fonts.meta)
                            .foregroundStyle(Theme.onSurfaceVariant)
                        if let report = item.report {
                            Text(verbatim: report.summary)
                                .font(Theme.Fonts.meta)
                                .foregroundStyle(Theme.onSurfaceVariant)
                                .lineLimit(1)
                        }
                    }
                }
                Spacer(minLength: 0)
            }
            .padding(.horizontal, Theme.Space.m)
            .padding(.vertical, Theme.Space.s)
            .background(
                hovered ? Theme.container : .clear,
                in: RoundedRectangle(cornerRadius: Theme.Radius.card)
            )
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .onHover { hovered = $0 }
    }
}
