//! macOS person proofs bound to the request and to its Mach message sender.
#![allow(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, c_void};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::app_confirmation::{self, macos};
use crate::kernel_process::{self, ProcessIdentity};

const TTL: Duration = Duration::from_secs(5);
const LIMIT: usize = 256;

#[derive(Default)]
pub(crate) struct Scope {
    home: PathBuf,
    hosted: HashSet<u64>,
    receipts: Mutex<HashMap<String, Receipt>>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProofKind {
    #[default]
    Operator,
    Session,
    App,
    RemoteOperator,
}

struct Receipt {
    request: [u8; 32],
    sender: ProcessIdentity,
    kind: ProofKind,
    app_audit: Option<[u32; 8]>,
    expires: Instant,
}

#[derive(Serialize, Deserialize)]
struct Claim {
    #[serde(default)]
    kind: ProofKind,
    request: [u8; 32],
}

pub(crate) fn service(home: &Path) -> String {
    format!("{}.scope", app_confirmation::service_name(home))
}

unsafe extern "C" {
    fn responsibility_get_pid_responsible_for_pid(pid: i32) -> i32;
    fn __proc_info_extended_id(
        call: i32,
        pid: i32,
        flavor: u32,
        flags: u32,
        extended_id: u64,
        arg: u64,
        buffer: usize,
        size: i32,
    ) -> i32;
}

/// Why the daemon issued no proof. Sent back to the requesting process (the
/// same user) and logged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub(crate) enum Refusal {
    /// The claim was not a proof request this daemon understands.
    BadClaim,
    /// The message came from another user or from launchd.
    WrongUser,
    /// The kernel could not describe the requesting process or its parents.
    ProcessUnreadable {
        detail: String,
    },
    CoalitionUnreadable,
    /// The requester runs inside a Senate-hosted agent's coalition.
    HostedAgent,
    /// No known terminal app, SSH login or pinned app stands behind it.
    NoTrustedOrigin {
        detail: String,
    },
    /// Only the pinned Senate app may ask for an app proof.
    NotTheApp,
    /// A process changed while it was being checked.
    ProcessChanged,
    Busy,
}

impl Refusal {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::BadClaim => "bad_claim",
            Self::WrongUser => "wrong_user",
            Self::ProcessUnreadable { .. } => "process_unreadable",
            Self::CoalitionUnreadable => "coalition_unreadable",
            Self::HostedAgent => "hosted_agent",
            Self::NoTrustedOrigin { .. } => "no_trusted_origin",
            Self::NotTheApp => "not_the_app",
            Self::ProcessChanged => "process_changed",
            Self::Busy => "busy",
        }
    }
}

/// The daemon's reply: a ticket as a bare string (what 0.13.0 clients
/// decode), or why there is none.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Answer {
    Ticket(String),
    Refused { refused: Refusal },
}

const TERMINALS: &std::ffi::CStr = c"((anchor apple and identifier \"com.apple.Terminal\") or (anchor apple generic and identifier \"com.mitchellh.ghostty\" and certificate leaf[subject.OU] = \"24VZTF6M5V\") or (anchor apple generic and identifier \"com.googlecode.iterm2\" and certificate leaf[subject.OU] = \"H7V7XYVQ7D\"))";

fn is_sshd(pid: u32) -> bool {
    kernel_process::executable(pid).is_ok_and(|path| {
        matches!(
            path.to_str(),
            Some("/usr/sbin/sshd" | "/usr/libexec/sshd-session")
        )
    })
}

/// Whether an SSH login stands behind `pid`: sshd among its own ancestors,
/// as the first process above them (a root sshd ends the user's readable
/// chain), or as the process macOS holds responsible for it. `None` when
/// the chain cannot be read.
pub(crate) fn behind_ssh(pid: u32) -> Option<bool> {
    let ancestry = kernel_process::ancestors(pid, 64).ok()?;
    Some(ssh_in(pid, &ancestry, responsible_for(pid)))
}

fn ssh_in(pid: u32, ancestry: &[ProcessIdentity], responsible: Option<u32>) -> bool {
    let top = ancestry.last().map_or(pid, |process| process.pid);
    let above = kernel_process::inspect(top)
        .ok()
        .map(|process| process.parent_pid)
        .filter(|parent| *parent > 1);
    ancestry
        .iter()
        .map(|process| process.pid)
        .chain(above)
        .chain(responsible)
        .any(is_sshd)
}

