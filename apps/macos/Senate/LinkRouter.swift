import AppKit
import Foundation
import Observation
import SwiftUI
import SenateCore

/// `senate://open/v1/…` links: the app's one validator and router. Every
/// link, from the system (a click in a browser, `senate open`) or from a
/// page inside the app (Town), comes through `open(url:)`.
///
/// The grammar is the Rust core's (`parseAppLink`, the protocol crate's
/// parser); nothing here reads a URL itself. A link only navigates: it
/// resolves against what the app already knows (or reads), then shows a
/// page. No route sends, approves, attaches, stops, closes or edits: the
/// only side effects are navigation, the confirmation before showing
/// another Senate, and bringing the window forward (bounded).
@MainActor
@Observable
final class LinkRouter {
    /// Where a link came from. A gesture is a click the app saw (Town); a
    /// system delivery carries no proof of one.
    enum Provenance { case gesture, system }

    /// What a link resolved to.
    enum Outcome: Equatable {
        /// Not a v1 `open` link: ignored.
        case rejected
        case shown(CoreStore.Place)
        /// The target is missing, deleted, unreadable, or not on this
        /// machine: a quiet page saying so.
        case unavailable
        /// Another Senate served here: the person confirms first.
        case confirm(SenateSwitch)
    }

    /// The link's Senate, named, waiting for the person to say yes.
    struct SenateSwitch: Hashable {
        /// `SenateInfo.key`.
        let key: String
        /// Its name as the app shows it, plain text.
        let title: String
    }

    @ObservationIgnored weak var store: CoreStore?
    /// Bursts: the last link wins, shown this long after the last one
    /// arrived, and never later than `maxWait` after the first.
    @ObservationIgnored var trailing: Duration = .milliseconds(300)
    @ObservationIgnored var maxWait: Duration = .seconds(1)
    /// At most one activation this often for links the system delivers.
    @ObservationIgnored var systemActivationGap: Duration = .seconds(2)
    @ObservationIgnored var clock: () -> ContinuousClock.Instant = { ContinuousClock.now }
    /// Whether the window is on screen now (the tests set their own).
    @ObservationIgnored var windowVisible: @MainActor () -> Bool = { LinkRouter.mainWindowVisible() }
    /// Bring the window forward (the tests count instead).
    @ObservationIgnored var activate: @MainActor () -> Void = {}

    @ObservationIgnored private var pending: (link: AppLinkRoute, gesture: Bool)?
    @ObservationIgnored private var burstStart: ContinuousClock.Instant?
    @ObservationIgnored private var timer: Task<Void, Never>?
    @ObservationIgnored private var lastSystemActivation: ContinuousClock.Instant?
    /// This machine's Observe installation id, once read (`nil` inside:
    /// the daemon hosts no Observe).
    @ObservationIgnored private var installation: String??
    /// Links routed: what each came to (the tests read it).
    @ObservationIgnored private(set) var routed: [Outcome] = []
    /// How many times a link brought the window forward.
    @ObservationIgnored private(set) var activations = 0

    init(store: CoreStore? = nil) {
        self.store = store
        activate = { [weak self] in self?.store?.showWindow() }
    }

    // MARK: Entry

    /// Open a link: the one entry for every caller. A page's navigation
    /// delegate (Town) calls this for a user-activated main-frame
    /// `senate:` navigation it cancelled; the app delegate calls it for
    /// what the system delivers. Returns false when the link is not a v1
    /// `open` link (nothing happens then).
    @discardableResult
    func open(url: URL, provenance: Provenance = .gesture) -> Bool {
        open(url.absoluteString, provenance: provenance)
    }

