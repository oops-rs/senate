//! Caller checks for person-only IPC and app-confirmed operations.
//!
//! On macOS, operator IPC requires a per-request Mach proof of process
//! generation, an eligible signed origin, and an unblocked coalition.
//! Linux/WSL with provisioned cgroups check membership using a stable socket
//! peer handle. Inherited membership survives reparenting.
//!
//! A kernel ancestry walk additionally rejects recorded runtimes, hosts,
//! the daemon and named coding runtimes. Native detached agents, shared
//! Codex backends outside protected scopes, and eligible terminal deputies
//! can pass routine checks. Linux and WSL advertise an
//! advisory guard when protected scopes are unavailable.
//!
//! Sensitive mutations require a decision from the pinned Mac app or an
//! OS user-authentication result in a trusted component. A decision applies
//! once to the exact request. Sensitive operations are refused in unsupported
//! environments. Writable terminal viewers can reuse device-owner approval
//! only on the same controlling terminal and live session leader. Session
//! selectors are checked against the calling runtime instance. See
//! `docs/person-guard.md` for platform and hook policy.

use std::collections::BTreeMap;

use super::*;
use crate::host;

/// The most ancestors walked; a deeper chain is refused.
const DEPTH: usize = 64;
pub(super) const APP_CONFIRMATION: &str = "This operation requires confirmation in the signed Senate app or local user authentication on the hosting Mac. Use the signed app there";
/// A person-only request arrived without a caller proof. A current CLI
/// does not send one so; an older or unsigned client does.
#[cfg(target_os = "macos")]
const NO_PROOF: &str = "the daemon could not tell which process sent this request: it carried no caller proof (an unsigned or older senate CLI?). Run this senate's own CLI from your terminal, or use the Senate app";
#[cfg(not(target_os = "macos"))]
const NO_PROOF: &str = "the daemon could not tell which process sent this request";
/// Executable names of the coding runtimes.
const RUNTIMES: [&str; 3] = ["claude", "codex", "agy"];

tokio::task_local! {
    pub(super) static VERIFIED_ORIGIN: &'static str;
}

pub(super) fn annotate_origin(event: NewEvent) -> NewEvent {
    match VERIFIED_ORIGIN.try_with(|origin| *origin) {
        Ok(origin) if origin != "unverified" => {
            event.with_meta("verified_origin", serde_json::json!(origin))
        }
        _ => event,
    }
}

/// Check the actual author selected by a socket request before appending.
/// Internal daemon work runs outside the socket's caller scope.
pub(super) fn check_person_author(is_person: bool) -> Result<(), IpcError> {
    if is_person
        && VERIFIED_ORIGIN
            .try_with(|origin| !matches!(*origin, "app" | "person-terminal"))
            .unwrap_or(false)
    {
        return Err(refusal(
            "Write as the person from your own terminal or the Senate app",
        ));
    }
    Ok(())
}

pub(super) struct PresencePeer {
    pub operator: Option<u32>,
    pub session: Option<u32>,
    pub signed_app: bool,
    pub remote_operator: bool,
}

pub(super) fn advisory_notice(locale: Locale) -> Option<&'static str> {
    #[cfg(target_os = "linux")]
    {
        (!crate::linux_scope::available()).then(|| locale.pick(
            "此机器的人员专用守卫仅为提示。托管代理脱离进程树后可能通过检查。运行 senate setup guard 进行设置。",
            "The person-only guard is advisory on this machine. A hosted agent that detaches can pass. Run senate setup guard to enforce it.",
        ))
    }
    #[cfg(target_os = "macos")]
    {
        (!signed_local_daemon()).then(|| {
            locale.pick(
                "此未签名守护进程的终端来访身份仅为提示。",
                "Hosted terminal authority is advisory on this unsigned macOS daemon.",
            )
        })
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = locale;
        None
    }
}

#[cfg(target_os = "macos")]
fn signed_local_daemon() -> bool {
    static SIGNED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SIGNED.get_or_init(|| crate::app_confirmation::macos::signed_daemon(std::process::id()))
}

/// The requests that answer a permission prompt.
pub(super) fn answers_a_prompt(request: &Request) -> bool {
    matches!(
        request,
        Request::Approve { .. } | Request::ApproveSession { .. }
    )
}

/// Effects whose authority cannot come from a session selector.
/// Exhaustive over Request so extending the IPC requires an effect decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestEffect {
    SessionOrRead,
    PersonWrite,
    Operator,
    Presence,
}

impl RequestEffect {
    pub(crate) fn requires_operator(self) -> bool {
        self != Self::SessionOrRead
    }
}

