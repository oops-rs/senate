import SenateCore
import SwiftUI

/// The pinned Secretary row's page: the secretary's own session page once
/// it runs here; before that, what it is, where it stands, and a composer
/// whose first message starts it (the daemon's ensure, as the person).
struct SecretaryPage: View {
    @Bindable var store: CoreStore

    var body: some View {
        Group {
            if store.secretary?.state == .running, let line = store.secretarySession, !store.secretaryStarting {
                SessionPage(store: store, session: line)
            } else {
                SecretaryIntro(store: store)
            }
        }
        .task { await store.refreshSecretary() }
    }
}

/// The secretary before it runs here.
private struct SecretaryIntro: View {
    @Bindable var store: CoreStore

    var body: some View {
        let state = store.secretary?.state
        VStack(spacing: 0) {
            ScrollView {
                VStack(alignment: .leading, spacing: Theme.Space.m) {
                    if store.secretaryStarting {
                        HStack(spacing: Theme.Space.s) {
                            ProgressView().controlSize(.small)
                            Text(verbatim: L("secretary.starting"))
                        }
                        .font(Theme.Fonts.text)
                        .foregroundStyle(Theme.onSurfaceVariant)
                    } else if let state {
                        Text(verbatim: Self.explanation(state))
                            .font(Theme.Fonts.reading)
                            .foregroundStyle(Theme.onSurfaceVariant)
                            .fixedSize(horizontal: false, vertical: true)
                    } else {
                        ProgressView().controlSize(.small)
                    }
                    if let error = store.secretaryError {
                        Text(verbatim: error)
                            .font(Theme.Fonts.text)
                            .foregroundStyle(Theme.danger)
                            .textSelection(.enabled)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
                .frame(maxWidth: Theme.readingWidth, alignment: .leading)
                .padding(.horizontal, Theme.Space.xl)
                .padding(.top, Theme.Space.xl)
                .frame(maxWidth: .infinity)
            }
            // Only the person starts it, by asking it something; a stopped
            // one is restored first, and a seat that is someone else's is
            // not theirs to start.
            if state != .stopped && state != .elsewhere {
                MessageComposer(
                    text: $store.secretaryDraft,
                    label: L("secretary.ask"),
                    placeholder: L("secretary.placeholder"),
                    footnote: L("secretary.footnote", store.state.person ?? "?"),
                    busy: store.secretaryStarting,
                    online: store.state.online
                ) {
                    Task { await store.askSecretary() }
                }
            }
        }
        .toolbar { toolbarTitle }
    }

    @ToolbarContentBuilder var toolbarTitle: some ToolbarContent {
        ToolbarItem(placement: .navigation) {
            Label(L("secretary.title"), systemImage: "person.crop.circle")
                .labelStyle(.titleAndIcon)
                .font(.headline)
                .padding(.horizontal, Theme.Space.xs)
                .accessibilityAddTraits(.isHeader)
        }
        .sharedBackgroundVisibility(.hidden)
        if store.secretary?.state == .stopped {
            ToolbarItem(placement: .primaryAction) {
                Button { Task { await store.restoreSecretary() } } label: {
                    Label(L("secretary.restore"), systemImage: "arrow.counterclockwise")
                }
                // The page's one decisive action: tinted.
                .buttonStyle(.borderedProminent)
                .help(L("secretary.restore"))
                .disabled(!store.state.online || store.isBusy("session:office::secretary"))
            }
        }
    }

    static func explanation(_ state: SecretaryState) -> String {
        switch state {
        case .notRecorded: L("secretary.not_recorded")
        case .running: L("secretary.not_seated")
        case .paused: L("secretary.paused")
        case .ended: L("secretary.ended")
        case .stopped: L("secretary.stopped")
        case .elsewhere: L("secretary.elsewhere")
        }
    }
}

extension SecretaryState {
    /// In a few words, for the sidebar row's accessibility value.
    var label: String {
        switch self {
        case .notRecorded: L("secretary.state.not_recorded")
        case .running: L("status.idle")
        case .paused: L("status.paused")
        case .ended, .stopped: L("status.stopped")
        case .elsewhere: L("secretary.state.elsewhere")
        }
    }
}
