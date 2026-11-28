import SwiftUI
import SenateCore

/// My Commissions (委派追踪): active and unread-completed tasks sponsored by you
/// across committees, ordered Blocked → Pending acceptance → Completed (unread) → In progress.
struct CommissionsView: View {
    let store: CoreStore

    private var filtered: [CommissionThread] {
        let q = store.query
        guard !q.isEmpty else { return store.state.commissions }
        return store.state.commissions.filter { item in
            item.title.localizedCaseInsensitiveContains(q)
                || item.committee.localizedCaseInsensitiveContains(q)
                || (item.phase?.localizedCaseInsensitiveContains(q) ?? false)
                || (item.nextOwner?.localizedCaseInsensitiveContains(q) ?? false)
                || (item.summaryLine?.localizedCaseInsensitiveContains(q) ?? false)
                || (item.originLabel?.localizedCaseInsensitiveContains(q) ?? false)
                || (item.owner?.localizedCaseInsensitiveContains(q) ?? false)
                || item.artifacts.contains {
                    $0.label.localizedCaseInsensitiveContains(q)
                        || $0.value.localizedCaseInsensitiveContains(q)
                }
                || "\(item.root)" == q
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
                text: store.query.isEmpty ? L("commissions.empty") : L("sidebar.no_matches"),
                detail: store.query.isEmpty ? L("commissions.empty_detail") : nil
            )
        } else {
            ReadingColumn(spacing: Theme.Space.xs) {
                HStack(alignment: .firstTextBaseline) {
                    Text(verbatim: L("commissions.heading"))
                        .font(Theme.Fonts.display)
                        .foregroundStyle(Theme.onSurface)
                        .accessibilityAddTraits(.isHeader)
                    Spacer(minLength: Theme.Space.m)
                }
                .padding(.top, Theme.Space.xxxl)
                .padding(.leading, Theme.Space.xs)
                Color.clear.frame(height: Theme.Space.s)
                LazyVStack(alignment: .leading, spacing: Theme.Space.xs) {
                    ForEach(items, id: \.commissionKey) { item in
                        CommissionRow(store: store, item: item)
                    }
                }
            }
        }
    }
}

private extension CommissionThread {
    var commissionKey: String { "\(committee):\(root)" }
}

private struct CommissionRow: View {
    let store: CoreStore
    let item: CommissionThread
    @State private var hovered = false

    private var badgeColor: Color {
        switch item.bucket {
        case "blocked", "pending_acceptance":
            Theme.attention
        case "completed_unread":
            Theme.primary
        default:
            Theme.onSurfaceVariant
        }
    }

    private var symbol: String {
        switch item.bucket {
        case "blocked":
            "exclamationmark.circle"
        case "pending_acceptance":
            "checkmark.circle"
        case "completed_unread":
            "checkmark.seal"
        default:
            "arrow.triangle.2.circlepath"
        }
    }

