import SenateCore
import SwiftUI

/// History: the machine's ended and stopped sessions, newest first, by
/// day (the list is read a page at a time, newest first, so days only grow
/// at the foot as you scroll; a committee is on each row and in the
/// filter). Its search finds words in past and live conversations.
struct HistoryView: View {
    @Bindable var store: CoreStore
    @FocusState private var searchFocused: Bool

    var body: some View {
        let h = store.history
        VStack(spacing: 0) {
            controls(h)
            Divider().opacity(0.4)
            if !h.query.trimmingCharacters(in: .whitespaces).isEmpty {
                SearchResults(store: store)
            } else {
                list(h)
            }
        }
        .task { if !h.loadedOnce { await h.reload() } }
        .background {
            Button("") { searchFocused = true }
                .keyboardShortcut("f")
                .opacity(0)
                .accessibilityHidden(true)
        }
    }

    private func controls(_ h: HistoryModel) -> some View {
        @Bindable var h = h
        return VStack(alignment: .leading, spacing: Theme.Space.s) {
            Text(verbatim: L("tab.history"))
                .font(Theme.Fonts.title)
                .foregroundStyle(Theme.onSurface)
                .accessibilityAddTraits(.isHeader)
            HStack(spacing: Theme.Space.s) {
                HStack(spacing: 6) {
                    Image(systemName: "magnifyingglass")
                        .foregroundStyle(Theme.onSurfaceVariant)
                        .accessibilityHidden(true)
                    TextField(L("history.search"), text: $h.query)
                        .textFieldStyle(.plain)
                        .focused($searchFocused)
                        .onExitCommand { h.query = "" }
                    if !h.query.isEmpty {
                        Button { h.query = "" } label: {
                            Image(systemName: "xmark.circle.fill").foregroundStyle(Theme.onSurfaceVariant)
                        }
                        .buttonStyle(.plain)
                        .accessibilityLabel(L("sidebar.close_search"))
                    }
                }
                .padding(.horizontal, 10)
                .frame(height: 30)
                .background(Theme.containerHigh, in: Capsule())
                Picker(L("history.filter.committee"), selection: $h.committee) {
                    Text(verbatim: L("history.filter.all_committees")).tag(String?.none)
                    ForEach(committees(h), id: \.self) { c in
                        Text(verbatim: c.isEmpty ? L("sidebar.no_committee") : c).tag(String?.some(c))
                    }
                }
                .labelsHidden()
                .fixedSize()
                Picker(L("history.filter.runtime"), selection: $h.runtime) {
                    Text(verbatim: L("history.filter.all_runtimes")).tag(String?.none)
                    ForEach(["claude", "codex", "agy", "acp"], id: \.self) { r in
                        Text(verbatim: r).tag(String?.some(r))
                    }
                }
                .labelsHidden()
                .fixedSize()
            }
        }
        .frame(maxWidth: Theme.readingWidth, alignment: .leading)
        .padding(.horizontal, Theme.Space.xl)
        .padding(.top, Theme.Space.xl)
        .padding(.bottom, Theme.Space.m)
        .frame(maxWidth: .infinity)
    }

    private func committees(_ h: HistoryModel) -> [String] {
        var names = Set(store.state.groups.compactMap(\.committee))
        for row in h.rows { names.insert(row.committee ?? "") }
        if let c = h.committee { names.insert(c) }
        return names.sorted { a, b in a.isEmpty ? false : b.isEmpty ? true : a < b }
    }