pub(crate) fn request_effect(request: &Request) -> RequestEffect {
    use RequestEffect::{Operator, PersonWrite, Presence, SessionOrRead};

    match request {
        Request::Take { .. } | Request::MemoryEdit { .. } | Request::Say { .. } => PersonWrite,

        Request::OpenCommittee { .. } | Request::Seen { .. } | Request::Watch { .. } => Operator,

        // A role holder's act within its role: a chair spawning, restoring
        // or stopping its committee's members; the Speaker's office (its
        // agent or a secretary) working the desk. The session's verified
        // process carries the authority, and each handler checks the role
        // and its scope. Without a session the same request is the person's.
        Request::Spawn {
            session,
            secretary: false,
            ..
        }
        | Request::Restore { session, .. }
        | Request::Stop { session, .. }
        | Request::DeskOpenCommittee { session, .. }
        | Request::SetChair { session, .. }
        | Request::SpeakerAlias { session, .. }
        | Request::SpeakerUnseat { session, .. }
        | Request::Forward { session, .. }
        | Request::Refuse { session, .. }
            if session != &SessionSelector::default() =>
        {
            SessionOrRead
        }

        Request::DeskWorker { .. }
        | Request::AuthorizeNativeAuthority { .. }
        | Request::RebindHost { .. }
        | Request::Restore { .. }
        | Request::Replace { .. }
        | Request::Stop { .. }
        | Request::Pause { .. }
        | Request::Resume { .. }
        | Request::SessionForget { .. }
        | Request::PruneHosts
        | Request::RestartHost { .. }
        | Request::Attach { .. }
        | Request::ArchiveCommittee { .. }
        | Request::EnsureSecretary
        | Request::SetAlias { .. }
        | Request::RemoveAlias { .. }
        | Request::SetCommitteeSeat { .. }
        | Request::DeskDecide { .. }
        | Request::SpeakerAlias { .. }
        | Request::SpeakerUnseat { .. }
        | Request::Forward { .. }
        | Request::Refuse { .. }
        | Request::SetChair { .. }
        | Request::DeskOpenCommittee { .. }
        | Request::SetSecretarySession { .. }
        | Request::Approve { .. }
        | Request::ApproveSession { .. } => Presence,

        Request::Secretaries { action, .. } => {
            if *action == ipc::SecretaryOp::List {
                SessionOrRead
            } else {
                Presence
            }
        }
        Request::Desk { set } => {
            if set.is_some() {
                Presence
            } else {
                SessionOrRead
            }
        }
        Request::Spawn {
            chair, secretary, ..
        } => {
            if *chair || *secretary {
                Presence
            } else {
                Operator
            }
        }
        Request::Answer {
            as_person, session, ..
        }
        | Request::Duty {
            as_person, session, ..
        }
        | Request::Task {
            as_person, session, ..
        } => {
            if *as_person || session == &SessionSelector::default() {
                PersonWrite
            } else {
                SessionOrRead
            }
        }
        Request::Ask { session, .. }
        | Request::Post { session, .. }
        | Request::Send { session, .. }
        | Request::ChatSend { session, .. } => {
            if session == &SessionSelector::default() {
                PersonWrite
            } else {
                SessionOrRead
            }
        }
        Request::Thread { mark_seen, .. } => {
            if *mark_seen {
                Operator
            } else {
                SessionOrRead
            }
        }
        Request::RemoteControl { command } => {
            if matches!(
                command,
                crate::remote::Control::Devices | crate::remote::Control::Candidate
            ) {
                SessionOrRead
            } else {
                Presence
            }
        }
        Request::ObserveAdmin { command } => {
            use senate_observe::AdminRequest;
            if matches!(
                command,
                AdminRequest::Create { .. }
                    | AdminRequest::Revoke { .. }
                    | AdminRequest::Publish { .. }
                    | AdminRequest::Withdraw { .. }
            ) {
                Presence
            } else {
                Operator
            }
        }
        Request::SetCard { session, .. } => {
            if session == &SessionSelector::default() {
                Presence
            } else {
                SessionOrRead
            }
        }
        Request::OpenViewer { writable, .. } => {
            if *writable {
                Presence
            } else {
                Operator
            }
        }
        // A session acts on its own scratch terminals (the daemon checks
        // whose each one is); without a session it is the person's.
        Request::Terminal { session, .. } => {
            if session == &SessionSelector::default() {
                Operator
            } else {
                SessionOrRead
            }
        }

        Request::ServerHealth
        | Request::Ping
        | Request::Status
        | Request::Profiles
        | Request::HookDropped(..)
        | Request::Hook(..)
        | Request::SessionHook(..)
        | Request::NativeAuthorityCandidate { .. }
        | Request::Sessions
        | Request::SessionDrop { .. }
        | Request::Stats { .. }
        | Request::SessionHistory { .. }
        | Request::SessionSearch { .. }
        | Request::Timeline { .. }
        | Request::CommitteeThreads { .. }
        | Request::ActivityWatch { .. }
        | Request::TimelineWatch { .. }
        | Request::AttachInfo { .. }
        | Request::AttachChoices
        | Request::ResolveSessionRole { .. }
        | Request::SessionRoleNames
        | Request::Waiting { .. }
        | Request::SessionEnd { .. }
        | Request::HostedSessionEnd { .. }
        | Request::HostedClaudeApproval { .. }
        | Request::CatchUp { .. }
        | Request::Join { .. }
        | Request::ArchivedCommittees
        | Request::FindSession { .. }
        | Request::DefaultSecretary
        | Request::Roster
        | Request::Aliases
        | Request::Receipt { .. }
        | Request::JoinAs { .. }
        | Request::Memory { .. }
        | Request::Search { .. }
        | Request::Brief { .. }
        | Request::Bound { .. }
        | Request::Escalate { .. }
        | Request::ChatThread { .. }
        | Request::Inbox { .. }
        | Request::Presides { .. }
        | Request::DeskItem { .. }
        | Request::Docket { .. }
        | Request::Route { .. }
        | Request::Cover { .. }
        | Request::Hold { .. }
        | Request::Refer { .. }
        | Request::ChairDocket { .. }
        | Request::ChairRoute { .. }
        | Request::ChairCover { .. }
        | Request::SecretaryCover { .. }
        | Request::ChairEscalate { .. }
        | Request::SecretaryWork { .. }
        | Request::Roles
        | Request::Approvals
        | Request::Checkpoint { .. } => SessionOrRead,
    }
}

