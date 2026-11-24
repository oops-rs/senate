import SenateCore
import SwiftUI

/// Every thread of a committee (the sidebar's "Show all"), newest first:
/// each opens its thread page.
struct CommitteeThreadsPage: View {
    let store: CoreStore
    let committee: String
    @State private var threads: [CommitteeThread]?

    var body: some View {
        Group {
            if let threads {
                if threads.isEmpty {
                    EmptyState(text: L("threads.empty"))
                } else {
                    ReadingColumn {
                        ForEach(threads, id: \.root) { t in
                            ThreadListRow(store: store, thread: t)
                        }
                    }
                }
            } else {
                ProgressView().controlSize(.small)
            }
        }
        .task(id: committee) { threads = await store.threads.all(committee) }
        .toolbar {
            ToolbarItem(placement: .navigation) {
                VStack(alignment: .leading, spacing: 0) {
                    Text(verbatim: L("threads.title")).font(.headline)
                    Text(verbatim: committee).font(.caption).foregroundStyle(Theme.onSurfaceVariant)
                }
                .padding(.horizontal, Theme.Space.xs)
                .accessibilityElement(children: .combine)
                .accessibilityAddTraits(.isHeader)
            }
            .sharedBackgroundVisibility(.hidden)
        }
    }
}

/// One thread in a committee's list: its mark, title, who to whom, when.
private struct ThreadListRow: View {
    let store: CoreStore
    let thread: CommitteeThread
    @State private var hovered = false

    var body: some View {
        let t = thread
        let unread = store.threads.unread(t)
        Button { store.openThread(committee: t.committee, root: t.root) } label: {
            HStack(spacing: Theme.Space.s) {
                ThreadStateMark(state: t.state, waiting: t.waitsOn != nil || t.escalatedTo != nil)
                    .frame(width: 20)
                VStack(alignment: .leading, spacing: 2) {
                    Text(verbatim: Mentions.shared.plain(t.title, committee: t.committee))
                        .font(Theme.Fonts.text.weight(unread ? .semibold : .regular))
                        .foregroundStyle(Theme.onSurface)
                        .lineLimit(1)
                    Text(verbatim: "#\(t.root) · " + ThreadParties.line(t))
                        .font(Theme.Fonts.meta)
                        .foregroundStyle(Theme.onSurfaceVariant)
                        .lineLimit(1)
                }
                Spacer(minLength: Theme.Space.s)
                if let at = t.lastAt { Ago(date: at).font(Theme.Fonts.meta).foregroundStyle(Theme.onSurfaceVariant) }
                if unread { Circle().fill(Theme.primary).frame(width: 7, height: 7).accessibilityHidden(true) }
            }
            .padding(.vertical, 8)
            .padding(.horizontal, Theme.Space.s)
            .background(hovered ? Theme.containerHigh : .clear, in: RoundedRectangle(cornerRadius: 8))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .onHover { hovered = $0 }
    }
}