    private func list(_ h: HistoryModel) -> some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 2, pinnedViews: []) {
                let days = Self.days(h.shown)
                ForEach(days, id: \.day) { group in
                    Text(verbatim: Self.dayTitle(group.day))
                        .font(Theme.Fonts.label)
                        .foregroundStyle(Theme.onSurfaceVariant)
                        .padding(.top, Theme.Space.l)
                        .padding(.bottom, Theme.Space.xs)
                        .accessibilityAddTraits(.isHeader)
                    ForEach(group.rows, id: \.sessionId) { row in
                        HistoryRowView(row: row) { store.place = .past(row.sessionId) }
                            .onAppear {
                                if row.sessionId == h.shown.last?.sessionId { Task { await h.loadMore() } }
                            }
                    }
                }
                if h.loading {
                    ProgressView().controlSize(.small).frame(maxWidth: .infinity).padding(Theme.Space.l)
                } else if h.loadedOnce && h.shown.isEmpty {
                    Text(verbatim: L("history.empty"))
                        .font(Theme.Fonts.text)
                        .foregroundStyle(Theme.onSurfaceVariant)
                        .padding(.top, Theme.Space.xl)
                } else if h.next != nil {
                    Button(L("history.load_more")) { Task { await h.loadMore() } }
                        .buttonStyle(.pill)
                        .frame(maxWidth: .infinity)
                        .padding(Theme.Space.l)
                }
                if let error = h.error {
                    Text(verbatim: error).font(Theme.Fonts.meta).foregroundStyle(Theme.danger)
                }
            }
            .frame(maxWidth: Theme.readingWidth, alignment: .leading)
            .padding(.horizontal, Theme.Space.xl)
            .padding(.bottom, Theme.Space.xl)
            .frame(maxWidth: .infinity)
        }
    }

    struct Day { let day: Date; let rows: [HistoryRow] }

    static func days(_ rows: [HistoryRow]) -> [Day] {
        var out: [Day] = []
        let calendar = Calendar.current
        for row in rows {
            let day = calendar.startOfDay(for: row.lastActivity)
            if out.last?.day == day {
                out[out.count - 1] = Day(day: day, rows: out[out.count - 1].rows + [row])
            } else {
                out.append(Day(day: day, rows: [row]))
            }
        }
        return out
    }

    static func dayTitle(_ day: Date) -> String {
        let calendar = Calendar.current
        if calendar.isDateInToday(day) { return L("history.today") }
        if calendar.isDateInYesterday(day) { return L("history.yesterday") }
        return day.formatted(.dateTime.weekday(.wide).month(.wide).day())
    }

    /// How long it ran: from its start to its last activity.
    static func ran(_ row: HistoryRow) -> String {
        let seconds = max(0, row.lastActivity.timeIntervalSince(row.startedAt))
        let f = DateComponentsFormatter()
        f.unitsStyle = .abbreviated
        f.allowedUnits = seconds >= 3600 ? [.hour, .minute] : [.minute]
        f.maximumUnitCount = 2
        return f.string(from: seconds) ?? ""
    }
}

/// One ended session: its runtime, its name and title, its committee, when
/// it was last active and how long it ran; "transcript gone" when it can no
/// longer be read.
private struct HistoryRowView: View {
    let row: HistoryRow
    let open: () -> Void

    var body: some View {
        Button(action: open) {
            HStack(alignment: .firstTextBaseline, spacing: Theme.Space.s) {
                RuntimeMark(runtime: row.runtime)
                    .alignmentGuide(.firstTextBaseline) { $0[.bottom] - 2 }
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 6) {
                        Text(verbatim: row.name).font(Theme.Fonts.label).foregroundStyle(Theme.onSurface)
                        if let c = row.committee {
                            Text(verbatim: c).font(Theme.Fonts.meta).foregroundStyle(Theme.committee)
                        }
                        if !row.readable {
                            Text(verbatim: L("history.unreadable"))
                                .font(Theme.Fonts.meta)
                                .foregroundStyle(Theme.onSurfaceVariant)
                                .italic()
                        }
                    }
                    if let title = row.title {
                        Text(verbatim: title)
                            .font(Theme.Fonts.row)
                            .foregroundStyle(Theme.onSurfaceVariant)
                            .lineLimit(1)
                    }
                }
                Spacer(minLength: Theme.Space.m)
                VStack(alignment: .trailing, spacing: 2) {
                    Text(verbatim: row.lastActivity.formatted(date: .omitted, time: .shortened))
                        .font(Theme.Fonts.meta)
                        .foregroundStyle(Theme.onSurfaceVariant)
                    Text(verbatim: L("history.ran", HistoryView.ran(row)))
                        .font(Theme.Fonts.meta)
                        .foregroundStyle(Theme.onSurfaceVariant)
                }
            }
            .padding(.vertical, 7)
            .padding(.horizontal, Theme.Space.s)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .background(Theme.container.opacity(0.001), in: RoundedRectangle(cornerRadius: 8))
        .accessibilityElement(children: .ignore)
        .accessibilityAddTraits(.isButton)
        .accessibilityLabel(row.name)
        .accessibilityValue([row.title, row.committee, row.runtime,
                             L("history.last_active", row.lastActivity.formatted(.relative(presentation: .named))),
                             row.readable ? nil : L("history.unreadable")].compactMap { $0 }.joined(separator: ", "))
    }
}