fn routine_operator_action(request: &Request) -> bool {
    // Observe exercises the actual caller check in disposable fixtures too.
    if matches!(request, Request::ObserveAdmin { .. }) {
        return false;
    }
    if matches!(
        request_effect(request),
        RequestEffect::PersonWrite | RequestEffect::Operator
    ) {
        return true;
    }
    matches!(
        request,
        Request::FindSession { .. } | Request::OpenViewer { .. }
    ) || matches!(request,
        Request::Escalate { session, .. }
        | Request::Checkpoint { session, .. }
        | Request::Join { session, .. }
        | Request::JoinAs { session, .. }
        | Request::Inbox { session, .. }
        | Request::CatchUp { session, .. }
        if session == &SessionSelector::default()
    )
}

/// What only the person does, and what to tell an agent that asks: `None`
/// for any other request.
pub(crate) fn person_only(request: &Request) -> Option<&'static str> {
    Some(match request {
        request if routine_operator_action(request) => {
            "Run this operator command from your own terminal or the Senate app"
        }
        Request::SetCard { session, .. } if session == &SessionSelector::default() => {
            "Manage your profile card from your own terminal or the Senate app"
        }
        Request::DeskWorker { .. } => {
            "Manage desk-worker credentials from your own terminal or the Senate app"
        }
        Request::ObserveAdmin { .. } => {
            "Manage Observe grants from your own terminal or the Senate app"
        }
        Request::NativeAuthorityCandidate { .. } | Request::AuthorizeNativeAuthority { .. } => {
            "Authorize a native seat from your own terminal or the Senate app"
        }
        Request::RestartHost { .. } => "Restart hosted sessions from your own terminal",
        Request::RebindHost { .. } => "Run `senate session rebind` from your own terminal",
        Request::Timeline { .. }
        | Request::SessionHistory { .. }
        | Request::Stats { .. }
        | Request::SessionSearch { .. }
        | Request::SessionForget { .. } => {
            "Read session transcripts from your own terminal or the Senate app"
        }
        Request::CommitteeThreads { .. } => {
            "Read committee threads from your own terminal or the Senate app"
        }
        // As the person (no session). With a session it is the Speaker's
        // office's, checked by the front and the server.
        Request::SetChair { session, .. }
            if session.session_id.is_none() && session.claude_pid.is_none() =>
        {
            "Naming a chair as the person is the person's to do: run `senate senator chair` from your own terminal"
        }
        // An agent names its own session with `join`, and the desk acts for
        // the office with its own tools.
        Request::SetAlias { .. }
        | Request::RemoveAlias { .. }
        | Request::SetCommitteeSeat { .. } => {
            "Aliases and seats set as the person are the person's to change: run the command from your own terminal (a session names itself with `join`; the Speaker's desk has speaker_alias and speaker_unseat)"
        }
        Request::ArchiveCommittee { .. } => {
            "Archiving a committee is the Speaker's own act: run `senate committees archive` from your own terminal"
        }
        Request::EnsureSecretary => {
            "Only the person starts the default secretary: ask it from your own terminal or the Senate app"
        }
        request if answers_a_prompt(request) => {
            "A permission prompt is a person's decision: answer it from your own terminal or the Senate app"
        }
        _ => return None,
    })
}

/// Host upgrades and viewers still use the operator's available proof
/// when this build has no device-owner authentication path.
pub(super) fn advisory_host_action(request: &Request) -> bool {
    if !matches!(
        request,
        Request::RestartHost { .. } | Request::OpenViewer { .. }
    ) {
        return false;
    }
    #[cfg(target_os = "macos")]
    if signed_local_daemon() {
        return false;
    }
    true
}

/// Operations documented as operator-only which previously relied on their CLI
/// entry point. Enforce these at a protected scope boundary as well.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn operator_only(request: &Request) -> bool {
    match request {
        Request::Replace { .. }
        | Request::Pause { .. }
        | Request::Resume { .. }
        | Request::PruneHosts
        | Request::SetSecretarySession { .. }
        | Request::DeskItem { .. }
        | Request::Approvals
        | Request::Desk { set: Some(_) }
        | Request::Attach { .. } => true,
        Request::Restore { session, .. }
        | Request::Spawn { session, .. }
        | Request::Stop { session, .. }
        | Request::SecretaryWork { session, .. }
        | Request::Docket { session, .. } => session == &SessionSelector::default(),
        _ => false,
    }
}

/// Every explicit session selector carries the authority of its runtime,
/// including requests that only read private session state.
pub(crate) fn session_selector(request: &Request) -> Option<&SessionSelector> {
    super::front::selector_of(request).filter(|selector| **selector != SessionSelector::default())
}

pub(crate) fn session_id_claim(request: &Request) -> Option<&str> {
    match request {
        Request::SessionEnd { session_id, .. }
        | Request::Waiting { session_id }
        | Request::HostedClaudeApproval { session_id, .. } => Some(session_id),
        _ => None,
    }
}

