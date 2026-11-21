import SwiftUI
import SenateCore

/// Every Senate the daemon serves: who you are in it, who holds the
/// Speaker's seat, the committees' chairs, your agents and their streams,
/// the Senates registered below it, and its registration above.
struct SenatesView: View {
    let store: CoreStore

    var body: some View {
        if store.state.senates.isEmpty {
            EmptyNote(online: store.state.online, text: L("senates.empty"), symbol: "building.columns")
        } else {
            List {
                ForEach(store.state.senates) { senate in
                    Section {
                        SenateBlock(store: store, senate: senate)
                    } header: {
                        HStack {
                            Text(verbatim: senate.title).font(.headline)
                            if senate.primary { Tag(text: L("senates.primary"), tint: .blue) }
                        }
                    }
                }
                if store.state.senates.count > 1 {
                    Text(verbatim: L("senates.inbox_note"))
                        .font(.callout).foregroundStyle(.secondary)
                }
            }
            .listStyle(.inset)
        }
    }
}

private struct SenateBlock: View {
    let store: CoreStore
    let senate: SenateInfo

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 4) {
                Fact(L("senates.address")) { Text(verbatim: senate.serverUrl).textSelection(.enabled) }
                Fact(L("senates.you")) { MentionText(senate.me) }
                Fact(L("senates.speaker")) {
                    if let speaker = senate.speaker {
                        MentionText(speaker)
                    } else {
                        Text(verbatim: L("senates.no_speaker")).foregroundStyle(.secondary)
                    }
                }
                if let uplink = senate.uplink {
                    Fact(L("senates.above")) { Text(verbatim: uplink) }
                }
            }
            if !senate.chairs.isEmpty {
                Heading(L("senates.chairs"))
                ForEach(senate.chairs, id: \.committee) { chair in
                    HStack(spacing: 6) {
                        Tag(text: chair.committee)
                        if let who = chair.chair {
                            MentionText(who).environment(\.mentionCommittee, chair.committee)
                        } else {
                            Text(verbatim: L("senates.no_chair")).foregroundStyle(.secondary)
                        }
                    }
                }
            }
            let agents = store.state.agents.filter { $0.senate == senate.key }
            if !agents.isEmpty {
                Heading(L("senates.agents"))
                ForEach(agents) { agent in
                    HStack(spacing: 6) {
                        MentionText(agent.mention)
                        if let runtime = agent.runtime { Tag(text: runtime) }
                        if agent.presiding { Tag(text: L("senates.presiding"), tint: .orange) }
                        if agent.sessions > 0 {
                            Tag(text: L("senates.sessions", Int(agent.sessions)))
                        }
                        if agent.pending > 0 {
                            Tag(text: L("senates.waiting", Int(agent.pending)), tint: .red)
                        }
                    }
                }
            }
            if !senate.committees.isEmpty {
                Heading(L("senates.streams"))
                ForEach(senate.committees) { line in
                    HStack(spacing: 6) {
                        StatusDot(online: line.live, size: 6)
                        MentionText("[senator::\(line.agent)]")
                            .environment(\.mentionCommittee, line.committee)
                        Text(verbatim: "· \(line.committee) · \(line.stream)")
                            .foregroundStyle(.secondary)
                    }
                    .font(.callout)
                    .help(L("senates.stream_help", String(line.delivered), Int(line.sessions)))
                }
            }
            if !senate.children.isEmpty {
                Heading(L("senates.children"))
                ForEach(senate.children, id: \.name) { child in
                    ChildBlock(child: child)
                }
            }
        }
        .padding(.vertical, 6)
    }
}

private struct ChildBlock: View {
    let child: ChildLine

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 6) {
                StatusDot(online: child.online, size: 6)
                Text(verbatim: child.name).font(.body.weight(.medium))
                Text(verbatim: L("senates.child_speaker")).foregroundStyle(.secondary)
                MentionText(child.speaker)
                if let seen = child.lastSeen {
                    TimedMention(date: seen) { L("senates.seen", $0) }
                        .foregroundStyle(.secondary)
                }
            }
            ForEach(child.seats, id: \.senator) { seat in
                HStack(spacing: 6) {
                    Text(verbatim: seat.name)
                        .fontWeight(.medium)
                        .foregroundStyle(.indigo)
                        .help("[senator::\(seat.senator)]")
                    Tag(text: seat.committee)
                    Tag(text: seat.presence, tint: seat.presence == "online" ? .green : .secondary)
                    if let duty = seat.duty {
                        Text(verbatim: duty).foregroundStyle(.secondary).lineLimit(1)
                    }
                }
                .font(.callout)
                .padding(.leading, 14)
            }
        }
    }
}

private struct Fact<Value: View>: View {
    let label: String
    @ViewBuilder let value: Value

    init(_ label: String, @ViewBuilder value: () -> Value) {
        self.label = label
        self.value = value()
    }

    var body: some View {
        GridRow {
            Text(verbatim: label)
                .foregroundStyle(.secondary)
                .gridColumnAlignment(.trailing)
            value
        }
    }
}

private struct Heading: View {
    let text: String

    init(_ text: String) {
        self.text = text
    }

    var body: some View {
        Text(verbatim: text)
            .font(.caption.weight(.semibold))
            .foregroundStyle(.secondary)
            .textCase(.uppercase)
            .padding(.top, 4)
            .accessibilityAddTraits(.isHeader)
    }
}