    var body: some View {
        let busy = store.isBusy(item.commissionKey)
        HStack(alignment: .top, spacing: Theme.Space.m) {
            Button {
                store.openThread(committee: item.committee, root: item.root)
            } label: {
                HStack(alignment: .top, spacing: Theme.Space.m) {
                    Image(systemName: symbol)
                        .font(.system(size: 14))
                        .foregroundStyle(badgeColor)
                        .frame(width: 20, height: 20)
                        .accessibilityHidden(true)
                    VStack(alignment: .leading, spacing: 4) {
                        HStack(alignment: .firstTextBaseline, spacing: 6) {
                            Text(verbatim: item.bucketLabel)
                                .font(Theme.Fonts.meta.weight(.medium))
                                .foregroundStyle(badgeColor)
                                .padding(.horizontal, 6)
                                .padding(.vertical, 1)
                                .background(badgeColor.opacity(0.12), in: Capsule())
                            if let phase = item.phase {
                                Text(verbatim: phase)
                                    .font(Theme.Fonts.meta)
                                    .foregroundStyle(Theme.onSurfaceVariant)
                                    .padding(.horizontal, 6)
                                    .padding(.vertical, 1)
                                    .background(Theme.containerHigh, in: Capsule())
                            }
                            MentionText(item.title)
                                .font(Theme.Fonts.text.weight(item.unread ? .semibold : .medium))
                                .foregroundStyle(Theme.onSurface)
                                .lineLimit(1)
                            if item.unread {
                                Circle()
                                    .fill(Theme.primary)
                                    .frame(width: 7, height: 7)
                                    .accessibilityLabel(L("a11y.unread"))
                            }
                        }
                        if let summary = item.summaryLine ?? item.originLabel {
                            MentionText(summary)
                                .font(Theme.Fonts.row)
                                .foregroundStyle(Theme.onSurfaceVariant)
                                .lineLimit(2)
                                .multilineTextAlignment(.leading)
                        }
                        if !item.artifacts.isEmpty || item.nextOwner != nil {
                            HStack(spacing: 6) {
                                if let nextOwner = item.nextOwner {
                                    Text(verbatim: L("thread.next_owner") + ": " + Mentions.shared.plain(nextOwner, committee: item.committee))
                                        .font(Theme.Fonts.meta)
                                        .foregroundStyle(Theme.onSurfaceVariant)
                                }
                                ForEach(item.artifacts, id: \.self) { artifact in
                                    Text(verbatim: "\(artifact.label): \(artifact.value)")
                                        .font(Theme.Fonts.code)
                                        .foregroundStyle(Theme.onSurfaceVariant)
                                        .padding(.horizontal, 5)
                                        .padding(.vertical, 1)
                                        .background(Theme.containerHigh, in: RoundedRectangle(cornerRadius: 4))
                                }
                            }
                            .lineLimit(1)
                        }
                        if let downstream = item.linkedDownstream {
                            let phase = downstream.phase.map { " · \($0)" } ?? ""
                            let steps = downstream.stepsTotal > 0 ? " · \(downstream.stepsDone)/\(downstream.stepsTotal)" : ""
                            Button {
                                store.openThread(committee: downstream.committee, root: downstream.root)
                            } label: {
                                Text(verbatim: L("thread.downstream") + ": \(downstream.committee) #\(downstream.root) · \(downstream.stateLabel)\(phase)\(steps)")
                                    .font(Theme.Fonts.meta)
                                    .foregroundStyle(Theme.primary)
                            }
                            .buttonStyle(.plain)
                        }
                        if let origin = item.originLabel, item.summaryLine != nil {
                            Text(verbatim: origin)
                                .font(Theme.Fonts.meta)
                                .foregroundStyle(Theme.onSurfaceVariant)
                        }
                    }
                    Spacer(minLength: Theme.Space.m)
                    HStack(spacing: 6) {
                        Text(verbatim: "\(item.committee) #\(item.root)")
                        Ago(date: item.since)
                    }
                    .font(Theme.Fonts.meta)
                    .foregroundStyle(Theme.onSurfaceVariant)
                    .lineLimit(1)
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .environment(\.mentionCommittee, item.committee)
            .accessibilityElement(children: .ignore)
            .accessibilityAddTraits(.isButton)
            .accessibilityLabel(Mentions.shared.plain(item.title, committee: item.committee))

            if item.awaitingAcceptance {
                Button(L("commissions.accept_close")) {
                    Task { await store.accept(committee: item.committee, root: item.root) }
                }
                .buttonStyle(.borderedProminent)
                .controlSize(.small)
                .disabled(busy || !store.state.online)
            }
        }
        .padding(.horizontal, Theme.Space.m)
        .padding(.vertical, 10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            hovered ? Theme.container : .clear,
            in: RoundedRectangle(cornerRadius: Theme.Radius.card)
        )
        .onHover { hovered = $0 }
    }
}