impl Front {
    /// Accept process metadata from the kernel's view of the hook caller.
    pub(super) async fn bind_hook_process_claims(
        &self,
        peer: Option<u32>,
        report: &mut HookReport,
    ) -> Result<(), IpcError> {
        if report.runtime == Runtime::Agy && report.host.is_some() {
            return self.verify_agy_host_hook(peer, report).await;
        }
        let peer = peer.ok_or_else(|| IpcError::bad_request("cannot identify the hook caller"))?;
        // In-process clients already share the daemon's authority.
        if peer == std::process::id() {
            return Ok(());
        }
        let stored = self
            .primary()
            .store
            .session(&report.session_id)
            .map_err(internal)?
            .filter(|session| session.state != SessionState::Ended);
        if stored
            .as_ref()
            .is_some_and(|session| session.runtime != report.runtime)
        {
            return Err(hook_owner_mismatch());
        }
        if report.runtime == Runtime::Agy
            && self
                .primary()
                .store
                .session_host(&report.session_id)
                .map_err(internal)?
                .is_some()
        {
            return Err(hook_owner_mismatch());
        }
        let recorded_owner = stored
            .and_then(|session| session.claude_pid)
            .and_then(|pid| kernel_process::inspect(pid).ok())
            .map(|process| process.identity);
        if report.runtime != Runtime::Claude {
            if let Some(owner) = recorded_owner
                && kernel_process::inspect(peer)
                    .ok()
                    .is_none_or(|process| process.identity != owner)
            {
                return Err(hook_owner_mismatch());
            }
            report.claude_pid = report
                .claude_pid
                .filter(|pid| report.runtime == Runtime::Acp && *pid == peer);
            report.ancestor_pids.clear();
            return Ok(());
        }
        let _permit = self
            .process_checks
            .acquire()
            .await
            .expect("process check budget is never closed");
        let ancestors = kernel_process::ancestors(peer, DEPTH)
            .map_err(|_| IpcError::bad_request("cannot verify the hook caller's ancestry"))?;
        if recorded_owner.is_some_and(|owner| !ancestors.contains(&owner)) {
            return Err(hook_owner_mismatch());
        }
        report.claude_pid = report
            .claude_pid
            .filter(|pid| ancestors.iter().any(|process| process.pid == *pid));
        report.ancestor_pids = ancestors.iter().map(|process| process.pid).collect();
        Ok(())
    }

    /// Replace the hook's claimed ancestry with the Unix peer's actual
    /// process path before letting it name or rebind a hosted agy.
    async fn verify_agy_host_hook(
        &self,
        peer: Option<u32>,
        report: &mut HookReport,
    ) -> Result<(), IpcError> {
        let wrong_host =
            || IpcError::bad_request("the agy hook caller is not inside the claimed terminal host");
        let peer = peer.ok_or_else(wrong_host)?;
        let paths = Paths::new(&self.primary().home);
        let host = report.host.as_ref().ok_or_else(wrong_host)?;
        let child = host::child_pid(&paths, host).ok_or_else(wrong_host)?;
        // Avoid process queries for obviously unrelated claims. The
        // supplied list is only a hint and is replaced before dispatch.
        if !report.ancestor_pids.contains(&child) {
            return Err(wrong_host());
        }
        let _permit = self
            .process_checks
            .acquire()
            .await
            .expect("process check budget is never closed");
        let ancestors = kernel_process::ancestors(peer, 15).map_err(|_| wrong_host())?;
        let process = ancestors
            .iter()
            .copied()
            .find(|process| process.pid == child)
            .ok_or_else(wrong_host)?;
        report.claude_pid = None;
        report.ancestor_pids = ancestors.iter().map(|process| process.pid).collect();
        let mut observed = lock(&self.primary().agy_host_observed);
        observed.retain(|_, proof| proof.at.elapsed() <= AGY_HOST_PROOF_TTL);
        observed.insert(
            host.clone(),
            AgyHostObservation {
                process,
                session_id: report.session_id.clone(),
                at: Instant::now(),
            },
        );
        Ok(())
    }

    pub(super) async fn verify_card_session(
        &self,
        peer: Option<u32>,
        session: &Session,
    ) -> Result<(), IpcError> {
        let no_identity =
            || IpcError::bad_request("Cannot verify the calling process owns the card's session");
        let peer = peer.ok_or_else(no_identity)?;
        let _permit = self
            .process_checks
            .acquire()
            .await
            .map_err(|_| no_identity())?;
        let shared = self.primary();
        if !session.is_live() {
            return Err(no_identity());
        }
        let mut allowed = Vec::new();
        allowed.extend(session.claude_pid);
        if let Some(host_id) = shared
            .store
            .session_host(&session.session_id)
            .map_err(internal)?
        {
            let paths = Paths::new(&shared.home);
            allowed.extend(host::child_pid(&paths, &host_id));
        }
        if allowed.is_empty() {
            return Err(no_identity());
        }
        let caller = kernel_process::inspect(peer).map_err(|_| no_identity())?;
        let ancestors = kernel_process::ancestors(peer, DEPTH).map_err(|_| no_identity())?;
        if !allowed
            .into_iter()
            .filter_map(|pid| kernel_process::inspect(pid).ok())
            .any(|owner| owner.identity == caller.identity || ancestors.contains(&owner.identity))
        {
            return Err(no_identity());
        }
        Ok(())
    }

    /// Serve `request` unless its caller is inside a coding session's
    /// process tree ([`person_only`]).
    pub(super) async fn guarded_dispatch(
        self: &Arc<Self>,
        peer: Option<u32>,
        guidance: &str,
        request: Request,
    ) -> Reply {
        if self.scope_fixture && routine_operator_action(&request) {
            return self.dispatch(request).await;
        }
        match self.refuse_agent_caller(peer, guidance).await {
            Ok(()) => self.dispatch(request).await,
            Err(refused) => Reply::err(refused),
        }
    }

