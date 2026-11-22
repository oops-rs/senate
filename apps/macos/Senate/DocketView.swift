import SwiftUI
import SenateCore

/// What awaits the Speaker of a Senate one of your agents presides over.
///
/// Read-only on purpose: routing an item to an agent, covering for an away
/// agent from its checkpoint, and putting an item to a person are the
/// presiding agent's own tools, run by that agent in its session. This
/// section shows what is waiting for it, so a person can see a closed or
/// stuck presiding session; it has no buttons that change anything.
struct DocketView: View {
    @Bindable var store: CoreStore

    var body: some View {
        ReadingColumn {
            HStack(alignment: .top) {
                PageHeading(text: L("tab.docket"), note: L("docket.note"))
                Spacer(minLength: Theme.Space.m)
                if store.state.docket.count > 1 {
                    Picker(L("docket.senate"), selection: senateChoice) {
                        ForEach(store.state.docket) { senate in
                            Text(verbatim: senate.title).tag(senate.senate)
                        }
                    }
                    .pickerStyle(.menu)
                    .labelsHidden()
                    .fixedSize()
                    .padding(.top, Theme.Space.xl)
                }
            }
            if let senate = store.docket, !senate.committees.isEmpty {
                if let speaker = senate.speaker {
                    MentionText(L("docket.speaker", speaker))
                        .font(Theme.Fonts.row)
                        .foregroundStyle(Theme.onSurfaceVariant)
                        .padding(.leading, Theme.Space.xs)
                }
                ForEach(senate.committees) { committee in
                    Group {
                        SectionHeader(committee.committee)
                        ForEach(committee.items) { item in
                            EntryCard(item: item)
                        }
                        if !committee.seats.isEmpty {
                            Seats(seats: committee.seats)
                        }
                    }
                    .environment(\.mentionCommittee, committee.committee)
                }
            } else {
                Text(verbatim: L("docket.empty"))
                    .font(Theme.Fonts.text)
                    .foregroundStyle(Theme.onSurfaceVariant)
                    .padding(.top, Theme.Space.l)
                    .padding(.leading, Theme.Space.xs)
            }
        }
    }

    private var senateChoice: Binding<String> {
        Binding(
            get: { store.docket?.senate ?? "" },
            set: { store.docketSenate = $0 }
        )
    }
}

/// One item awaiting the Speaker: what kind, what the Speaker is to do with
/// it, what was said, and why it is here.
private struct EntryCard: View {
    let item: DocketEntry

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.s) {
            HStack(spacing: 6) {
                Chip(text: item.kindLabel)
                Chip(text: L("docket.\(item.awaiting)"), tone: item.awaiting == "away" ? .attention : .accent)
                Spacer(minLength: Theme.Space.s)
                TimedMention(date: item.since) { ago in
                    L("docket.meta", String(item.seq), item.by, ago)
                }
                .font(Theme.Fonts.meta)
                .foregroundStyle(Theme.onSurfaceVariant)
                .lineLimit(1)
            }
            MentionText(item.body)
                .font(Theme.Fonts.text)
                .foregroundStyle(Theme.onSurface)
                .lineLimit(4)
                .textSelection(.enabled)
            MentionText(item.why)
                .font(Theme.Fonts.row)
                .foregroundStyle(Theme.onSurfaceVariant)
        }
        .card()
    }
}

/// The agents seated on a committee, and each one's last checkpoint.
private struct Seats: View {
    let seats: [SeatLine]

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.xs) {
            Text(verbatim: L("docket.seated"))
                .font(Theme.Fonts.meta)
                .foregroundStyle(Theme.onSurfaceVariant)
            ForEach(seats) { seat in
                HStack(spacing: 6) {
                    StatusDot(online: seat.connected, size: 6)
                    MentionText(seat.agent)
                        .font(Theme.Fonts.row)
                    if let checkpoint = seat.checkpoint {
                        Text(verbatim: L("docket.checkpoint", checkpoint))
                            .font(Theme.Fonts.row)
                            .foregroundStyle(Theme.onSurfaceVariant)
                            .lineLimit(1)
                    }
                }
            }
        }
        .padding(.horizontal, Theme.Space.l)
        .padding(.top, Theme.Space.xs)
    }
}
