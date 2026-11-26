//! Per-request confirmation from the signed macOS application.

use senate_core::EventId;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::Paths;

pub(crate) const LIFETIME: Duration = Duration::from_secs(60);

#[derive(Serialize, Deserialize)]
pub struct Prompt {
    pub id: EventId,
    pub display: String,
    pub reason: String,
    pub age_seconds: u64,
}

#[cfg(target_os = "macos")]
pub(crate) use queue::{ConfirmedRequest, EnqueueError, Queue};

#[cfg(target_os = "macos")]
mod queue {
    use super::*;
    use crate::ipc::Request;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;
    use tokio::sync::oneshot;

    const CAPACITY: usize = 16;
    const APP_RECENCY: Duration = Duration::from_secs(5);
    /// The most a prompt's display text runs to.
    const DISPLAY_LIMIT: usize = 16 * 1024;

    #[derive(Default)]
    pub(crate) struct Queue {
        state: Mutex<State>,
        helper: tokio::sync::Mutex<()>,
    }

    #[derive(Default)]
    struct State {
        last_app_poll: Option<Instant>,
        waiting: BTreeMap<EventId, Pending>,
    }

    pub(crate) struct ConfirmedRequest(Request);

    impl ConfirmedRequest {
        pub(crate) fn into_request(self) -> Request {
            self.0
        }
    }

    struct Pending {
        request: Request,
        display: String,
        reason: String,
        created: Instant,
        result: oneshot::Sender<Option<ConfirmedRequest>>,
    }

    pub(crate) struct Ticket {
        id: EventId,
        queue: Arc<Queue>,
        pub(crate) reply: oneshot::Receiver<Option<ConfirmedRequest>>,
    }

    #[derive(Debug, Eq, PartialEq)]
    pub(crate) enum EnqueueError {
        AppUnavailable,
        Busy,
        RequestTooLarge,
    }

    impl State {
        fn expire(&mut self, now: Instant) {
            self.waiting
                .retain(|_, item| now.duration_since(item.created) < LIFETIME);
        }
    }

    impl Queue {
        pub(crate) fn accept_signed_app(&self, request: Request) -> ConfirmedRequest {
            ConfirmedRequest(request)
        }

        pub(crate) async fn authenticate_helper(
            &self,
            home: &std::path::Path,
            request: Request,
            reason: &str,
        ) -> Option<ConfirmedRequest> {
            let _exclusive = self.helper.try_lock().ok()?;
            macos::authenticate_helper(home, reason)
                .await
                .then_some(ConfirmedRequest(request))
        }

        pub(crate) fn enqueue(
            self: &Arc<Self>,
            request: Request,
            reason: &str,
        ) -> Result<Ticket, EnqueueError> {
            let value =
                serde_json::to_value(&request).map_err(|_| EnqueueError::RequestTooLarge)?;
            let display =
                serde_json::to_string_pretty(&value).map_err(|_| EnqueueError::RequestTooLarge)?;
            // Keep untrusted text visibly escaped in the signed app's prompt.
            let mut escaped = String::with_capacity(display.len());
            for ch in display.chars() {
                if ch.is_ascii() {
                    escaped.push(ch);
                } else {
                    escaped.extend(ch.escape_unicode());
                }
            }
            let display = format!("{reason}\n\n{escaped}");
            // The request dump is detail for an app that shows it; the reason
            // is what the person approves. A request too large to show (a
            // cleanup plan of a whole Senate) is shown by its reason alone,
            // never refused for its size.
            let display = if display.len() > DISPLAY_LIMIT {
                format!(
                    "{reason}\n\n(the request, {} bytes, is too large to show here)",
                    escaped.len()
                )
            } else {
                display
            };
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let now = Instant::now();
            state.expire(now);
            if !state
                .last_app_poll
                .is_some_and(|last| now.duration_since(last) < APP_RECENCY)
            {
                return Err(EnqueueError::AppUnavailable);
            }
            if state.waiting.len() >= CAPACITY {
                return Err(EnqueueError::Busy);
            }
            let id = EventId::new();
            let (result, reply) = oneshot::channel();
            state.waiting.insert(
                id.clone(),
                Pending {
                    request,
                    display,
                    reason: reason.to_owned(),
                    created: now,
                    result,
                },
            );
            Ok(Ticket {
                id,
                queue: self.clone(),
                reply,
            })
        }

        #[cfg(debug_assertions)]
        pub(crate) fn confirm_fixture(&self) -> Option<bool> {
            self.poll()
                .first()
                .map(|prompt| self.respond(&prompt.id, true))
        }

        // Only messages from the code-authenticated app reach these methods.
        pub(super) fn poll(&self) -> Vec<Prompt> {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let now = Instant::now();
            state.last_app_poll = Some(now);
            state.expire(now);
            let mut waiting: Vec<_> = state.waiting.iter().collect();
            waiting.sort_by_key(|(_, pending)| pending.created);
            waiting
                .into_iter()
                .map(|(id, item)| Prompt {
                    id: id.clone(),
                    display: item.display.clone(),
                    reason: item.reason.clone(),
                    age_seconds: now.duration_since(item.created).as_secs(),
                })
                .collect()
        }