    #[cfg(target_os = "macos")]
    pub(super) async fn app_confirmed_dispatch<R: tokio::io::AsyncBufRead + Unpin>(
        self: &Arc<Self>,
        peer: PresencePeer,
        request: Request,
        reader: &mut R,
        mut shutdown: watch::Receiver<bool>,
    ) -> Reply {
        use crate::app_confirmation::{EnqueueError, LIFETIME};
        use tokio::io::AsyncBufReadExt;

        let home = &self.primary().home;
        let pin_change = crate::app_confirmation::macos::pending_change(home);
        let session_action =
            !request_effect(&request).requires_operator() && session_selector(&request).is_some();
        let requester_pid = if session_action {
            peer.session
        } else {
            if let Err(error) = self
                .refuse_agent_caller(peer.operator, APP_CONFIRMATION)
                .await
            {
                return Reply::err(error);
            }
            peer.operator
        };
        let requester_instance = requester_pid
            .and_then(|pid| kernel_process::inspect(pid).ok())
            .and_then(|process| kernel_process::BoundProcess::capture(process.identity).ok());
        let require_touch = answers_a_prompt(&request)
            && crate::app_confirmation::macos::approvals_require_touch(home);
        if peer.signed_app && !session_action && pin_change.is_none() && !require_touch {
            if let Request::OpenViewer { host, writable } = &request {
                return self.broker_viewer(host, *writable, peer.operator).await;
            }
            return self
                .dispatch_app_confirmation(
                    self.app_confirmations.accept_signed_app(request),
                    peer.session,
                )
                .await;
        }
        let Some(requester) = requester_pid else {
            tracing::warn!("refused a confirmation: the request carried no proof of its caller");
            return Reply::err(refusal(NO_PROOF));
        };
        if peer.remote_operator || crate::mac_scope::behind_ssh(requester) != Some(false) {
            tracing::warn!(
                pid = requester,
                "refused a confirmation: the requester is behind SSH or unreadable"
            );
            return Reply::err(refusal(APP_CONFIRMATION));
        }
        let view = match &request {
            Request::OpenViewer {
                host,
                writable: true,
            } => Some(host.clone()),
            _ => None,
        };
        let terminal = view
            .as_ref()
            .and_then(|_| requester_pid.and_then(kernel_process::terminal_identity));
        if let (Some(host), Some(terminal)) = (&view, &terminal)
            && pin_change.is_none()
            && self.app_confirmations.terminal_ok(terminal)
        {
            return self.broker_viewer(host, true, requester_pid).await;
        }
        let requester = requester_pid.map_or_else(
            || "unidentified process".to_owned(),
            |pid| {
                let program = kernel_process::ancestors(pid, DEPTH)
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|process| kernel_process::executable(process.pid).ok())
                    .find_map(|path| {
                        let name = path.file_name()?.to_str()?;
                        matches!(name, "tmux" | "screen" | "Terminal" | "iTerm2" | "ghostty")
                            .then(|| name.to_owned())
                    })
                    .or_else(|| {
                        kernel_process::executable(pid)
                            .ok()?
                            .file_name()?
                            .to_str()
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| "process".to_owned());
                let program: String = program
                    .chars()
                    .take(64)
                    .flat_map(char::escape_default)
                    .collect();
                let terminal = kernel_process::terminal(pid)
                    .map(|name| format!(", {}", name.escape_default()))
                    .unwrap_or_default();
                let job = if kernel_process::terminal_foreground(pid) == Some(false) {
                    ", background job"
                } else {
                    ""
                };
                format!("{program} (PID {pid}{terminal}{job})")
            },
        );
        let mut reason = crate::app_confirmation::request_reason(&request, &requester);
        if let Some(host) = &view
            && let Ok(Some(session_id)) = self.primary().store.session_for_host(host)
        {
            let alias = self.primary().shown_session_name(&session_id);
            reason = format!(
                "Open a writable terminal for {:?} — requested by {requester}",
                alias
            );
        }
        let relay_prompt = if let Request::Approve { id, decision } = &request {
            let Some(approval) = self
                .primary()
                .relay
                .list()
                .into_iter()
                .find(|approval| &approval.id == id)
            else {
                return Reply::err(IpcError::bad_request(
                    "The permission prompt is no longer waiting",
                ));
            };
            let subject = self
                .primary()
                .store
                .session(&approval.session_id)
                .ok()
                .flatten()
                .map_or_else(
                    || approval.session_id.clone(),
                    |session| {
                        let alias = self.primary().shown_session_name(&session.session_id);
                        session.committee().map_or_else(
                            || alias.clone(),
                            |committee| format!("{committee}::{alias}"),
                        )
                    },
                );
            let command = approval
                .command
                .as_deref()
                .unwrap_or(&approval.summary)
                .lines()
                .next()
                .unwrap_or_default();
            let detail: String =
                format!("{decision:?} {:?} for {subject}: {command}", approval.kind)
                    .chars()
                    .take(512)
                    .flat_map(char::escape_default)
                    .collect();
            reason = format!("{detail} — requested by {requester}");
            Some(approval.received_at)
        } else {
            None
        };
        let approval_target = if let Request::ApproveSession { senator, decision } = &request {
            match self.primary().pending_hosted_approval(senator).await {
                Ok(target) => {
                    let subject: String = target
                        .description
                        .chars()
                        .take(120)
                        .flat_map(char::escape_default)
                        .collect();
                    reason = format!("{decision:?} {subject} — requested by {requester}");
                    Some(target)
                }
                Err(error) => return Reply::err(error),
            }
        } else {
            None
        };
        if let Request::AuthorizeNativeAuthority {
            session_id,
            approval,
        } = &request
        {
            match self.pending_native_details(session_id) {
                Ok(Some(candidate)) if &candidate.approval == approval => {
                    reason = format!(
                        "{reason}\nPID {} running {:?}, team {}, cwd {:?}, hosted {}, started {:?}",
                        candidate.pid,
                        candidate.executable,
                        candidate.signing_team.as_deref().unwrap_or("none verified"),
                        candidate.cwd,
                        candidate.hosted,
                        candidate.started_at
                    );
                }
                Ok(_) => {
                    return Reply::err(IpcError::bad_request(
                        "The pending process changed before authorization",
                    ));
                }
                Err(error) => return Reply::err(error),
            }
        }
        if let Some(notice) = crate::app_confirmation::macos::change_notice(home) {
            reason = format!("{notice}\n{reason}");
        }
        let authorization = async {
            if pin_change.is_some() {
                return Ok(self
                    .app_confirmations
                    .authenticate_helper(home, request, &reason)
                    .await);
            }
            match self.app_confirmations.enqueue(request.clone(), &reason) {
                Ok(mut ticket) => Ok((&mut ticket.reply).await.ok().flatten()),
                Err(EnqueueError::AppUnavailable) => Ok(self
                    .app_confirmations
                    .authenticate_helper(home, request, &reason)
                    .await),
                Err(EnqueueError::Busy) => {
                    Err(refusal("The Senate app has too many pending confirmations"))
                }
                Err(EnqueueError::RequestTooLarge) => Err(IpcError::bad_request(
                    "The confirmation request is too large",
                )),
            }
        };
        let approved = tokio::select! {
            biased;
            _ = shutdown.changed() => None,
            _ = reader.fill_buf() => None,
            _ = tokio::time::sleep(LIFETIME) => None,
            result = authorization => match result {
                Ok(value) => value,
                Err(error) => return Reply::err(error),
            },
        };
        let Some(approved) = approved else {
            return Reply::err(refusal(APP_CONFIRMATION));
        };
        if !requester_instance.is_some_and(|process| process.live()) {
            return Reply::err(refusal(
                "The requesting process changed while waiting for authentication",
            ));
        }
        if crate::app_confirmation::macos::pending_change(home) != pin_change {
            return Reply::err(refusal(
                "The installed trust records changed while waiting for authentication",
            ));
        }
        if let Some(change) = &pin_change
            && let Err(error) = crate::app_confirmation::macos::accept_change(home, change)
        {
            return Reply::err(IpcError::bad_request(error.to_string()));
        }
        if let Some(host) = view {
            if let Some(terminal) = terminal
                && requester_pid
                    .and_then(kernel_process::terminal_identity)
                    .as_ref()
                    == Some(&terminal)
            {
                self.app_confirmations.trust_terminal(terminal);
            }
            return self.broker_viewer(&host, true, requester_pid).await;
        }
        if let Some(expected) = relay_prompt {
            let Request::Approve { id, decision } = approved.into_request() else {
                unreachable!("presence request is sealed to its original payload");
            };
            return wrap(
                self.primary()
                    .answer_desktop_approval(&id, decision, Some(expected))
                    .await,
            );
        }
        if let Some(target) = approval_target {
            let Request::ApproveSession { decision, .. } = approved.into_request() else {
                unreachable!("presence request is sealed to its original payload");
            };
            return wrap(
                self.primary()
                    .answer_confirmed_hosted_prompt(&target, decision)
                    .await,
            );
        }
        self.dispatch_app_confirmation(approved, peer.session).await
    }

