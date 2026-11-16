import SwiftUI

/// What awaits the Speaker of a Senate one of your agents presides over.
///
/// Read-only on purpose: routing an item to an agent, covering for an away
/// agent from its checkpoint, and putting an item to a person are the
/// presiding agent's own tools, run by that agent in its session. This tab
/// shows what is waiting for it, so a person can see a closed or stuck
/// presiding session; it has no buttons that change anything.
struct DocketView: View {
    @Bindable var store: CoreStore

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                if store.state.docket.count > 1 {
                    Picker(L("docket.senate"), selection: senateChoice) {
                        ForEach(store.state.docket) { senate in
                            Text(verbatim: senate.title).tag(senate.senate)
                        }
                    }
                    .pickerStyle(.menu)
                    .fixedSize()
                }
                Text(verbatim: L("docket.note"))
                    .font(.callout).foregroundStyle(.secondary)
                Spacer()
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 6)
            if let senate = store.docket, !senate.committees.isEmpty {
                List {
                    if let speaker = senate.speaker {
                        MentionText(L("docket.speaker", speaker))
                            .font(.callout).foregroundStyle(.secondary)
                    }
                    ForEach(senate.committees) { committee in
                        Section(committee.committee) {
                            ForEach(committee.items) { item in
                                EntryRow(item: item)
                                    .padding(.vertical, 4)
                            }
                            if !committee.seats.isEmpty {
                                SeatsRow(seats: committee.seats)
                            }
                        }
                    }
                }
            } else {
                EmptyNote(online: store.state.online, text: L("docket.empty"))
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

private struct EntryRow: View {
    let item: DocketEntry

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 6) {
                Tag(text: item.kindLabel)
                Tag(text: L("docket.\(item.awaiting)"), tint: item.awaiting == "away" ? .orange : .blue)
                TimedMention(date: item.since) { ago in
                    L("docket.meta", String(item.seq), item.by, ago)
                }
                .font(.callout).foregroundStyle(.secondary)
            }
            MentionText(item.body).lineLimit(4).textSelection(.enabled)
            MentionText(item.why).font(.callout).foregroundStyle(.secondary)
        }
    }
}

private struct SeatsRow: View {
    let seats: [SeatLine]

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(verbatim: L("docket.seated")).font(.caption).foregroundStyle(.secondary)
            ForEach(seats) { seat in
                HStack(spacing: 6) {
                    Circle()
                        .fill(seat.connected ? Color.green : Color.secondary)
                        .frame(width: 6, height: 6)
                    MentionText(seat.agent).font(.callout)
                    if let checkpoint = seat.checkpoint {
                        Text(verbatim: L("docket.checkpoint", checkpoint))
                            .font(.callout).foregroundStyle(.secondary).lineLimit(1)
                    }
                }
            }
        }
    }
}
