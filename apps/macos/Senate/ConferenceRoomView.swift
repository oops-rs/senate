import SwiftUI
import SenateCore

/// Live multi-party conference room: seat bar (`Seated` / `Delegate` / `Pending` / `Absent`),
/// isolated room transcript rendered via `ConversationPage` (`senate-markdown`),
/// floor round info, composer, and convener controls (Start Without Absent / Adjourn).
struct ConferenceRoomView: View {
    @Bindable var store: CoreStore
    let id: String

    @State private var showAdjournSheet = false
    @State private var adjournSummary = ""
    @State private var adjournDecisions = ""

    private var summary: ConferenceSummaryView? {
        if let detail = store.conference, detail.summary.id == id {
            return detail.summary
        }
        return store.state.conferences.first { $0.id == id }
    }

    private var isAdjourned: Bool {
        summary?.state == "adjourned"
    }

    var body: some View {
        VStack(spacing: 0) {
            if let summary {
                SeatStatusBar(summary: summary)
            }
            if let detail = store.conference, detail.summary.id == id {
                ConversationPane(store: store, source: .conference(id))
                    .overlay(alignment: .bottom) {
                        LinearGradient(
                            colors: [Theme.surface.opacity(0), Theme.surface],
                            startPoint: .top,
                            endPoint: .bottom
                        )
                        .frame(height: Theme.Space.xl)
                        .allowsHitTesting(false)
                        .accessibilityHidden(true)
                    }
                if let report = detail.summary.report {
                    ReportBanner(report: report)
                }
            } else if let error = store.conferenceError {
                VStack(alignment: .leading, spacing: Theme.Space.xs) {
                    Text(verbatim: L("conference.failed"))
                        .font(Theme.Fonts.heading)
                        .foregroundStyle(Theme.onSurface)
                    Text(verbatim: error)
                        .font(Theme.Fonts.text)
                        .foregroundStyle(Theme.onSurfaceVariant)
                        .textSelection(.enabled)
                }
                .frame(maxWidth: Theme.readingWidth, alignment: .leading)
                .padding(Theme.Space.xl)
                Spacer()
            } else {
                ProgressView()
                    .controlSize(.small)
                    .frame(maxWidth: .infinity)
                    .padding(.top, Theme.Space.xl)
                Spacer()
            }

            if isAdjourned {
                Text(verbatim: L("conference.adjourned_readonly"))
                    .font(Theme.Fonts.meta)
                    .foregroundStyle(Theme.onSurfaceVariant)
                    .padding(.vertical, Theme.Space.s)
            } else if summary?.state == "in_session" {
                MessageComposer(
                    text: Binding(
                        get: { store.conferenceDrafts[id] ?? "" },
                        set: { store.conferenceDrafts[id] = $0.isEmpty ? nil : $0 }
                    ),
                    label: L("inbox.reply"),
                    placeholder: L("conference.reply_placeholder"),
                    hint: nil,
                    busy: store.isBusy("conf:\(id)"),
                    online: store.state.online
                ) {
                    Task { await store.sendConferenceMessage(id) }
                }
            }
        }
        .task(id: id) { await store.loadConference(id) }
        .toolbar { toolbar }
        .sheet(isPresented: $showAdjournSheet) {
            AdjournSheet(
                summary: $adjournSummary,
                decisions: $adjournDecisions,
                onCancel: { showAdjournSheet = false },
                onConfirm: {
                    let s = adjournSummary.trimmingCharacters(in: .whitespacesAndNewlines)
                    let d = adjournDecisions
                        .split(separator: "\n")
                        .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
                        .filter { !$0.isEmpty }
                    showAdjournSheet = false
                    Task { await store.adjournConference(id, summary: s, decisions: d) }
                }
            )
        }
    }