    @discardableResult
    func open(_ url: String, provenance: Provenance) -> Bool {
        guard let link = parseAppLink(url: url) else {
            Log.app.notice("ignored a link that is not a Senate open link")
            routed.append(.rejected)
            return false
        }
        let now = clock()
        // Each link carries its own gesture evidence: the burst's last link
        // is routed, and it inherits none from the links it replaced.
        let gesture = provenance == .gesture
        pending = (link, gesture)
        let start = burstStart ?? now
        burstStart = start
        let delay = min(trailing, start + maxWait - now)
        timer?.cancel()
        timer = Task { [weak self] in
            if delay > .zero { try? await Task.sleep(for: delay) }
            guard !Task.isCancelled else { return }
            await self?.flush()
        }
        return true
    }

    /// Route the burst's last link now.
    func flush() async {
        guard let (link, gesture) = pending else { return }
        pending = nil
        burstStart = nil
        timer = nil
        let outcome = await resolve(link)
        Log.app.notice("link routed: \(String(describing: outcome), privacy: .private)")
        routed.append(outcome)
        // A thread a link opens without the person's click is shown unread:
        // only their own action moves its read marker.
        if case let .shown(.thread(key)) = outcome { store?.unreadShown = gesture ? nil : key }
        show(outcome, link: link)
        bringForward(gesture: gesture)
    }

    /// A link as the system delivered it. A Debug build registers
    /// `senate-debug` rather than `senate` (project.yml), so it never takes
    /// links from the installed app; it reads its own scheme as `senate`.
    nonisolated static func delivered(_ url: URL) -> String {
        let text = url.absoluteString
        #if DEBUG
        let debug = "senate-debug://"
        if text.hasPrefix(debug) { return "senate://" + text.dropFirst(debug.count) }
        #endif
        return text
    }

    // MARK: Resolving

    /// Where a parsed link leads, against what the app knows. Reads only.
    func resolve(_ link: AppLinkRoute) async -> Outcome {
        guard let store else { return .unavailable }
        switch await senate(link.senate, store: store) {
        case .current: break
        case let .other(other): return .confirm(other)
        case .unknown: return .unavailable
        }
        return await destination(link.route, store: store)
    }

    private enum SenateMatch { case current, other(SenateSwitch), unknown }

    /// The link's Senate among those this app knows: none named is the
    /// selected one; `id:` must be this machine's installation; a path must
    /// be a Senate the daemon serves. An unknown one asks nothing of anyone.
    private func senate(_ selector: String?, store: CoreStore) async -> SenateMatch {
        guard let selector else { return .current }
        if let id = selector.stripPrefix("id:") {
            if installation == nil {
                let read = try? await store.core?.observeInstallationId()
                installation = .some(read)
            }
            return installation == .some(id) ? .current : .unknown
        }
        let path = selector.trimmingCharacters(in: CharacterSet(charactersIn: "/"))
        let senates = store.state.senates
        guard let found = senates.first(where: { ($0.senate ?? $0.key) == path || $0.key == path }) else {
            return .unknown
        }
        if found.key == senates.first?.key { return .current }
        return .other(SenateSwitch(key: found.key, title: found.title))
    }

