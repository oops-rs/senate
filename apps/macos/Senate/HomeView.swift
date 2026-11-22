import SwiftUI
import SenateCore

/// Home: everything waiting on you, the most pressing first. What holds an
/// agent at a permission, then the threads, each with what was last said.
/// A row opens what it names.
struct HomeView: View {
    let store: CoreStore

    var body: some View {
        let permissions = store.permissionsWaiting
        let threads = store.state.inbox
        let name = store.state.person ?? ""
        if permissions.isEmpty && threads.isEmpty {
            EmptyState(text: L("home.clear", name), detail: L("home.clear_detail"))
        } else {
            ReadingColumn(spacing: Theme.Space.xs) {
                Text(verbatim: L("home.waiting", name, Int64(permissions.count + threads.count)))
                    .font(Theme.Fonts.display)
                    .foregroundStyle(Theme.onSurface)
                    .padding(.top, Theme.Space.xxxl)
                    .padding(.bottom, Theme.Space.l)
                    .padding(.leading, Theme.Space.xs)
                    .accessibilityAddTraits(.isHeader)
                if !permissions.isEmpty {
                    SectionHeader(L("home.permissions"))
                    ForEach(permissions) { item in
                        WaitingRow(store: store, item: item)
                    }
                }
                if !threads.isEmpty {
                    SectionHeader(L("home.threads"))
                    // Lazy: an inbox of thousands draws what is on screen.
                    LazyVStack(alignment: .leading, spacing: Theme.Space.xs) {
                        ForEach(threads) { thread in
                            WaitingRow(store: store, item: Waiting(kind: .thread(thread)))
                        }
                    }
                    // The lazy stack is an element of its own: name it.
                    .accessibilityElement(children: .contain)
                    .accessibilityLabel(L("home.threads"))
                }
            }
        }
    }
}

/// One waiting item: what it is, what it asks or what was last said, where
/// and for how long. The whole row opens it.
private struct WaitingRow: View {
    let store: CoreStore
    let item: Waiting
    @State private var hovered = false

    /// What VoiceOver says after the title.
    private var spoken: String {
        var parts = [Mentions.shared.plain(item.detail, committee: item.committee)]
        if let committee = item.committee { parts.append(committee) }
        parts.append(item.since.formatted(.relative(presentation: .named)))
        if item.unread { parts.append(L("a11y.unread")) }
        return parts.joined(separator: ", ")
    }

    var body: some View {
        Button {
            store.open(item)
        } label: {
            HStack(alignment: .top, spacing: Theme.Space.m) {
                Image(systemName: item.symbol)
                    .font(.system(size: 14))
                    .foregroundStyle(item.isPermission ? Theme.attention : Theme.onSurfaceVariant)
                    .frame(width: 20, height: 20)
                    .accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 2) {
                    HStack(alignment: .firstTextBaseline, spacing: 6) {
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
                    MentionText(item.detail)
                        .font(item.isPermission ? Theme.Fonts.code : Theme.Fonts.row)
                        .foregroundStyle(Theme.onSurfaceVariant)
                        .lineLimit(2)
                        .multilineTextAlignment(.leading)
                }
                Spacer(minLength: Theme.Space.m)
                HStack(spacing: 6) {
                    if let committee = item.committee {
                        Text(verbatim: committee)
                    }
                    Ago(date: item.since)
                }
                .font(Theme.Fonts.meta)
                .foregroundStyle(Theme.onSurfaceVariant)
                .lineLimit(1)
            }
            .padding(.horizontal, Theme.Space.m)
            .padding(.vertical, 10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(
                hovered ? Theme.container : .clear,
                in: RoundedRectangle(cornerRadius: Theme.Radius.card)
            )
            .contentShape(RoundedRectangle(cornerRadius: Theme.Radius.card))
        }
        .buttonStyle(.plain)
        .onHover { hovered = $0 }
        .environment(\.mentionCommittee, item.committee)
        // One element, named as it reads: mentions as names, not as written.
        .accessibilityElement(children: .ignore)
        .accessibilityAddTraits(.isButton)
        .accessibilityLabel(Mentions.shared.plain(item.title, committee: item.committee))
        .accessibilityValue(spoken)
    }
}