    #[cfg(not(target_os = "macos"))]
    pub(super) async fn app_confirmed_dispatch<R: tokio::io::AsyncBufRead + Unpin>(
        self: &Arc<Self>,
        peer: PresencePeer,
        _request: Request,
        _reader: &mut R,
        _shutdown: watch::Receiver<bool>,
    ) -> Reply {
        let _ = (
            peer.operator,
            peer.session,
            peer.signed_app,
            peer.remote_operator,
        );
        Reply::err(refusal(APP_CONFIRMATION))
    }

    /// Refuse a caller inside a coding session's process tree.
    pub(super) async fn refuse_agent_caller(
        &self,
        peer: Option<u32>,
        guidance: &str,
    ) -> Result<(), IpcError> {
        let Some(peer) = peer else {
            tracing::warn!(guidance, "refused person-only IPC: no caller proof");
            return Err(refusal(NO_PROOF));
        };
        let me = std::process::id();
        // The daemon's own in-process clients (tests drive it so).
        if peer == me {
            return Ok(());
        }
        let _permit = self
            .process_checks
            .acquire()
            .await
            .expect("process check budget is never closed");
        let mut agents = BTreeMap::new();
        agents.insert(me, "the Senate daemon".to_owned());
        for shared in self.senates() {
            shared.agent_processes(&mut agents).map_err(|e| match e {
                StoreError::Io(ref source) if crate::fd::exhausted(source) => fd_refusal(peer),
                _ => internal(e),
            })?;
        }
        let chain = match ancestry(peer, process).await {
            Ok(chain) => chain,
            Err(ProcessLookup::Descriptors) => return Err(fd_refusal(peer)),
            Err(ProcessLookup::Unreadable | ProcessLookup::Foreign) => {
                tracing::warn!(
                    peer,
                    guidance,
                    "refused person-only IPC: the caller's ancestry is unreadable"
                );
                return Err(refusal(
                    "the daemon could not read the calling process's ancestry",
                ));
            }
        };
        let owner = chain.iter().find_map(|(pid, name)| {
            agents.get(pid).cloned().or_else(|| {
                RUNTIMES
                    .contains(&name.as_str())
                    .then(|| format!("a `{name}` process (pid {pid})"))
            })
        });
        match owner {
            Some(owner) => {
                tracing::warn!(peer, %owner, guidance, "refused person-only IPC from inside a coding session's process tree");
                Err(refusal(&format!(
                    "this request comes from inside the process tree of {owner}. {guidance}"
                )))
            }
            None => {
                tracing::debug!(
                    peer,
                    guidance,
                    "person-only IPC passed the process-tree check"
                );
                Ok(())
            }
        }
    }
}