/// What a search found: each match's session, the words around it with
/// the match marked, and when.
private struct SearchResults: View {
    let store: CoreStore

    var body: some View {
        let h = store.history
        let q = h.query.trimmingCharacters(in: .whitespaces)
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 2) {
                if let progress = h.indexing {
                    HStack(spacing: Theme.Space.s) {
                        ProgressView().controlSize(.small)
                        Text(verbatim: progress.total.map { L("history.indexing_progress", Int64(progress.indexed ?? 0), Int64($0)) }
                            ?? L("history.indexing"))
                    }
                    .font(Theme.Fonts.meta)
                    .foregroundStyle(Theme.onSurfaceVariant)
                    .padding(.vertical, Theme.Space.s)
                }
                ForEach(Array(h.hits.enumerated()), id: \.offset) { _, hit in
                    Button { store.open(hit, query: q) } label: { hitRow(hit, q) }
                        .buttonStyle(.plain)
                        .accessibilityElement(children: .ignore)
                        .accessibilityAddTraits(.isButton)
                        .accessibilityLabel(name(hit))
                        .accessibilityValue(hit.snippet)
                }
                if h.searching && h.hits.isEmpty {
                    ProgressView().controlSize(.small).frame(maxWidth: .infinity).padding(Theme.Space.l)
                } else if !h.searching && h.hits.isEmpty && h.indexing == nil {
                    Text(verbatim: L("history.no_matches"))
                        .font(Theme.Fonts.text)
                        .foregroundStyle(Theme.onSurfaceVariant)
                        .padding(.top, Theme.Space.xl)
                } else if h.canSearchMore {
                    Button(L("history.load_more")) { Task { await h.searchMore() } }
                        .buttonStyle(.pill)
                        .frame(maxWidth: .infinity)
                        .padding(Theme.Space.l)
                }
            }
            .frame(maxWidth: Theme.readingWidth, alignment: .leading)
            .padding(.horizontal, Theme.Space.xl)
            .padding(.vertical, Theme.Space.m)
            .frame(maxWidth: .infinity)
        }
    }

    private func name(_ hit: SearchHit) -> String {
        store.session(hit.sessionId)?.displayName ?? store.history.row(hit.sessionId)?.name ?? L("history.ended_session")
    }

    private func runtime(_ hit: SearchHit) -> String? {
        store.session(hit.sessionId)?.runtime ?? store.history.row(hit.sessionId)?.runtime
    }

    private func hitRow(_ hit: SearchHit, _ q: String) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: Theme.Space.s) {
            if let r = runtime(hit) {
                RuntimeMark(runtime: r).alignmentGuide(.firstTextBaseline) { $0[.bottom] - 2 }
            }
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    Text(verbatim: name(hit)).font(Theme.Fonts.label).foregroundStyle(Theme.onSurface)
                    Text(verbatim: Self.kind(hit.kind)).font(Theme.Fonts.meta).foregroundStyle(Theme.onSurfaceVariant)
                    if hit.live {
                        Text(verbatim: L("history.live")).font(Theme.Fonts.meta).foregroundStyle(Theme.success)
                    }
                }
                Text(Self.marked(hit.snippet, q))
                    .font(.system(size: 12.5, design: .monospaced))
                    .foregroundStyle(Theme.onSurfaceVariant)
                    .lineLimit(2)
            }
            Spacer(minLength: Theme.Space.m)
            if let at = hit.at {
                Ago(date: at).font(Theme.Fonts.meta).foregroundStyle(Theme.onSurfaceVariant)
            }
        }
        .padding(.vertical, 7)
        .padding(.horizontal, Theme.Space.s)
        .contentShape(Rectangle())
    }

    static func kind(_ k: SearchKind) -> String {
        switch k {
        case .user: L("history.kind.user")
        case .reply: L("history.kind.reply")
        case .tool: L("history.kind.tool")
        }
    }

    /// The snippet with every occurrence of the words marked.
    static func marked(_ snippet: String, _ q: String) -> AttributedString {
        var s = AttributedString(snippet)
        guard !q.isEmpty else { return s }
        var from = snippet.startIndex
        while let r = snippet.range(of: q, options: [.caseInsensitive, .diacriticInsensitive], range: from..<snippet.endIndex) {
            if let a = AttributedString.Index(r.lowerBound, within: s), let b = AttributedString.Index(r.upperBound, within: s) {
                s[a..<b].foregroundColor = Theme.onSurface
                s[a..<b].backgroundColor = Theme.attention.opacity(0.25)
                s[a..<b].font = .system(size: 12.5, weight: .semibold, design: .monospaced)
            }
            from = r.upperBound
        }
        return s
    }
}