fn responsible_for(pid: u32) -> Option<u32> {
    let pid = i32::try_from(pid).ok()?;
    // A plain query of the kernel's responsibility record for a PID.
    u32::try_from(unsafe { responsibility_get_pid_responsible_for_pid(pid) })
        .ok()
        .filter(|pid| *pid > 0)
}

/// What a person reads about a process: its path, signing identifier and
/// team, as far as they can be read.
fn describe(pid: u32) -> String {
    let path = kernel_process::executable(pid).map_or_else(
        |_| "an unreadable executable".to_owned(),
        |path| path.display().to_string(),
    );
    let (identifier, team) = macos::signing_of(pid);
    let or_none = |text: String| {
        if text.is_empty() {
            "none".to_owned()
        } else {
            text
        }
    };
    format!(
        "{path} (pid {pid}, identifier {}, team {})",
        or_none(identifier),
        or_none(team)
    )
}

fn trusted_origin(pid: u32) -> Result<ProofKind, Refusal> {
    let ancestry =
        kernel_process::ancestors(pid, 64).map_err(|error| Refusal::ProcessUnreadable {
            detail: format!("the requesting process's parents could not be read: {error}"),
        })?;
    let responsible = responsible_for(pid);
    if ssh_in(pid, &ancestry, responsible) {
        return Ok(ProofKind::RemoteOperator);
    }
    let responsible = responsible.ok_or_else(|| Refusal::NoTrustedOrigin {
        detail: format!("macOS names no process responsible for pid {pid}"),
    })?;
    let owner = kernel_process::inspect(responsible)
        .map_err(|error| Refusal::NoTrustedOrigin {
            detail: format!(
                "the process macOS holds responsible for this shell (pid {responsible}) is gone or unreadable ({error}); \
                 a tmux or screen server can outlive the terminal app that started it — start a new one from your terminal"
            ),
        })?
        .identity;
    if macos::guest_hash(None, responsible, TERMINALS).is_none() {
        return Err(Refusal::NoTrustedOrigin {
            detail: format!(
                "the app responsible for this shell is {}, not Terminal, Ghostty or iTerm2 as signed by their developers",
                describe(responsible)
            ),
        });
    }
    if kernel_process::inspect(responsible)
        .ok()
        .map(|process| process.identity)
        != Some(owner)
    {
        return Err(Refusal::ProcessChanged);
    }
    Ok(ProofKind::Operator)
}

fn coalition(pid: i32, version: Option<u32>) -> std::io::Result<u64> {
    // XNU's PROC_PIDCOALITIONINFO has two IDs and three reserved words.
    let mut ids = [0_u64; 5];
    let size = std::mem::size_of_val(&ids) as i32;
    // The ABI takes a writable buffer of exactly this size. Comparing the
    // audit token's pidversion inside XNU makes PID lookup and validation atomic.
    let read = unsafe {
        match version {
            Some(version) => __proc_info_extended_id(
                2,
                pid,
                20,
                1,
                (version as i32) as u64,
                0,
                ids.as_mut_ptr() as usize,
                size,
            ),
            None => libc::proc_pidinfo(pid, 20, 0, ids.as_mut_ptr().cast(), size),
        }
    };
    if read != size || ids[0] <= 1 {
        return Err(std::io::Error::other("resource coalition unavailable"));
    }
    Ok(ids[0])
}

pub(crate) fn boot_id() -> std::io::Result<String> {
    let mut bytes = [0_u8; 64];
    let mut len = bytes.len();
    // sysctl updates len and writes at most the supplied buffer size.
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            bytes.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let text = CStr::from_bytes_until_nul(bytes.get(..len).unwrap_or_default())
        .ok()
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| std::io::Error::other("invalid kernel boot identity"))?;
    Ok(text.to_owned())
}

impl Scope {
    pub(crate) fn contains_process(&self, pid: u32) -> bool {
        i32::try_from(pid)
            .ok()
            .and_then(|pid| coalition(pid, None).ok())
            .is_some_and(|scope| self.hosted.contains(&scope))
    }

