//! macOS person proofs bound to the request and to its Mach message sender.
#![allow(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, c_void};
use std::path::Path;
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
    hosted: HashSet<u64>,
    receipts: Mutex<HashMap<String, Receipt>>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProofKind {
    #[default]
    Operator,
    Session,
}

struct Receipt {
    request: [u8; 32],
    sender: ProcessIdentity,
    kind: ProofKind,
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

    fn issue(&self, token: [u32; 8], request: [u8; 32], kind: ProofKind) -> Option<String> {
        if token[1] != rustix::process::geteuid().as_raw() {
            return None;
        }
        let pid = i32::try_from(token[5]).ok().filter(|pid| *pid > 1)?;
        let sender = kernel_process::inspect(u32::try_from(pid).ok()?)
            .ok()?
            .identity;
        let scope = coalition(pid, Some(token[7]))
            .map_err(|error| {
                tracing::debug!(pid, %error, "could not attest caller coalition");
            })
            .ok()?;
        if kind == ProofKind::Operator && self.hosted.contains(&scope) {
            tracing::debug!(pid, scope, "refused person proof for hosted coalition");
            return None;
        }
        let mut receipts = self.receipts.lock().ok()?;
        let now = Instant::now();
        receipts.retain(|_, proof| proof.expires > now);
        if receipts.len() >= LIMIT {
            return None;
        }
        let mut token = [0_u8; 32];
        getrandom::fill(&mut token).ok()?;
        let token = hex::encode(token);
        receipts.insert(
            token.clone(),
            Receipt {
                request,
                sender,
                kind,
                expires: now + TTL,
            },
        );
        tracing::debug!(pid, scope, "issued one-time person scope proof");
        Some(token)
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
    let proof = serde_json::from_slice::<Claim>(bytes)
        .ok()
        .and_then(|claim| scope.issue(audit, claim.request, claim.kind));
    if let Ok(bytes) = serde_json::to_vec(&proof) {
        unsafe { macos::senate_xpc_send_reply(reply, bytes.as_ptr(), bytes.len()) };
    }
}

pub(crate) async fn prove(path: &Path, request: &[u8], kind: ProofKind) -> Option<String> {
    app_confirmation::team()?;
    let service = service(path.parent()?);
    let bytes = serde_json::to_vec(&Claim {
        kind,
        request: Sha256::digest(request).into(),
    })
    .ok()?;
    tokio::time::timeout(
        Duration::from_secs(2),
        tokio::task::spawn_blocking(move || {
            macos::call::<Option<String>>(&service, &bytes).flatten()
        }),
    )
    .await
    .ok()?
    .ok()?
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
        assert!(
            eligible
                .issue(audit, request, ProofKind::Operator)
                .is_some()
        );

        let hosted = Scope {
            hosted: HashSet::from([own_coalition]),
            ..Scope::default()
        };
        assert!(hosted.issue(audit, request, ProofKind::Operator).is_none());

        audit[7] ^= 1;
        assert!(
            eligible
                .issue(audit, request, ProofKind::Operator)
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
