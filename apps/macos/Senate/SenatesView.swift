import SwiftUI
import SenateCore

/// Every Senate the daemon serves, one card each: who you are in it, who
/// holds the Speaker's seat, the committees' chairs, your agents and their
/// streams, the Senates registered below it, and its registration above.
struct SenatesView: View {
    @Bindable var store: CoreStore

    var body: some View {
        if store.state.senates.isEmpty {
            EmptyState(text: L("senates.empty"))
        } else {
            ReadingColumn {
                HStack(alignment: .top) {
                    PageHeading(
                        text: L("tab.senates"),
                        note: store.state.senates.count > 1 ? L("senates.inbox_note") : nil
                    )
                    Spacer(minLength: Theme.Space.m)
                    InactiveToggle(store: store, hidden: hidden)
                        .padding(.top, Theme.Space.xl)
                }
                .padding(.bottom, Theme.Space.s)
                ForEach(store.state.senates) { senate in
                    SenateCard(store: store, senate: senate)
                }
            }
        }
    }
}

extension SenatesView {
    /// The agents and streams "Show inactive" hides now.
    private var hidden: Int {
        store.state.agents.filter { !$0.active }.count
            + store.state.senates.flatMap(\.committees).filter { !$0.live }.count
    }
}

private struct SenateCard: View {
    let store: CoreStore
    let senate: SenateInfo

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.l) {
            HStack(spacing: 6) {
                Text(verbatim: senate.title)
                    .font(Theme.Fonts.heading)
                    .foregroundStyle(Theme.onSurface)
                if senate.primary { Chip(text: L("senates.primary"), tone: .accent) }
            }
            Grid(alignment: .leading, horizontalSpacing: Theme.Space.l, verticalSpacing: 6) {
                Fact(L("senates.address")) { Text(verbatim: senate.serverUrl).textSelection(.enabled) }
                Fact(L("senates.you")) { MentionText(senate.me) }
                Fact(L("senates.speaker")) {
                    if let speaker = senate.speaker {
                        MentionText(speaker)
                    } else {
                        Text(verbatim: L("senates.no_speaker")).foregroundStyle(Theme.onSurfaceVariant)
                    }
                }
                if let uplink = senate.uplink {
                    Fact(L("senates.above")) { Text(verbatim: uplink) }
                }
            }
            .font(Theme.Fonts.text)
            if !senate.chairs.isEmpty {
                Group_(L("senates.chairs")) {
                    ForEach(senate.chairs, id: \.committee) { chair in
                        HStack(spacing: 6) {
                            Chip(text: chair.committee)
                            if let who = chair.chair {
                                MentionText(who).environment(\.mentionCommittee, chair.committee)
                            } else {
                                Text(verbatim: L("senates.no_chair")).foregroundStyle(Theme.onSurfaceVariant)
                            }
                        }
                    }
                }
            }
            let agents = store.state.agents.filter { $0.senate == senate.key && (store.showInactive || $0.active) }
            if !agents.isEmpty {
                Group_(L("senates.agents")) {
                    ForEach(agents) { agent in
                        HStack(spacing: 6) {
                            SenatorName(name: agent.displayName, id: agent.path)
                            if !agent.active { Chip(text: L("roster.inactive")) }
                            if let runtime = agent.runtime { Chip(text: runtime) }
                            if agent.presiding { Chip(text: L("senates.presiding"), tone: .accent) }
                            if agent.sessions > 0 {
                                Chip(text: L("senates.sessions", Int(agent.sessions)))
                            }
                            if agent.pending > 0 {
                                Chip(text: L("senates.waiting", Int(agent.pending)), tone: .attention)
                            }
                        }
                    }
                }
            }
            let streams = senate.committees.filter { store.showInactive || $0.live }
            if !streams.isEmpty {
                Group_(L("senates.streams")) {
                    ForEach(streams) { line in
                        HStack(spacing: 6) {
                            StatusDot(online: line.live, size: 6)
                            MentionText("[senator::\(line.agent)]")
                                .environment(\.mentionCommittee, line.committee)
                            Text(verbatim: "· \(line.committee) · \(line.stream)")
                                .foregroundStyle(Theme.onSurfaceVariant)
                            if let duty = line.duty {
                                Text(verbatim: "· \(duty)")
                                    .foregroundStyle(Theme.onSurfaceVariant)
                                    .lineLimit(1)
                            }
                        }
                        .help(L("senates.stream_help", String(line.delivered), Int(line.sessions)))
                    }
                }
            }
            if !senate.children.isEmpty {
                Group_(L("senates.children")) {
                    ForEach(senate.children, id: \.name) { child in
                        ChildBlock(child: child)
                    }
                }
            }
        }
        .card()
    }
}

/// A Senate registered below: its Speaker, when it was last seen, and the
/// seats it declares.
private struct ChildBlock: View {
    let child: ChildLine

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.xs) {
            HStack(spacing: 6) {
                StatusDot(online: child.online, size: 6)
                Text(verbatim: child.name).fontWeight(.medium)
                Text(verbatim: L("senates.child_speaker")).foregroundStyle(Theme.onSurfaceVariant)
                MentionText(child.speaker)
                if let seen = child.lastSeen {
                    TimedMention(date: seen) { L("senates.seen", $0) }
                        .foregroundStyle(Theme.onSurfaceVariant)
                }
            }
            ForEach(child.seats, id: \.senator) { seat in
                HStack(spacing: 6) {
                    Text(verbatim: seat.name)
                        .fontWeight(.medium)
                        .foregroundStyle(Theme.agent)
                        .help("[senator::\(seat.senator)]")
                    Chip(text: seat.committee)
                    Chip(text: seat.presenceLabel, tone: seat.presence == "online" ? .success : .neutral)
                    if let duty = seat.duty {
                        Text(verbatim: duty).foregroundStyle(Theme.onSurfaceVariant).lineLimit(1)
                    }
                }
                .padding(.leading, 14)
            }
        }
    }
}

/// One fact about a Senate: what it is, then its value.
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
                .foregroundStyle(Theme.onSurfaceVariant)
            value
                .foregroundStyle(Theme.onSurface)
        }
    }
}

/// A named group of lines inside a Senate's card.
private struct Group_<Content: View>: View {
    let title: String
    @ViewBuilder let content: Content

    init(_ title: String, @ViewBuilder content: () -> Content) {
        self.title = title
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(verbatim: title)
                .font(Theme.Fonts.row)
                .foregroundStyle(Theme.onSurfaceVariant)
                .accessibilityAddTraits(.isHeader)
            content
        }
        .font(Theme.Fonts.text)
        .foregroundStyle(Theme.onSurface)
    }
}