        pub(super) fn respond(&self, id: &EventId, approved: bool) -> bool {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.expire(Instant::now());
            let Some(pending) = state.waiting.remove(id) else {
                return false;
            };
            pending
                .result
                .send(approved.then_some(ConfirmedRequest(pending.request)))
                .is_ok()
        }
    }

    impl Drop for Ticket {
        fn drop(&mut self) {
            self.queue
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .waiting
                .remove(&self.id);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn approval_is_bound_to_one_queued_request() {
            let queue = Arc::new(Queue::default());
            assert!(matches!(
                queue.enqueue(Request::Ping, "test terminal"),
                Err(EnqueueError::AppUnavailable)
            ));
            assert!(queue.poll().is_empty());
            let mut first = queue.enqueue(Request::Ping, "test terminal").unwrap();
            let mut second = queue.enqueue(Request::Status, "test terminal").unwrap();
            assert_ne!(first.id, second.id);
            assert!(!queue.respond(&EventId::new(), true));
            assert!(queue.respond(&first.id, true));
            assert!(matches!(
                first.reply.try_recv(),
                Ok(Some(ConfirmedRequest(Request::Ping)))
            ));
            assert!(!queue.respond(&first.id, true));
            assert!(queue.respond(&second.id, false));
            assert!(matches!(second.reply.try_recv(), Ok(None)));
        }

        #[test]
        fn dropped_or_expired_requests_cannot_be_approved() {
            let queue = Arc::new(Queue::default());
            queue.poll();
            let dropped = queue.enqueue(Request::Ping, "test terminal").unwrap();
            let id = dropped.id.clone();
            drop(dropped);
            assert!(!queue.respond(&id, true));
            let expired = queue.enqueue(Request::Ping, "test terminal").unwrap();
            queue
                .state
                .lock()
                .unwrap()
                .waiting
                .get_mut(&expired.id)
                .unwrap()
                .created -= LIFETIME;
            assert!(!queue.respond(&expired.id, true));
        }

        #[test]
        fn a_request_too_large_to_show_is_shown_by_its_reason() {
            // A whole Senate's cleanup plan runs to about 100 KB.
            let queue = Arc::new(Queue::default());
            queue.poll();
            let big = Request::SetCard {
                session: Default::default(),
                target: crate::ipc::CardTarget::Own,
                document: "x".repeat(100_000),
            };
            let mut ticket = queue
                .enqueue(big, "Senate cleanup: unseat 110")
                .expect("a large request is still put to the app");
            let prompt = queue.poll().pop().expect("the app sees it");
            assert!(
                prompt.display.len() <= DISPLAY_LIMIT,
                "{}",
                prompt.display.len()
            );
            assert!(prompt.display.starts_with("Senate cleanup: unseat 110"));
            assert_eq!(prompt.reason, "Senate cleanup: unseat 110");
            assert!(queue.respond(&ticket.id, true));
            assert!(matches!(
                ticket.reply.try_recv(),
                Ok(Some(ConfirmedRequest(Request::SetCard { .. })))
            ));
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn request_reason(request: &crate::ipc::Request, requester: &str) -> String {
    use crate::ipc::Request;
    use senate_observe::AdminRequest;
    let action = match request {
        Request::ApproveSession { senator, decision } => {
            format!("{decision:?} permission prompt for {senator}")
        }
        Request::Approve { id, decision } => format!("{decision:?} permission prompt {id}"),
        Request::AuthorizeNativeAuthority { session_id, .. } => {
            format!("Authorize native seat {session_id}")
        }
        Request::DeskWorker {
            command:
                senate_core::worker::Admin::Issue {
                    worker,
                    committees,
                    desk_inbox,
                },
        } => {
            let names = committees
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let mut scopes = Vec::new();
            if !names.is_empty() {
                scopes.push(format!("committees {names}"));
            }
            if let Some(inbox) = desk_inbox {
                scopes.push(format!("desk intake in {inbox}"));
            }
            format!(
                "Issue a scoped desk-worker credential for {worker} with {}",
                scopes.join(" and ")
            )
        }
        Request::DeskWorker {
            command: senate_core::worker::Admin::Rotate { id },
        } => format!("Rotate the secret for desk-worker credential {id}"),
        Request::DeskWorker {
            command: senate_core::worker::Admin::Revoke { id },
        } => format!("Revoke desk-worker credential {id}"),
        Request::ObserveAdmin {
            command: AdminRequest::Create { grant },
        } => {
            let committees = if grant.all_committees {
                "all committees".into()
            } else {
                grant
                    .committees
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            format!(
                "Create Observe {:?} access named {:?} for {committees}",
                grant.scope, grant.name
            )
        }
        Request::ObserveAdmin {
            command: AdminRequest::Publish { committee, seq, .. },
        } => format!("Publish the preview of {committee} #{seq} to eligible observers"),
        Request::CleanupApply { plan } => plan.summary(),
        Request::Secretaries {
            action,
            for_,
            secretary,
            ..
        } => {
            let principal = for_
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "the selected principal".into());
            let secretary = secretary
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "the secretary".into());
            format!("{action:?} {secretary} as a secretary for {principal}")
        }
        Request::SetSecretarySession {
            target_session,
            senator,
            appointed,
            ..
        } => {
            let verb = if *appointed { "Appoint" } else { "Dismiss" };
            let who = senator
                .as_ref()
                .map(ToString::to_string)
                .or_else(|| target_session.clone())
                .unwrap_or_else(|| "the session".into());
            format!("{verb} {who} on the secretary team")
        }
        Request::EnsureSecretary => "Start or restore the default secretary".into(),
        Request::Desk { set: Some(_) } => "Change the Speaker's desk workers".into(),
        Request::DeskDecide { committee, seq, .. } => {
            format!("Decide desk item {committee} #{seq}")
        }
        Request::Forward { committee, seq, .. } => {
            format!("Forward desk item {committee} #{seq}")
        }
        Request::Refuse { committee, seq, .. } => {
            format!("Refuse desk item {committee} #{seq}")
        }
        Request::DeskOpenCommittee { committee, .. } => {
            format!("Open committee {committee} and start its chair")
        }
        Request::SetChair {
            committee, senator, ..
        } => match committee {
            Some(committee) => format!("Appoint {senator} as chair of {committee}"),
            None => format!("Appoint {senator} as chair"),
        },
        Request::Spawn {
            committee,
            chair,
            secretary,
            ..
        } => {
            let role = match (*chair, *secretary) {
                (true, true) => "chair and secretary",
                (true, false) => "chair",
                (false, true) => "secretary",
                (false, false) => "senator",
            };
            format!("Spawn a {role} on {committee}")
        }
        Request::SpeakerAlias {
            committee,
            senator,
            alias,
            ..
        } => format!("Set alias {alias} for {senator} on {committee}"),
        Request::SpeakerUnseat {
            committee, senator, ..
        } => format!("Unseat {senator} from {committee}"),
        Request::ArchiveCommittee {
            committee,
            archived,
            ..
        } => {
            let action = if *archived { "Archive" } else { "Unarchive" };
            format!("{action} committee {committee}")
        }
        Request::RemoteControl { command } => match command {
            crate::remote::Control::PrepareEnable { listen } => {
                format!("Prepare remote machine access on {listen}")
            }
            crate::remote::Control::Pair { .. } => "Create a remote device pairing offer".into(),
            crate::remote::Control::PrepareEnroll => {
                "Prepare remote device enrollment for administrator approval".into()
            }
            _ => "Manage remote machine access".into(),
        },
        other => serde_json::to_value(other)
            .ok()
            .and_then(|value| value["op"].as_str().map(|op| op.replace('_', " ")))
            .unwrap_or_else(|| "Senate action".into()),
    };
    let action: String = action
        .chars()
        .take(110)
        .flat_map(char::escape_default)
        .collect();
    format!("{action} — requested by {requester}")
}

#[cfg(target_os = "macos")]
pub fn run_presence_helper() -> anyhow::Result<()> {
    macos::run_presence_helper()
}

#[cfg(target_os = "macos")]
#[derive(Serialize, Deserialize)]
struct AppMessage {
    home: std::path::PathBuf,
    command: AppCommand,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AppCommand {
    Poll,
    Respond { id: EventId, approved: bool },
    ApprovalTouch { required: bool },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AppReply {
    Prompts { prompts: Vec<Prompt> },
    Recorded { recorded: bool },
    WrongHome,
}

/// The name registered by the launchd service configuration for one home.
#[cfg(target_os = "macos")]
pub(crate) fn service_name(home: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};
    use std::os::unix::ffi::OsStrExt;

    let home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    let digest = Sha256::digest(home.as_os_str().as_bytes());
    format!(
        "dev.senate.senated.person-approval.{}",
        hex::encode(&digest[..12])
    )
}

#[cfg(target_os = "macos")]
pub(crate) fn team() -> Option<&'static str> {
    option_env!("SENATE_PERSON_TEAM_ID").filter(|team| {
        team.len() == 10
            && team
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    })
}

/// Poll the trusted local daemon for CLI approvals. A mismatched daemon, an
/// absent signed service, and an unavailable platform all yield no prompts.
pub fn pending(paths: &Paths) -> Vec<Prompt> {
    match call(paths, AppCommand::Poll) {
        Some(AppReply::Prompts { prompts }) => prompts,
        _ => Vec::new(),
    }
}

/// Record a decision on an immutable queued request, by its server-issued ID.
pub fn respond(paths: &Paths, id: EventId, approved: bool) -> bool {
    matches!(
        call(paths, AppCommand::Respond { id, approved }),
        Some(AppReply::Recorded { recorded: true })
    )
}

pub fn set_approval_touch(paths: &Paths, required: bool) -> bool {
    matches!(
        call(paths, AppCommand::ApprovalTouch { required }),
        Some(AppReply::Recorded { recorded: true })
    )
}

pub fn security_status(paths: &Paths) -> (bool, Option<String>) {
    #[cfg(target_os = "macos")]
    return (
        macos::approvals_require_touch(paths.home()),
        macos::change_notice(paths.home()),
    );
    #[cfg(not(target_os = "macos"))]
    {
        let _ = paths;
        (false, None)
    }
}

#[cfg(target_os = "macos")]
fn call(paths: &Paths, command: AppCommand) -> Option<AppReply> {
    let message = AppMessage {
        home: paths.home().canonicalize().ok()?,
        command,
    };
    macos::call(
        &service_name(&message.home),
        &serde_json::to_vec(&message).ok()?,
        &message.home,
        true,
    )
}

#[cfg(not(target_os = "macos"))]
fn call(_: &Paths, _: AppCommand) -> Option<AppReply> {
    None
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub(crate) mod macos {
    use super::*;
    use std::ffi::{CString, c_char, c_void};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::ptr::NonNull;
    use std::sync::Arc;

    struct Context {
        home: PathBuf,
        queue: Arc<Queue>,
    }

    unsafe extern "C" {
        fn senate_accessibility_trusted() -> bool;
        fn senate_device_owner_authenticated(reason: *const c_char) -> bool;
        fn senate_guest_team(pid: i32, text: *mut c_char, size: usize) -> bool;
        pub(crate) fn senate_xpc_listen(
            name: *const c_char,
            requirement: *const c_char,
            context: *mut c_void,
            drop_context: extern "C" fn(*mut c_void),
            handle: extern "C" fn(*mut c_void, *const u8, usize, *mut c_void, *const u32),
        ) -> *mut c_void;
        fn senate_xpc_stop(listener: *mut c_void);
        pub(crate) fn senate_xpc_send_reply(reply: *mut c_void, bytes: *const u8, len: usize);
        fn senate_xpc_call(
            name: *const c_char,
            requirement: *const c_char,
            request: *const u8,
            size: usize,
            reply_size: *mut usize,
            status: *mut i32,
        ) -> *mut u8;
        fn senate_guest_signing(
            pid: i32,
            identifier: *mut c_char,
            identifier_size: usize,
            team: *mut c_char,
            team_size: usize,
        );
        fn senate_code_hash_path(
            path: *const c_char,
            requirement: *const c_char,
            hash: *mut u8,
        ) -> bool;
        fn senate_code_hash_guest(
            audit: *const u32,
            pid: i32,
            requirement: *const c_char,
            hash: *mut u8,
        ) -> bool;
    }

    pub(crate) struct Listener(pub(crate) NonNull<c_void>);

    // libxpc permits cancellation and release from any queue.
    unsafe impl Send for Listener {}

    impl Drop for Listener {
        fn drop(&mut self) {
            // The handle is owned by this listener and is released exactly once.
            unsafe { senate_xpc_stop(self.0.as_ptr()) };
        }
    }

    pub(crate) fn requirement(identifier: &str) -> Option<CString> {
        let team = team()?;
        CString::new(format!(
            "anchor apple generic and identifier \"{identifier}\" and certificate leaf[subject.OU] = \"{team}\""
        )).ok()
    }

    const PIN_NAME: &str = "presence-code.json";

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub(crate) struct InstalledCode {
        app: Option<(PathBuf, [u8; 20])>,
        daemon: [u8; 20],
        #[serde(default)]
        approval_touch_id: bool,
    }

    fn installed(home: &Path) -> Option<InstalledCode> {
        serde_json::from_slice(&std::fs::read(home.join(PIN_NAME)).ok()?).ok()
    }

    fn accepted(home: &Path) -> Option<InstalledCode> {
        let database = Paths::new(home).state_db();
        let connection = rusqlite::Connection::open_with_flags(
            database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .ok()?;
        connection.busy_timeout(Duration::from_millis(100)).ok()?;
        let data: Vec<u8> = connection
            .query_row(
                "SELECT record FROM presence_identity WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .ok()?;
        serde_json::from_slice(&data).ok()
    }

    fn trusted_code(home: &Path) -> Option<InstalledCode> {
        accepted(home)
    }

    fn record_accepted(home: &Path, code: &InstalledCode) -> anyhow::Result<()> {
        let database = Paths::new(home).state_db();
        let connection = rusqlite::Connection::open(database)?;
        connection.busy_timeout(Duration::from_secs(2))?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS presence_identity(
                id INTEGER PRIMARY KEY CHECK(id = 1), record BLOB NOT NULL
            )",
        )?;
        connection.execute(
            "INSERT INTO presence_identity (id, record) VALUES (1, ?)
             ON CONFLICT(id) DO UPDATE SET record = excluded.record",
            [serde_json::to_vec(code)?],
        )?;
        Ok(())
    }

    fn write_install(home: &Path, code: &InstalledCode) -> anyhow::Result<()> {
        use std::io::Write as _;
        let mut file = tempfile::NamedTempFile::new_in(home)?;
        serde_json::to_writer(&mut file, code)?;
        file.flush()?;
        file.as_file().sync_all()?;
        file.persist(home.join(PIN_NAME))?;
        record_accepted(home, code)
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) struct PinChange {
        candidate: Option<InstalledCode>,
        prior: Option<InstalledCode>,
    }

    pub(crate) fn pending_change(home: &Path) -> Option<PinChange> {
        let candidate = installed(home);
        let prior = accepted(home);
        (candidate != prior || (candidate.is_none() && home.join(PIN_NAME).exists()))
            .then_some(PinChange { candidate, prior })
    }

    pub(crate) fn change_notice(home: &Path) -> Option<String> {
        pending_change(home)?;
        let observed = std::fs::metadata(home.join(PIN_NAME))
            .and_then(|metadata| metadata.modified())
            .map_or_else(
                |_| chrono::Utc::now(),
                chrono::DateTime::<chrono::Utc>::from,
            );
        Some(format!(
            "The trusted app or approval policy changed on {}. Confirm that you installed it.",
            observed.format("%Y-%m-%d")
        ))
    }

    pub(crate) fn accept_change(home: &Path, change: &PinChange) -> anyhow::Result<()> {
        use anyhow::Context as _;
        anyhow::ensure!(
            pending_change(home).as_ref() == Some(change),
            "Trust changed while waiting for authentication"
        );
        let code = change
            .candidate
            .as_ref()
            .or(change.prior.as_ref())
            .context("The installed code pin is missing. Reinstall the service.")?;
        let daemon_rule =
            requirement("dev.senate.daemon").context("missing daemon signing requirement")?;
        anyhow::ensure!(
            guest_hash(None, std::process::id(), &daemon_rule) == Some(code.daemon),
            "The daemon does not match the new pin. Reinstall the service."
        );
        if let Some((path, hash)) = &code.app {
            let app_rule =
                requirement("dev.senate.app").context("missing app signing requirement")?;
            anyhow::ensure!(
                hash_path(path, &app_rule) == Some(*hash),
                "The app does not match the new code pin"
            );
        }
        if change.candidate.is_none() {
            write_install(home, code)
        } else {
            record_accepted(home, code)
        }
    }

    pub(crate) fn approvals_require_touch(home: &Path) -> bool {
        pending_change(home).is_some()
            || trusted_code(home).is_some_and(|code| code.approval_touch_id)
    }

    fn hash_path(path: &Path, requirement: &std::ffi::CStr) -> Option<[u8; 20]> {
        let path = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut hash = [0; 20];
        unsafe { senate_code_hash_path(path.as_ptr(), requirement.as_ptr(), hash.as_mut_ptr()) }
            .then_some(hash)
    }

    pub(crate) fn guest_hash(
        audit: Option<&[u32; 8]>,
        pid: u32,
        requirement: &std::ffi::CStr,
    ) -> Option<[u8; 20]> {
        let mut hash = [0; 20];
        let audit = audit.map_or(std::ptr::null(), |token| token.as_ptr());
        let pid = i32::try_from(pid).ok()?;
        unsafe { senate_code_hash_guest(audit, pid, requirement.as_ptr(), hash.as_mut_ptr()) }
            .then_some(hash)
    }

    pub(crate) fn trusted_app(home: &Path, audit: &[u32; 8]) -> bool {
        let Some((_, expected)) = trusted_code(home).and_then(|pins| pins.app) else {
            return false;
        };
        requirement("dev.senate.app").is_some_and(|requirement| {
            guest_hash(Some(audit), audit[5], &requirement) == Some(expected)
        })
    }

    pub(crate) fn signing_team(pid: u32) -> Option<String> {
        let pid = i32::try_from(pid).ok()?;
        let mut team = [0; 32];
        unsafe { senate_guest_team(pid, team.as_mut_ptr(), team.len()) }.then(|| {
            unsafe { std::ffi::CStr::from_ptr(team.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        })
    }

    pub(crate) fn signed_daemon(pid: u32) -> bool {
        requirement("dev.senate.daemon").is_some_and(|rule| guest_hash(None, pid, &rule).is_some())
    }

    pub(crate) fn has_daemon_pin(home: &Path) -> bool {
        trusted_code(home).is_some()
    }

    pub(crate) fn pin_doctor(home: &Path) -> crate::doctor::Check {
        use crate::doctor::Check;

        const ID: &str = "person_presence.app";
        const FIX: &str = "Run senate setup service install --yes --app /path/to/Senate.app";
        if let Some(notice) = change_notice(home) {
            return Check::fail(ID, notice, Some(FIX));
        }
        let Some((path, expected)) = trusted_code(home).and_then(|pins| pins.app) else {
            return Check::warn(ID, "No Senate app code is pinned", Some(FIX));
        };
        let Some(requirement) = requirement("dev.senate.app") else {
            return Check::warn(ID, "This build has no Apple signing team", Some(FIX));
        };
        if hash_path(&path, &requirement) != Some(expected) {
            return Check::warn(
                ID,
                "The installed Senate app does not match the pin",
                Some(FIX),
            );
        }
        if !crate::kernel_process::running_pids()
            .into_iter()
            .any(|pid| {
                crate::kernel_process::executable(pid)
                    .is_ok_and(|program| program.starts_with(&path))
                    && guest_hash(None, pid, &requirement) == Some(expected)
            })
        {
            return Check::warn(
                ID,
                "The pinned Senate app is not running",
                Some("Open the installed Senate app"),
            );
        }
        Check::ok(ID, "The running Senate app matches its installed code pin")
    }

    pub(crate) fn accessibility_doctor() -> crate::doctor::Check {
        use crate::doctor::Check;
        const ID: &str = "person_presence.accessibility";
        const HELP: &str =
            "Check Accessibility and Automation for each terminal in System Settings";
        if unsafe { senate_accessibility_trusted() } {
            Check::warn(
                ID,
                "macOS trusts this invoking process for Accessibility",
                Some(HELP),
            )
        } else {
            Check::ok(
                ID,
                "macOS does not trust this invoking process for Accessibility. Other applications' grants were not checked.",
            )
        }
    }

    pub(crate) fn app_copies_doctor(home: &Path) -> Option<crate::doctor::Check> {
        use std::os::unix::ffi::OsStringExt as _;
        let (pinned, expected) = trusted_code(home)?.app?;
        let rule = requirement("dev.senate.app")?;
        let result = std::process::Command::new("/usr/bin/mdfind")
            .args(["-0", "kMDItemCFBundleIdentifier == 'dev.senate.app'"])
            .output()
            .ok()?;
        if !result.status.success() {
            return None;
        }
        let others: Vec<_> = result
            .stdout
            .split(|byte| *byte == 0)
            .filter(|bytes| !bytes.is_empty())
            .take(64)
            .map(|bytes| PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec())))
            .filter(|path| path != &pinned)
            .filter(|path| hash_path(path, &rule).is_some_and(|hash| hash != expected))
            .take(8)
            .map(|path| format!("{path:?}"))
            .collect();
        (!others.is_empty()).then(|| {
            crate::doctor::Check::warn(
                "person_presence.app_copies",
                format!(
                    "Spotlight found team-signed Senate apps with an unpinned code hash: {}",
                    others.join(", ")
                ),
                Some("Review the copies and open the pinned Senate app"),
            )
        })
    }

    pub(crate) fn trusted_helper(home: &Path, pid: u32) -> bool {
        let Some(pins) = trusted_code(home) else {
            return false;
        };
        requirement("dev.senate.daemon")
            .is_some_and(|requirement| guest_hash(None, pid, &requirement) == Some(pins.daemon))
    }

    pub(super) async fn authenticate_helper(home: &Path, reason: &str) -> bool {
        use std::process::Stdio;
        use tokio::io::AsyncWriteExt as _;

        let Ok(executable) = std::env::current_exe() else {
            return false;
        };
        let Ok(mut child) = tokio::process::Command::new(executable)
            .arg("--person-presence-helper")
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
        else {
            return false;
        };
        if !child.id().is_some_and(|pid| {
            if accepted(home).is_some() {
                return trusted_helper(home, pid);
            }
            let Some(rule) = requirement("dev.senate.daemon") else {
                return false;
            };
            let Some(candidate) = installed(home) else {
                return false;
            };
            guest_hash(None, std::process::id(), &rule) == Some(candidate.daemon)
                && guest_hash(None, pid, &rule) == Some(candidate.daemon)
        }) {
            return false;
        }
        let Some(mut input) = child.stdin.take() else {
            return false;
        };
        if input.write_all(reason.as_bytes()).await.is_err() {
            return false;
        }
        drop(input);
        child.wait().await.is_ok_and(|status| status.success())
    }

    pub(super) fn run_presence_helper() -> anyhow::Result<()> {
        use std::io::Read as _;

        let requirement = requirement("dev.senate.daemon")
            .ok_or_else(|| anyhow::anyhow!("Missing daemon signing requirement"))?;
        let own_signature = guest_hash(None, std::process::id(), &requirement);
        let parent = std::os::unix::process::parent_id();
        anyhow::ensure!(
            own_signature.is_some() && guest_hash(None, parent, &requirement) == own_signature,
            "The presence helper requires its signed daemon parent"
        );
        let mut reason = Vec::new();
        std::io::stdin().take(4097).read_to_end(&mut reason)?;
        anyhow::ensure!(
            (1..=4096).contains(&reason.len()),
            "Invalid authentication request"
        );
        let reason = CString::new(reason)?;
        anyhow::ensure!(
            unsafe { senate_device_owner_authenticated(reason.as_ptr()) },
            "Authentication declined or unavailable"
        );
        Ok(())
    }

    pub(crate) fn install_pins(
        home: &Path,
        app: Option<&Path>,
        daemon: &Path,
    ) -> anyhow::Result<()> {
        use anyhow::Context as _;

        if team().is_none() {
            anyhow::ensure!(app.is_none(), "This build has no configured signing team");
            return Ok(());
        }
        let daemon_rule =
            requirement("dev.senate.daemon").context("Invalid daemon signing requirement")?;
        let daemon = hash_path(daemon, &daemon_rule)
            .context("senated does not have the required code identity")?;
        let app = app
            .map(|path| -> anyhow::Result<_> {
                let path = path
                    .canonicalize()
                    .context("locating the installed Senate app")?;
                let app_rule =
                    requirement("dev.senate.app").context("Invalid app signing requirement")?;
                let hash = hash_path(&path, &app_rule)
                    .context("The installed Senate app does not have the required code identity")?;
                Ok((path, hash))
            })
            .transpose()?;
        let approval_touch_id = accepted(home).is_some_and(|code| code.approval_touch_id);
        write_install(
            home,
            &InstalledCode {
                app,
                daemon,
                approval_touch_id,
            },
        )
    }

    pub(crate) fn listen(paths: &Paths, queue: Arc<Queue>) -> Option<Listener> {
        let name = CString::new(service_name(paths.home())).ok()?;
        let requirement = requirement("dev.senate.app")?;
        let context = Box::new(Context {
            home: paths.home().canonicalize().ok()?,
            queue,
        });
        // C takes ownership of the context on all paths. The two C strings
        // are live until the setup call returns.
        let listener = unsafe {
            senate_xpc_listen(
                name.as_ptr(),
                requirement.as_ptr(),
                Box::into_raw(context).cast(),
                drop_context,
                handle,
            )
        };
        NonNull::new(listener).map(Listener)
    }

    extern "C" fn drop_context(context: *mut c_void) {
        // C invokes this finalizer once, after all peer callbacks finish.
        drop(unsafe { Box::from_raw(context.cast::<Context>()) });
    }

    extern "C" fn handle(
        context: *mut c_void,
        bytes: *const u8,
        len: usize,
        reply: *mut c_void,
        audit: *const u32,
    ) {
        // libxpc owns the input and reply objects for the duration of this
        // callback. The listener's context outlives its peer connections.
        let context = unsafe { &*context.cast::<Context>() };
        let bytes = unsafe { std::slice::from_raw_parts(bytes, len) };
        let Ok(message) = serde_json::from_slice::<AppMessage>(bytes) else {
            return;
        };
        let audit = unsafe { &*audit.cast::<[u32; 8]>() };
        let response = if message.home != context.home || !trusted_app(&context.home, audit) {
            AppReply::WrongHome
        } else {
            match message.command {
                AppCommand::Poll => AppReply::Prompts {
                    prompts: context.queue.poll(),
                },
                AppCommand::Respond { id, approved } => AppReply::Recorded {
                    recorded: context.queue.respond(&id, approved),
                },
                AppCommand::ApprovalTouch { required } => AppReply::Recorded {
                    recorded: update_approval_touch(&context.home, required).is_ok(),
                },
            }
        };
        if let Ok(data) = serde_json::to_vec(&response) {
            // xpc_dictionary_set_data copies these bytes into the reply.
            unsafe { senate_xpc_send_reply(reply, data.as_ptr(), data.len()) };
        }
    }

    fn update_approval_touch(home: &Path, required: bool) -> anyhow::Result<()> {
        use anyhow::Context as _;
        anyhow::ensure!(
            pending_change(home).is_none(),
            "The installed pin or approval policy changed"
        );
        let mut code = accepted(home).context("No approved app policy")?;
        code.approval_touch_id = required;
        write_install(home, &code)
    }

    pub(crate) fn call<T: serde::de::DeserializeOwned>(
        service: &str,
        message: &[u8],
        home: &Path,
        require_pin: bool,
    ) -> Option<T> {
        serde_json::from_slice(&exchange(service, message, home, require_pin).ok()?).ok()
    }

    /// Why the daemon's Mach service gave no reply.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum CallFailure {
        /// This binary was built without a signing team.
        NoTeam,
        /// The pin is required and none is recorded.
        NoPin,
        /// An oversized message, an allocation failure, or a bad requirement.
        Local,
        /// No such Mach service, or the connection was invalidated.
        Lookup,
        /// The service's code did not satisfy the daemon requirement and pin.
        Requirement,
        TimedOut,
        Interrupted,
        Malformed,
    }

    /// One request and its raw reply over the daemon's Mach service. The
    /// service must satisfy `dev.senate.daemon` with this build's team and,
    /// when one is recorded, the installed daemon pin.
    pub(crate) fn exchange(
        service: &str,
        message: &[u8],
        home: &Path,
        require_pin: bool,
    ) -> Result<Vec<u8>, CallFailure> {
        let name = CString::new(service).map_err(|_| CallFailure::Local)?;
        let mut requirement = requirement("dev.senate.daemon").ok_or(CallFailure::NoTeam)?;
        if let Some(pin) = trusted_code(home) {
            requirement = CString::new(format!(
                "{} and cdhash H\"{}\"",
                requirement.to_str().map_err(|_| CallFailure::Local)?,
                hex::encode(pin.daemon)
            ))
            .map_err(|_| CallFailure::Local)?;
        } else if require_pin {
            return Err(CallFailure::NoPin);
        }
        let mut size = 0;
        let mut status = 0;
        // The arguments remain live until the synchronous XPC call returns.
        let data = unsafe {
            senate_xpc_call(
                name.as_ptr(),
                requirement.as_ptr(),
                message.as_ptr(),
                message.len(),
                &mut size,
                &mut status,
            )
        };
        let Some(data) = NonNull::new(data) else {
            return Err(match status {
                2 => CallFailure::Lookup,
                3 => CallFailure::Requirement,
                4 => CallFailure::TimedOut,
                5 => CallFailure::Interrupted,
                6 => CallFailure::Malformed,
                _ => CallFailure::Local,
            });
        };
        // C allocated the response with malloc, and `size` is its length.
        let bytes = unsafe { std::slice::from_raw_parts(data.as_ptr(), size) }.to_vec();
        unsafe { libc::free(data.as_ptr().cast()) };
        Ok(bytes)
    }

    /// What a running process presents as its signing identifier and team,
    /// valid or not, for diagnostics.
    pub(crate) fn signing_of(pid: u32) -> (String, String) {
        let Ok(pid) = i32::try_from(pid) else {
            return Default::default();
        };
        let mut identifier = [0 as c_char; 256];
        let mut team = [0 as c_char; 32];
        // Both buffers are writable for their full sizes and NUL-terminated.
        unsafe {
            senate_guest_signing(
                pid,
                identifier.as_mut_ptr(),
                identifier.len(),
                team.as_mut_ptr(),
                team.len(),
            );
        }
        let text = |buffer: &[c_char]| {
            unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        };
        (text(&identifier), text(&team))
    }

    #[cfg(test)]
    #[test]
    fn apple_signature_and_code_directory_hash_are_independent_requirements() {
        let app = Path::new("/System/Applications/Utilities/Terminal.app");
        let base = r#"anchor apple and identifier "com.apple.Terminal""#;
        let rule = CString::new(base).unwrap();
        let actual = hash_path(app, &rule).expect("macOS Terminal has a valid platform signature");
        let rule = CString::new(format!("{base} and cdhash H\"{}\"", hex::encode(actual))).unwrap();
        assert_eq!(hash_path(app, &rule), Some(actual));
        let rule =
            CString::new(format!("{base} and cdhash H\"{}\"", hex::encode([0; 20]))).unwrap();
        assert_eq!(hash_path(app, &rule), None);
    }

    #[cfg(test)]
    #[test]
    fn a_missing_or_reset_preference_does_not_silently_clear_touch_policy() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let approved = InstalledCode {
            app: None,
            daemon: [37; 20],
            approval_touch_id: true,
        };
        write_install(dir.path(), &approved)?;
        assert!(pending_change(dir.path()).is_none());
        assert!(approvals_require_touch(dir.path()));
        std::fs::remove_file(dir.path().join(PIN_NAME))?;
        assert!(pending_change(dir.path()).is_some());
        assert!(approvals_require_touch(dir.path()));
        assert_eq!(accepted(dir.path()), Some(approved));
        Ok(())
    }
}

/// Whether a bridge request may wait on local authorization.
pub fn required(request: &crate::ipc::Request) -> bool {
    use crate::daemon::approval_guard::{RequestEffect, request_effect};

    request_effect(request) == RequestEffect::Presence
}

/// The unsigned, disposable integration fixtures exercise the existing
/// operational handlers directly. Keep prior signed-consent tests gated.
pub(crate) fn fixture_requires_confirmation(request: &crate::ipc::Request) -> bool {
    use crate::ipc::Request;
    matches!(
        request,
        Request::ObserveAdmin {
            command: senate_observe::AdminRequest::Create { grant },
        } if grant.scope == senate_observe::Scope::Lines
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::Request;
    use senate_observe::{AdminRequest, LinePolicy};

    #[test]
    fn sensitive_requests_do_not_accept_client_assertions_of_presence() {
        let request: Request = serde_json::from_value(serde_json::json!({
            "op": "approve",
            "id": "fixture",
            "decision": "allow",
            "authenticated": true
        }))
        .unwrap();
        assert!(required(&request));
        assert!(required(&Request::EnsureSecretary));
        assert!(required(&Request::ObserveAdmin {
            command: AdminRequest::Publish {
                committee: "senate".parse().unwrap(),
                seq: senate_core::Seq(1),
                reviewed: senate_observe::Preview {
                    committee: "senate".parse().unwrap(),
                    event_id: EventId::new(),
                    line: "preview".into(),
                },
            },
        }));
        assert!(!required(&Request::ObserveAdmin {
            command: AdminRequest::Revoke {
                id: "fixture".into()
            },
        }));
        assert!(!required(&Request::ObserveAdmin {
            command: AdminRequest::Withdraw {
                committee: "senate".parse().unwrap(),
                seq: senate_core::Seq(1),
            },
        }));
        assert!(required(&Request::DeskWorker {
            command: senate_core::worker::Admin::Rotate { id: EventId::new() },
        }));
        assert!(!required(&Request::DeskWorker {
            command: senate_core::worker::Admin::Revoke { id: EventId::new() },
        }));
        assert!(required(&Request::RemoteControl {
            command: crate::remote::Control::PrepareEnroll,
        }));
        assert!(!required(&Request::ObserveAdmin {
            command: AdminRequest::List
        }));
        assert!(!required(&Request::RemoteControl {
            command: crate::remote::Control::Devices
        }));
        assert!(!required(&Request::SetCard {
            session: crate::ipc::SessionSelector::default(),
            target: crate::ipc::CardTarget::Own,
            document: "{}".into(),
        }));
        assert!(!required(&Request::SetCard {
            session: crate::ipc::SessionSelector {
                session_id: Some("session".into()),
                claude_pid: None,
            },
            target: crate::ipc::CardTarget::Own,
            document: "{}".into(),
        }));
        assert!(!required(&Request::Status));
        assert!(!required(&Request::Sessions));
    }

    #[test]
    fn caller_fields_cannot_authorize_either_line_policy() {
        for policy in [LinePolicy::Auto, LinePolicy::Published] {
            let value = serde_json::json!({
                "op": "observe_admin",
                "command": {
                    "action": "create",
                    "grant": {
                        "name": "test",
                        "scope": "lines",
                        "lines": policy,
                        "all_committees": false,
                        "origins": [],
                        "topology": false,
                        "auto_confirmed": true,
                    },
                },
            });
            let request = serde_json::from_value::<Request>(value).unwrap();
            assert!(required(&request));
            let Request::ObserveAdmin {
                command: AdminRequest::Create { grant },
            } = request
            else {
                panic!("expected an Observe grant request");
            };
            assert!(
                serde_json::to_value(grant)
                    .unwrap()
                    .get("auto_confirmed")
                    .is_none()
            );
        }
    }
}