/// An ended or stopped session, read-only: what it was, Restore when it
/// can be, Forget in a menu, and its conversation (no composer).
struct PastSessionPage: View {
    @Bindable var store: CoreStore
    let sessionId: String
    @State private var confirmForget = false

    var body: some View {
        let row = store.history.row(sessionId)
        VStack(spacing: 0) {
            header(row)
                .frame(maxWidth: Theme.readingWidth, alignment: .leading)
                .padding(.horizontal, Theme.Space.xl)
                .padding(.bottom, Theme.Space.s)
                .frame(maxWidth: .infinity)
            if let row, !row.readable {
                Text(verbatim: L("past.unreadable"))
                    .font(Theme.Fonts.text)
                    .foregroundStyle(Theme.onSurfaceVariant)
                    .frame(maxWidth: Theme.readingWidth, alignment: .leading)
                    .padding(Theme.Space.xl)
                Spacer()
            } else {
                ConversationPane(store: store, source: .session(sessionId))
            }
        }
        .task(id: sessionId) { if store.history.row(sessionId) == nil { await store.history.find(sessionId) } }
        .confirmationDialog(L("past.forget_title", row?.name ?? sessionId), isPresented: $confirmForget) {
            Button(L("past.forget_confirm"), role: .destructive) {
                Task {
                    if let row, await store.history.forget(row) { store.place = .section(.history) }
                }
            }
        } message: {
            Text(verbatim: L("past.forget_message"))
        }
    }

    @ViewBuilder private func header(_ row: HistoryRow?) -> some View {
        VStack(alignment: .leading, spacing: Theme.Space.s) {
            HStack(alignment: .firstTextBaseline, spacing: Theme.Space.s) {
                Text(verbatim: row?.name ?? sessionId)
                    .font(Theme.Fonts.title)
                    .foregroundStyle(Theme.onSurface)
                    .accessibilityAddTraits(.isHeader)
                Spacer(minLength: Theme.Space.m)
                if let row {
                    HStack(spacing: Theme.Space.s) {
                        if store.isBusy("session:\(row.label)") { ProgressView().controlSize(.small) }
                        if row.restorable {
                            Button(L("sessions.restore")) { Task { await store.history.restore(row) } }
                                .buttonStyle(.pill(.filled))
                                .disabled(store.isBusy("session:\(row.label)") || !store.state.online)
                        }
                        Menu {
                            Button(L("past.forget"), role: .destructive) { confirmForget = true }
                            Divider()
                            Button(L("mention.copy", row.sessionId)) { Mentions.copyText(row.sessionId) }
                        } label: {
                            Label(L("thread.more"), systemImage: "ellipsis")
                        }
                        .menuIndicator(.hidden)
                        .fixedSize()
                    }
                }
            }
            if let row {
                if let title = row.title {
                    Text(verbatim: title).font(Theme.Fonts.text).foregroundStyle(Theme.onSurface).lineLimit(2)
                }
                HStack(spacing: Theme.Space.xs) {
                    Chip(text: L("history.ended"))
                    if let c = row.committee {
                        Button { store.place = .committee(c) } label: { Chip(text: c) }.buttonStyle(.plain)
                    }
                    Chip(text: [row.runtime, row.model].compactMap { $0 }.joined(separator: " · "))
                }
                Text(verbatim: [row.dir, row.branch].compactMap { $0 }.joined(separator: " · "))
                    .font(Theme.Fonts.row)
                    .foregroundStyle(Theme.onSurfaceVariant)
                    .textSelection(.enabled)
                Text(verbatim: L("past.meta",
                                 row.startedAt.formatted(date: .abbreviated, time: .shortened),
                                 row.lastActivity.formatted(.relative(presentation: .named)),
                                 HistoryView.ran(row)))
                    .font(Theme.Fonts.row)
                    .foregroundStyle(Theme.onSurfaceVariant)
            } else {
                ProgressView().controlSize(.small)
            }
        }
        .padding(.top, Theme.Space.xl)
    }
}