    /// A launchd job has a separate kernel resource coalition. The managed
    /// profile uses that coalition for its direct host processes.
    pub(crate) fn start(home: &Path) -> anyhow::Result<(Arc<Self>, macos::Listener)> {
        use std::io::Write;
        anyhow::ensure!(
            std::os::unix::process::parent_id() == 1,
            "the daemon must run as a launchd job"
        );
        let boot = boot_id()?;
        let path = home.join("host-coalitions.json");
        let mut hosted = match std::fs::read(&path) {
            Ok(bytes) => {
                let (prior_boot, prior): (String, HashSet<u64>) = serde_json::from_slice(&bytes)?;
                if prior_boot == boot {
                    prior
                } else {
                    HashSet::new()
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashSet::new(),
            Err(error) => return Err(error.into()),
        };
        hosted.insert(coalition(i32::try_from(std::process::id())?, None)?);
        let mut pending = tempfile::NamedTempFile::new_in(home)?;
        serde_json::to_writer(&mut pending, &(boot, &hosted))?;
        pending.flush()?;
        pending.as_file().sync_all()?;
        pending.persist(&path).map_err(|error| error.error)?;

        let name = CString::new(service(home))?;
        let scope = Arc::new(Self {
            home: home.to_owned(),
            hosted,
            receipts: Mutex::default(),
        });
        // The listener takes ownership of the boxed Arc. Any same-user process
        // may ask for proof; the kernel audit token determines eligibility.
        let listener = unsafe {
            macos::senate_xpc_listen(
                name.as_ptr(),
                std::ptr::null(),
                Box::into_raw(Box::new(scope.clone())).cast(),
                drop_scope,
                handle,
            )
        };
        let listener = NonNull::new(listener)
            .ok_or_else(|| anyhow::anyhow!("cannot open the person scope service"))?;
        tracing::debug!(service = %service(home), "registered macOS scope listener");
        Ok((scope, macos::Listener(listener)))
    }

    fn issue(
        &self,
        token: [u32; 8],
        request: [u8; 32],
        kind: ProofKind,
    ) -> Result<String, Refusal> {
        if token[1] != rustix::process::geteuid().as_raw() {
            return Err(Refusal::WrongUser);
        }
        let pid = i32::try_from(token[5])
            .ok()
            .filter(|pid| *pid > 1)
            .ok_or(Refusal::WrongUser)?;
        let unreadable = |error: std::io::Error| Refusal::ProcessUnreadable {
            detail: format!("pid {pid}: {error}"),
        };
        let sender = kernel_process::inspect(pid.unsigned_abs())
            .map_err(unreadable)?
            .identity;
        let scope = coalition(pid, Some(token[7])).map_err(|_| Refusal::CoalitionUnreadable)?;
        if kind != ProofKind::Session && self.hosted.contains(&scope) {
            return Err(Refusal::HostedAgent);
        }
        let kind = match kind {
            ProofKind::Operator | ProofKind::App if macos::trusted_app(&self.home, &token) => {
                ProofKind::App
            }
            ProofKind::Operator => trusted_origin(sender.pid)?,
            ProofKind::App | ProofKind::RemoteOperator => return Err(Refusal::NotTheApp),
            ProofKind::Session => ProofKind::Session,
        };
        if kernel_process::inspect(sender.pid)
            .map_err(unreadable)?
            .identity
            != sender
        {
            return Err(Refusal::ProcessChanged);
        }
        let app_audit = (kind == ProofKind::App).then_some(token);
        let mut receipts = self.receipts.lock().map_err(|_| Refusal::Busy)?;
        let now = Instant::now();
        receipts.retain(|_, proof| proof.expires > now);
        if receipts.len() >= LIMIT {
            return Err(Refusal::Busy);
        }
        let mut token = [0_u8; 32];
        getrandom::fill(&mut token).map_err(|_| Refusal::Busy)?;
        let token = hex::encode(token);
        receipts.insert(
            token.clone(),
            Receipt {
                request,
                sender,
                kind,
                app_audit,
                expires: now + TTL,
            },
        );
        tracing::debug!(pid, scope, "issued one-time person scope proof");
        Ok(token)
    }

    pub(crate) fn consume(
        &self,
        token: &str,
        request: &[u8],
    ) -> Option<(ProcessIdentity, ProofKind)> {
        let proof = self.receipts.lock().ok()?.remove(token)?;
        let digest: [u8; 32] = Sha256::digest(request).into();
        if proof.expires <= Instant::now() || proof.request != digest {
            return None;
        }
        if proof
            .app_audit
            .is_some_and(|audit| !macos::trusted_app(&self.home, &audit))
        {
            return None;
        }
        let current = kernel_process::inspect(proof.sender.pid).ok()?;
        (current.identity == proof.sender).then_some((proof.sender, proof.kind))
    }
}

extern "C" fn drop_scope(context: *mut c_void) {
    // Paired with Box::into_raw in start. The listener owns this allocation.
    drop(unsafe { Box::from_raw(context.cast::<Arc<Scope>>()) });
}

extern "C" fn handle(
    context: *mut c_void,
    bytes: *const u8,
    len: usize,
    reply: *mut c_void,
    audit: *const u32,
) {
    // XPC supplies all pointers for the duration of this synchronous callback.
    let scope = unsafe { &*context.cast::<Arc<Scope>>() };
    let bytes = unsafe { std::slice::from_raw_parts(bytes, len) };
    let audit = unsafe { *audit.cast::<[u32; 8]>() };
    tracing::debug!("received scope proof request");
    let (kind, answer) = match serde_json::from_slice::<Claim>(bytes) {
        Ok(claim) => (
            Some(claim.kind),
            scope.issue(audit, claim.request, claim.kind),
        ),
        Err(_) => (None, Err(Refusal::BadClaim)),
    };
    let answer = match answer {
        Ok(ticket) => Answer::Ticket(ticket),
        Err(refused) => {
            let detail = match &refused {
                Refusal::ProcessUnreadable { detail } | Refusal::NoTrustedOrigin { detail } => {
                    detail.as_str()
                }
                _ => "",
            };
            tracing::warn!(
                pid = audit[5],
                ?kind,
                stage = refused.code(),
                detail,
                "refused a caller proof"
            );
            Answer::Refused { refused }
        }
    };
    if let Ok(bytes) = serde_json::to_vec(&answer) {
        unsafe { macos::senate_xpc_send_reply(reply, bytes.as_ptr(), bytes.len()) };
    }
}

/// Why a request went without a proof of its caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProofFailure {
    /// The socket path has no parent directory to name the service by.
    NoHome,
    Call(macos::CallFailure),
    TimedOut,
    /// The daemon answered, and declined.
    Refused(Refusal),
    /// The daemon declined without a reason (0.13.0 answers `null`).
    Unexplained,
}

impl ProofFailure {
    /// One sentence for the person, naming the stage.
    pub(crate) fn sentence(&self, service: &str) -> String {
        use macos::CallFailure;
        let why = match self {
            Self::NoHome => "the daemon socket has no home directory".to_owned(),
            Self::Call(CallFailure::NoTeam) => {
                "this senate was built without a signing team, so it cannot prove where it runs"
                    .to_owned()
            }
            Self::Call(CallFailure::Lookup) => format!(
                "the daemon's proof service {service} was not found: the signed daemon is not running as the launchd service for this home"
            ),
            Self::Call(CallFailure::Requirement | CallFailure::NoPin) => {
                "the running daemon's code did not satisfy this build's signing requirement and the installed daemon pin; reinstall the service from your terminal"
                    .to_owned()
            }
            Self::Call(CallFailure::TimedOut) | Self::TimedOut => {
                "the daemon's proof service did not answer in time".to_owned()
            }
            Self::Call(failure) => format!("the daemon's proof service failed ({failure:?})"),
            Self::Refused(Refusal::HostedAgent) => {
                "this command runs inside a Senate-hosted agent session, and an agent cannot act as the person: ask the person to run it from their own terminal or the Senate app"
                    .to_owned()
            }
            Self::Refused(Refusal::NoTrustedOrigin { detail }) => {
                format!("your terminal could not be verified: {detail}")
            }
            Self::Refused(Refusal::ProcessUnreadable { detail }) => {
                format!("the daemon could not read this process: {detail}")
            }
            Self::Refused(refused) => format!("the daemon refused the proof ({})", refused.code()),
            Self::Unexplained => {
                "the daemon refused the proof without a reason; it may be an older daemon".to_owned()
            }
        };
        format!("could not prove which process sent this request: {why}")
    }
}

/// Ask the daemon for a one-time proof binding `request` to this process.
pub(crate) async fn prove(
    path: &Path,
    request: &[u8],
    kind: ProofKind,
) -> Result<String, (ProofFailure, String)> {
    let home = path
        .parent()
        .ok_or((ProofFailure::NoHome, String::new()))?
        .to_owned();
    let service = service(&home);
    let named = service.clone();
    let bytes = serde_json::to_vec(&Claim {
        kind,
        request: Sha256::digest(request).into(),
    })
    .map_err(|_| (ProofFailure::Unexplained, named.clone()))?;
    let reply = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::task::spawn_blocking(move || macos::exchange(&service, &bytes, &home, false)),
    )
    .await
    .map_err(|_| (ProofFailure::TimedOut, named.clone()))?
    .map_err(|_| (ProofFailure::TimedOut, named.clone()))?
    .map_err(|failure| (ProofFailure::Call(failure), named.clone()))?;
    match serde_json::from_slice::<Option<Answer>>(&reply) {
        Ok(Some(Answer::Ticket(ticket))) => Ok(ticket),
        Ok(Some(Answer::Refused { refused })) => Err((ProofFailure::Refused(refused), named)),
        Ok(None) | Err(_) => Err((ProofFailure::Unexplained, named)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(deprecated)]
    fn kernel_generation_and_hosted_coalition_are_independent_checks() {
        let mut audit = [0_u32; 8];
        let mut words = audit.len() as u32;
        // TASK_AUDIT_TOKEN returns eight words for our current task.
        let status = unsafe {
            libc::task_info(
                libc::mach_task_self(),
                15,
                audit.as_mut_ptr().cast(),
                &mut words,
            )
        };
        assert_eq!(status, libc::KERN_SUCCESS);
        assert_eq!(words, 8);
        let pid = i32::try_from(audit[5]).unwrap();
        let own_coalition = coalition(pid, Some(audit[7])).unwrap();
        let request = Sha256::digest(b"fixture").into();
        let eligible = Scope::default();
        assert!(eligible.issue(audit, request, ProofKind::Session).is_ok());
        assert_eq!(
            eligible.issue(audit, request, ProofKind::App),
            Err(Refusal::NotTheApp)
        );

        let hosted = Scope {
            hosted: HashSet::from([own_coalition]),
            ..Scope::default()
        };
        assert_eq!(
            hosted.issue(audit, request, ProofKind::Operator),
            Err(Refusal::HostedAgent)
        );

        audit[7] ^= 1;
        assert_eq!(
            eligible.issue(audit, request, ProofKind::Session),
            Err(Refusal::CoalitionUnreadable)
        );
    }

    /// A terminal app starts its shells under the root-owned
    /// `/usr/bin/login`, whose details the kernel withholds from the user.
    /// The user's chain ends there instead of failing the whole check, so
    /// a shell in Ghostty or Terminal can reach its terminal app.
    #[test]
    fn a_root_login_above_the_shell_ends_the_chain() {
        let login = |pid: u32| {
            kernel_process::executable(pid).is_ok_and(|path| path == Path::new("/usr/bin/login"))
                && kernel_process::inspect(pid).is_err_and(|error| kernel_process::foreign(&error))
        };
        // Find a shell of this user whose parent is a root `login`.
        let Some(shell) = (2..100_000_u32).find(|pid| {
            kernel_process::inspect(*pid).is_ok_and(|process| login(process.parent_pid))
        }) else {
            eprintln!("no terminal shell under /usr/bin/login on this machine; skipped");
            return;
        };
        let chain = kernel_process::ancestors(shell, 64).expect("readable up to login");
        assert!(chain.is_empty(), "the chain stops at the root login");
        assert_eq!(behind_ssh(shell), Some(false));
    }

    #[test]
    fn answers_keep_the_bare_ticket_older_clients_decode() {
        let ticket = serde_json::to_vec(&Answer::Ticket("abc".into())).unwrap();
        assert_eq!(
            serde_json::from_slice::<Option<String>>(&ticket).unwrap(),
            Some("abc".into())
        );
        let refused = serde_json::to_vec(&Answer::Refused {
            refused: Refusal::NoTrustedOrigin {
                detail: "why".into(),
            },
        })
        .unwrap();
        assert!(serde_json::from_slice::<Option<String>>(&refused).is_err());
        assert!(matches!(
            serde_json::from_slice::<Option<Answer>>(&refused).unwrap(),
            Some(Answer::Refused {
                refused: Refusal::NoTrustedOrigin { .. }
            })
        ));
        assert!(
            serde_json::from_slice::<Option<Answer>>(b"null")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn proof_matches_one_exact_request_and_is_consumed() {
        let scope = Scope::default();
        let sender = kernel_process::inspect(std::process::id())
            .unwrap()
            .identity;
        let seal = |key: &str| {
            scope.receipts.lock().unwrap().insert(
                key.into(),
                Receipt {
                    request: Sha256::digest(b"first").into(),
                    sender,
                    kind: ProofKind::Operator,
                    app_audit: None,
                    expires: Instant::now() + TTL,
                },
            );
        };
        seal("first");
        assert_eq!(
            scope.consume("first", b"first"),
            Some((sender, ProofKind::Operator))
        );
        assert_eq!(scope.consume("first", b"first"), None);

        seal("second");
        assert_eq!(scope.consume("second", b"changed"), None);
        assert_eq!(scope.consume("second", b"first"), None);
    }
}
