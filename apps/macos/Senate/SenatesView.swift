import SwiftUI

/// Every Senate the daemon serves: who you are in it, who holds the
/// Speaker's seat, your agents there and their streams.
struct SenatesView: View {
    let store: CoreStore

    var body: some View {
        if store.state.senates.isEmpty {
            EmptyNote(online: store.state.online, text: L("senates.empty"))
        } else {
            List {
                ForEach(store.state.senates) { senate in
                    Section {
                        SenateBlock(store: store, senate: senate)
                    } header: {
                        Text(verbatim: senate.title).font(.headline)
                    }
                }
                if store.state.senates.count > 1 {
                    Text(verbatim: L("senates.inbox_note"))
                        .font(.callout).foregroundStyle(.secondary)
                }
            }
        }
    }
}

private struct SenateBlock: View {
    let store: CoreStore
    let senate: SenateInfo

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(verbatim: senate.serverUrl).font(.callout).foregroundStyle(.secondary)
            MentionText(L("senates.you", senate.me))
            if let speaker = senate.speaker {
                MentionText(L("senates.speaker", speaker))
            } else {
                Text(verbatim: L("senates.no_speaker")).foregroundStyle(.secondary)
            }
            let agents = store.state.agents.filter { $0.senate == senate.key }
            if !agents.isEmpty {
                Text(verbatim: L("senates.agents")).font(.caption).foregroundStyle(.secondary)
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
            ForEach(senate.committees) { line in
                HStack(spacing: 6) {
                    Tag(text: "\(line.agent) · \(line.committee) · \(line.stream) · seq \(line.delivered)",
                        tint: line.live ? .green : .orange)
                        .help(L("senates.stream_help", String(line.delivered), Int(line.sessions)))
                }
            }
        }
        .padding(.vertical, 4)
    }
}