impl Shared {
    /// The processes of this Senate's coding sessions, each with whose it is.
    fn agent_processes(&self, out: &mut BTreeMap<u32, String>) -> Result<(), StoreError> {
        for session in self.store.sessions()? {
            if session.state == SessionState::Ended {
                continue;
            }
            if let Some(pid) = session.claude_pid {
                out.insert(pid, Address::senator(&session.agent).in_text());
            }
        }
        let paths = Paths::new(&self.home);
        for id in host::running_ids(&paths)? {
            let owner = match self.store.session_for_host(&id)? {
                Some(native) => self
                    .store
                    .session(&native)?
                    .map(|s| Address::senator(&s.agent).in_text()),
                None => None,
            }
            .unwrap_or_else(|| format!("terminal host {id}"));
            for pid in [host::host_pid(&paths, &id), host::child_pid(&paths, &id)]
                .into_iter()
                .flatten()
            {
                out.insert(pid, owner.clone());
            }
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ProcessLookup {
    Unreadable,
    Descriptors,
    /// Owned by another user (a root `login` or `sshd` above a shell).
    Foreign,
}

impl From<std::io::Error> for ProcessLookup {
    fn from(error: std::io::Error) -> Self {
        if crate::fd::exhausted(&error) {
            Self::Descriptors
        } else if kernel_process::foreign(&error) {
            Self::Foreign
        } else {
            Self::Unreadable
        }
    }
}

fn fd_refusal(peer: u32) -> IpcError {
    tracing::warn!(
        peer,
        "refused person-only IPC: the daemon is out of file descriptors"
    );
    // Keep failing closed, but report a daemon error rather than implying
    // that the caller is an agent.
    IpcError::internal(
        "the daemon is out of file descriptors and could not verify the caller; try again",
    )
}

/// Read the parent PID and executable name from the kernel's process table.
async fn process(pid: u32) -> Result<(u32, String), ProcessLookup> {
    let process = kernel_process::inspect(pid)?;
    Ok((process.parent_pid, process.name))
}

/// `start` and every process above it up to pid 1, each with its name, or
/// an error when a process cannot be read before then.
async fn ancestry<F, Fut>(start: u32, read: F) -> Result<Vec<(u32, String)>, ProcessLookup>
where
    F: Fn(u32) -> Fut,
    Fut: std::future::Future<Output = Result<(u32, String), ProcessLookup>>,
{
    let mut chain = Vec::new();
    let mut pid = start;
    loop {
        if chain.len() >= DEPTH || chain.iter().any(|(seen, _)| *seen == pid) {
            return Err(ProcessLookup::Unreadable);
        }
        if pid <= 1 {
            chain.push((pid, String::new()));
            return Ok(chain);
        }
        let (parent, name) = match read(pid).await {
            // The user's own chain ends below another user's process, as
            // the kernel's own ancestry walk does.
            Err(ProcessLookup::Foreign) if pid != start => return Ok(chain),
            result => result?,
        };
        chain.push((pid, name));
        pid = parent;
    }
}

fn refusal(message: &str) -> IpcError {
    IpcError::new(codes::NOT_A_PERSON, message)
}

fn hook_owner_mismatch() -> IpcError {
    IpcError::bad_request("the hook caller cannot refresh that session's process binding")
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn request_effect_and_final_author_do_not_inherit_session_authority() {
        let session = serde_json::json!({"session_id":"fixture"});
        let examples = [
            (
                serde_json::json!({"op":"answer","session":session,"as_person":true,"thread":1,"body":"answer"}),
                RequestEffect::PersonWrite,
            ),
            (
                serde_json::json!({"op":"memory_edit","session":session,"targets":[1],"text":"correction"}),
                RequestEffect::PersonWrite,
            ),
            (
                serde_json::json!({"op":"take","session":session,"thread":1}),
                RequestEffect::PersonWrite,
            ),
            (
                serde_json::json!({"op":"secretaries","session":session,"action":"add","for":"operator","secretary":"agent"}),
                RequestEffect::Presence,
            ),
            (
                serde_json::json!({"op":"secretaries","session":session,"action":"list"}),
                RequestEffect::SessionOrRead,
            ),
            (
                serde_json::json!({"op":"answer","session":session,"thread":1,"body":"answer"}),
                RequestEffect::SessionOrRead,
            ),
        ];
        for (wire, expected) in examples {
            let request: Request = serde_json::from_value(wire).unwrap();
            assert_eq!(request_effect(&request), expected);
            assert_eq!(
                crate::app_confirmation::required(&request),
                expected == RequestEffect::Presence
            );
            if expected == RequestEffect::PersonWrite {
                assert!(person_only(&request).is_some());
            }
        }
        for origin in ["session", "unverified", "person-terminal", "app"] {
            VERIFIED_ORIGIN
                .scope(origin, async {
                    assert!(check_person_author(false).is_ok());
                    assert_eq!(
                        check_person_author(true).is_ok(),
                        matches!(origin, "person-terminal" | "app")
                    );
                })
                .await;
        }
    }

    #[test]
    fn person_and_session_writes_require_their_own_proofs() {
        let person = SessionSelector::default();
        let runtime = SessionSelector {
            session_id: Some("runtime".into()),
            claude_pid: None,
        };
        let post = |session| Request::Post {
            session,
            body: "fixture".into(),
            proposal: false,
            committee: None,
            agent: None,
            id: None,
        };
        assert!(person_only(&post(person)).is_some());
        assert!(person_only(&post(runtime.clone())).is_none());
        assert_eq!(session_selector(&post(runtime.clone())), Some(&runtime));
        let end = Request::SessionEnd {
            session_id: "runtime".into(),
            reason: None,
        };
        assert_eq!(session_id_claim(&end), Some("runtime"));
        assert_eq!(
            session_id_claim(&Request::Waiting {
                session_id: "runtime".into()
            }),
            Some("runtime")
        );
        assert!(crate::app_confirmation::required(&Request::SessionForget {
            session_id: "runtime".into()
        }));
        for writable in [false, true] {
            let request = Request::OpenViewer {
                host: EventId::new(),
                writable,
            };
            assert!(person_only(&request).is_some());
            assert_eq!(crate::app_confirmation::required(&request), writable);
        }
    }

    #[test]
    fn the_persons_own_acts_are_guarded_and_a_sessions_are_not() {
        use senate_core::CommitteeSeatAction;
        let person = SessionSelector::default();
        let session = SessionSelector {
            session_id: Some("s".into()),
            claude_pid: None,
        };
        let chair = |session| Request::SetChair {
            session,
            committee: None,
            senator: "codex-1".parse().unwrap(),
        };
        assert!(person_only(&chair(person)).is_some_and(|g| g.contains("senate senator chair")));
        assert_eq!(
            person_only(&chair(session.clone())),
            None,
            "the desk's speaker_chair"
        );
        for request in [
            Request::SetAlias {
                senator: senate_core::alias::AliasName::bare("codex-1".parse().unwrap()),
                alias: "main".parse().unwrap(),
                committee: None,
            },
            Request::RemoveAlias {
                primary: "codex-1".parse().unwrap(),
                alias: "main".parse().unwrap(),
                committee: None,
            },
            Request::SetCommitteeSeat {
                committee: senate_core::CommitteeId::new("ibot").unwrap(),
                request: senate_core::api::SetCommitteeSeat {
                    senator: "codex-1".parse().unwrap(),
                    action: CommitteeSeatAction::Unseat,
                    id: Default::default(),
                    acted_by: None,
                },
            },
        ] {
            assert!(
                person_only(&request).is_some_and(|g| g.contains("speaker_alias")),
                "{request:?}"
            );
        }
        // A session naming itself is the session's own act.
        assert_eq!(
            person_only(&Request::JoinAs {
                session,
                committee: senate_core::CommitteeId::new("ibot").unwrap(),
                senator: "main".parse().unwrap(),
            }),
            None
        );
        assert!(person_only(&Request::EnsureSecretary).is_some());
        assert_eq!(person_only(&Request::Ping), None);
    }

    use super::*;

    #[tokio::test]
    async fn the_chain_is_read_to_pid_1_or_not_at_all() {
        let table: BTreeMap<u32, (u32, &str)> =
            [(40, (30, "senate")), (30, (20, "zsh")), (20, (1, "claude"))].into();
        let read = |pid: u32| {
            let found = table.get(&pid).map(|(p, n)| (*p, (*n).to_owned()));
            async move { found.ok_or(ProcessLookup::Unreadable) }
        };
        let pids =
            |chain: Vec<(u32, String)>| chain.into_iter().map(|(p, _)| p).collect::<Vec<_>>();
        assert_eq!(pids(ancestry(40, read).await.unwrap()), vec![40, 30, 20, 1]);
        assert_eq!(ancestry(40, read).await.unwrap()[2].1, "claude");
        // A process that cannot be read fails closed.
        assert_eq!(ancestry(41, read).await, Err(ProcessLookup::Unreadable));
        // A loop is not a chain.
        let cycle: BTreeMap<u32, (u32, &str)> = [(40, (30, "a")), (30, (40, "b"))].into();
        let read = |pid: u32| {
            let found = cycle.get(&pid).map(|(p, n)| (*p, (*n).to_owned()));
            async move { found.ok_or(ProcessLookup::Unreadable) }
        };
        assert_eq!(ancestry(40, read).await, Err(ProcessLookup::Unreadable));
        // Another user's process (a root `login` under a terminal app) ends
        // the user's chain; the caller itself must still be readable.
        let read = |pid: u32| async move {
            match pid {
                40 => Ok((30, "senate".to_owned())),
                30 => Ok((20, "zsh".to_owned())),
                _ => Err(std::io::Error::from_raw_os_error(libc::EPERM).into()),
            }
        };
        assert_eq!(pids(ancestry(40, read).await.unwrap()), vec![40, 30]);
        let read = |_| async move { Err(std::io::Error::from_raw_os_error(libc::EPERM).into()) };
        assert_eq!(ancestry(40, read).await, Err(ProcessLookup::Foreign));
    }

    #[tokio::test]
    async fn descriptor_exhaustion_is_reported_as_a_daemon_error() {
        for errno in [libc::EMFILE, libc::ENFILE] {
            let read = |_| async move { Err(std::io::Error::from_raw_os_error(errno).into()) };
            assert_eq!(ancestry(40, read).await, Err(ProcessLookup::Descriptors));
        }
        assert!(
            fd_refusal(40)
                .to_string()
                .contains("out of file descriptors")
        );
    }

    #[tokio::test]
    async fn this_process_has_a_readable_chain() {
        let me = std::process::id();
        let chain = ancestry(me, process).await.unwrap();
        assert_eq!(chain.first().map(|(pid, _)| *pid), Some(me));
        // To launchd, or to a root `login` when run from a terminal app.
        let (top, _) = chain.last().unwrap();
        assert!(*top == 1 || process(*top).await.is_ok());
    }
}