    private func destination(_ route: AppRoute, store: CoreStore) async -> Outcome {
        switch route {
        case .inbox: return .shown(.section(.inbox))
        case .approvals: return .shown(.section(.approvals))
        // The desk's list is the Approvals page's "For the Speaker".
        case .desk: return .shown(.section(.approvals))
        case .history: return .shown(.section(.history))
        case .profile: return .shown(.section(.senates))
        case let .conference(id):
            if store.state.conferences.contains(where: { $0.id == id }) {
                return .shown(.conference(id))
            }
            guard let core = store.core, (try? await core.conferenceDetail(id: id)) != nil else {
                return .unavailable
            }
            return .shown(.conference(id))
        case let .committee(name):
            return await knownCommittee(name, store: store) ? .shown(.committee(name)) : .unavailable
        case let .thread(committee, root):
            guard await knownCommittee(committee, store: store) else { return .unavailable }
            if store.state.inbox.contains(where: { $0.committee == committee && $0.root == root }) {
                return .shown(.thread("\(committee):\(root)"))
            }
            // Not waiting on you: there only if it can be read. Probed without
            // reading it (the read marker stays put); the person reads it when
            // the page is shown.
            guard let core = store.core,
                  (try? await core.threadPeek(committee: committee, thread: root)) != nil
            else { return .unavailable }
            return .shown(.thread("\(committee):\(root)"))
        case let .session(selector):
            if let (committee, id) = observed(selector) {
                guard let name = await seat(id, committee: committee, store: store),
                      let line = live(name, committee: committee, store: store)
                else { return .unavailable }
                return .shown(.session(line))
            }
            return session(selector, store: store).map { .shown(.session($0)) } ?? .unavailable
        case let .senator(name):
            if let (committee, id) = observed(name) {
                guard let seat = await seat(id, committee: committee, store: store) else { return .unavailable }
                if let line = live(seat, committee: committee, store: store) { return .shown(.session(line)) }
                return await knownCommittee(committee, store: store) ? .shown(.committee(committee)) : .unavailable
            }
            return senator(name, store: store)
        case let .deskItem(committee, seq):
            guard let entry = store.docketEntries.first(where: { $0.committee == committee && $0.seq == seq })
            else { return .unavailable }
            return .shown(.desk(entry))
        case let .historyEntry(id):
            var native = id
            if id.hasPrefix("obs-") {
                guard let core = store.core,
                      let found = try? await core.observeHistorySession(id: id)
                else { return .unavailable }
                native = found
            }
            if store.session(native) != nil { return .shown(.past(native)) }
            // A link looks a little way back, not through the whole History.
            if store.history.row(native) == nil { await store.history.find(native, pages: 3) }
            return store.history.row(native) != nil ? .shown(.past(native)) : .unavailable
        }
    }

    /// An Observe seat selector, `<committee>::obs-…`, split. One without
    /// a committee cannot be resolved (the lookup is per committee), and
    /// is reported as `("", id)`, which no committee matches.
    private func observed(_ selector: String) -> (String, String)? {
        if selector.hasPrefix("obs-") { return ("", selector) }
        guard let range = selector.range(of: "::"),
              selector[range.upperBound...].hasPrefix("obs-") else { return nil }
        return (String(selector[..<range.lowerBound]), String(selector[range.upperBound...]))
    }

    /// The person's own senator behind an Observe seat selector, by the
    /// daemon's person-guarded lookup; never a guess from the id.
    private func seat(_ id: String, committee: String, store: CoreStore) async -> String? {
        guard !committee.isEmpty, let core = store.core else { return nil }
        return (try? await core.observeSeatIdentity(id: id, committee: committee)) ?? nil
    }

    /// The session speaking as `senator`, on `committee` first.
    private func live(_ senator: String, committee: String, store: CoreStore) -> String? {
        let lines = ordered(store).filter { Mentions.id($0.mention) == senator }
        return (lines.first { $0.committee == committee } ?? lines.first)?.sessionId
    }

    /// A committee the person can read here: in the roster, or archived.
    private func knownCommittee(_ name: String, store: CoreStore) async -> Bool {
        if store.roster.contains(where: { $0.committee == name }) { return true }
        if store.state.groups.contains(where: { $0.committee == name }) { return true }
        guard let core = store.core, let archived = try? await core.archivedCommittees() else { return false }
        return archived.contains(name)
    }

    /// The sessions on this machine, in the sidebar's order, the active
    /// ones first: the deterministic choice when a name has several.
    private func ordered(_ store: CoreStore) -> [SessionLine] {
        let all = store.state.groups.flatMap(\.sessions)
        return all.filter(\.active) + all.filter { !$0.active }
    }

    /// A session by its id or `committee::alias`.
    private func session(_ selector: String, store: CoreStore) -> String? {
        if store.session(selector) != nil { return selector }
        return ordered(store).first { line in
            line.hosted?.label == selector || line.committee.map { "\($0)::\(line.displayName)" } == selector
        }?.sessionId
    }