    @ToolbarContentBuilder private var toolbar: some ToolbarContent {
        let busy = store.isBusy("conf:\(id)")
        ToolbarItem(placement: .navigation) {
            VStack(alignment: .leading, spacing: 2) {
                Text(verbatim: summary?.topic ?? id)
                    .font(Theme.Fonts.heading)
                    .foregroundStyle(Theme.onSurface)
                    .lineLimit(1)
                if let summary {
                    Text(verbatim: "\(summary.id) · \(summary.stateLabel)")
                        .font(Theme.Fonts.meta)
                        .foregroundStyle(Theme.onSurfaceVariant)
                }
            }
        }
        .sharedBackgroundVisibility(.hidden)
        ToolbarSpacer(.flexible)
        ToolbarItemGroup(placement: .primaryAction) {
            if summary?.state == "convening" {
                Button {
                    Task { await store.startConference(id) }
                } label: {
                    Text(verbatim: L("conference.start_without_absent"))
                }
                .disabled(busy)
            }
            if let summary, summary.state != "adjourned" {
                Button {
                    adjournSummary = ""
                    adjournDecisions = ""
                    showAdjournSheet = true
                } label: {
                    Text(verbatim: L("conference.adjourn"))
                }
                .disabled(busy)
            }
        }
    }
}

private struct SeatStatusBar: View {
    let summary: ConferenceSummaryView

    private func badgeColor(_ status: String) -> Color {
        switch status {
        case "seated": Theme.primary
        case "delegate": Theme.primary
        case "absent": Theme.onSurfaceVariant
        default: Theme.attention
        }
    }

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                ForEach(summary.seats, id: \.target) { seat in
                    let color = badgeColor(seat.status)
                    HStack(spacing: 4) {
                        Text(verbatim: seat.target)
                            .font(Theme.Fonts.meta.weight(.medium))
                            .foregroundStyle(Theme.onSurface)
                        Text(verbatim: "·")
                            .foregroundStyle(Theme.onSurfaceVariant)
                        Text(verbatim: seat.statusLabel)
                            .font(Theme.Fonts.meta)
                            .foregroundStyle(color)
                        if let participant = seat.participant {
                            Text(verbatim: Mentions.shared.plain(participant, committee: seat.committee))
                                .font(Theme.Fonts.meta)
                                .foregroundStyle(Theme.onSurfaceVariant)
                        }
                    }
                    .padding(.horizontal, 8)
                    .padding(.vertical, 3)
                    .background(color.opacity(0.10), in: Capsule())
                }
            }
            .padding(.horizontal, Theme.Space.xl)
            .padding(.vertical, Theme.Space.xs)
        }
        .background(Theme.surface)
    }
}

private struct ReportBanner: View {
    let report: ConferenceReportView

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(verbatim: L("conference.report_heading"))
                .font(Theme.Fonts.label)
                .foregroundStyle(Theme.primary)
            Text(verbatim: report.summary)
                .font(Theme.Fonts.text)
                .foregroundStyle(Theme.onSurface)
                .textSelection(.enabled)
            ForEach(report.decisions, id: \.self) { decision in
                Text(verbatim: "• \(decision)")
                    .font(Theme.Fonts.row)
                    .foregroundStyle(Theme.onSurfaceVariant)
            }
        }
        .frame(maxWidth: Theme.readingWidth, alignment: .leading)
        .padding(Theme.Space.m)
        .background(Theme.container, in: RoundedRectangle(cornerRadius: Theme.Radius.card))
        .padding(.horizontal, Theme.Space.xl)
        .padding(.bottom, Theme.Space.s)
    }
}

private struct AdjournSheet: View {
    @Binding var summary: String
    @Binding var decisions: String
    let onCancel: () -> Void
    let onConfirm: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.m) {
            Text(verbatim: L("conference.adjourn_title"))
                .font(Theme.Fonts.heading)
            TextField(L("conference.adjourn_summary_placeholder"), text: $summary, axis: .vertical)
                .lineLimit(3...6)
            TextField(L("conference.adjourn_decisions_placeholder"), text: $decisions, axis: .vertical)
                .lineLimit(2...5)
            HStack {
                Spacer()
                Button(L("conference.cancel"), action: onCancel)
                Button(L("conference.adjourn_submit"), action: onConfirm)
                    .disabled(summary.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(Theme.Space.xl)
        .frame(width: 440)
    }
}