    /// A senator: its live session here, else its seat's committee.
    private func senator(_ name: String, store: CoreStore) -> Outcome {
        if name.contains("::") {
            if let id = session(name, store: store) { return .shown(.session(id)) }
            let parts = name.components(separatedBy: "::")
            let (committee, alias) = (parts[0], parts[parts.count - 1])
            let seat = store.roster.first { $0.committee == committee }?
                .members.first { Mentions.id($0.senatorId) == alias || $0.displayName == alias }
            if seat != nil { return .shown(.committee(committee)) }
            return .unavailable
        }
        if let line = ordered(store).first(where: { Mentions.id($0.mention) == name }) {
            return .shown(.session(line.sessionId))
        }
        if let seat = store.roster.first(where: { $0.members.contains { Mentions.id($0.senatorId) == name } }) {
            return .shown(.committee(seat.committee))
        }
        return .unavailable
    }

    // MARK: Showing

    private func show(_ outcome: Outcome, link: AppLinkRoute) {
        guard let store else { return }
        switch outcome {
        case .rejected: break
        case let .shown(place): store.place = place
        case .unavailable: store.place = .unavailable
        case let .confirm(other): store.place = .confirmSenate(other, link.canonical)
        }
    }

    /// The person said yes to another Senate: show it. This app reads the
    /// primary Senate's committees and threads; another Senate served here
    /// is shown on the Senates page, where it is listed.
    func confirmed(_ other: LinkRouter.SenateSwitch) {
        store?.place = .section(.senates)
    }

    /// Bring the window forward: once for a gesture; for what the system
    /// delivers, not when the window is already on screen and at most once
    /// per `systemActivationGap`.
    private func bringForward(gesture: Bool) {
        if !gesture {
            if windowVisible() { return }
            let now = clock()
            if let last = lastSystemActivation, now - last < systemActivationGap { return }
            lastSystemActivation = now
        }
        activations += 1
        activate()
    }

    /// The main window is open and on screen.
    static func mainWindowVisible() -> Bool {
        NSApplication.shared.windows.contains {
            $0.identifier?.rawValue.hasPrefix(CoreStore.windowID) == true && $0.isVisible && !$0.isMiniaturized
        }
    }
}

private extension String {
    func stripPrefix(_ prefix: String) -> String? {
        hasPrefix(prefix) ? String(dropFirst(prefix.count)) : nil
    }
}

/// A link for another Senate served here: its name, and the person's yes
/// or no. Nothing is shown of the link's Senate until they say yes. The
/// name and the link are plain text.
struct SenateSwitchPage: View {
    let store: CoreStore
    let other: LinkRouter.SenateSwitch
    let link: String

    var body: some View {
        CenteredPage {
            VStack(spacing: Theme.Space.m) {
                Text(verbatim: L("link.other_senate", other.title))
                    .font(Theme.Fonts.display)
                    .foregroundStyle(Theme.onSurface)
                Text(verbatim: L("link.other_senate_detail"))
                    .font(Theme.Fonts.text)
                    .foregroundStyle(Theme.onSurfaceVariant)
                Text(verbatim: link)
                    .font(Theme.Fonts.text.monospaced())
                    .foregroundStyle(Theme.onSurfaceVariant)
                    .textSelection(.enabled)
                    .lineLimit(2)
                    .truncationMode(.middle)
                HStack(spacing: Theme.Space.s) {
                    Button(L("link.cancel")) {
                        if !store.back() { store.select(.section(.inbox)) }
                    }
                    .buttonStyle(.pill)
                    .keyboardShortcut(.cancelAction)
                    Button(L("link.show_senate", other.title)) { store.links.confirmed(other) }
                        .buttonStyle(.pill(.filled))
                        .keyboardShortcut(.defaultAction)
                }
            }
            .multilineTextAlignment(.center)
        }
    }
}
