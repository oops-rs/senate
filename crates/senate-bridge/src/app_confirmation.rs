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
}

#[cfg(target_os = "macos")]
pub(crate) use queue::{EnqueueError, Queue};

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

    #[derive(Default)]
    pub(crate) struct Queue {
        state: Mutex<State>,
    }

    #[derive(Default)]
    struct State {
        last_app_poll: Option<Instant>,
        waiting: BTreeMap<EventId, Pending>,
    }

    struct Pending {
        request: Request,
        display: String,
        created: Instant,
        result: oneshot::Sender<Option<Request>>,
    }

    pub(crate) struct Ticket {
        id: EventId,
        queue: Arc<Queue>,
        pub(crate) reply: oneshot::Receiver<Option<Request>>,
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
        pub(crate) fn enqueue(self: &Arc<Self>, request: Request) -> Result<Ticket, EnqueueError> {
            let display = serde_json::to_string_pretty(&request)
                .map_err(|_| EnqueueError::RequestTooLarge)?;
            // Keep untrusted text visibly escaped in the signed app's prompt.
            let mut escaped = String::with_capacity(display.len());
            for ch in display.chars() {
                if ch.is_ascii() {
                    escaped.push(ch);
                } else {
                    escaped.extend(ch.escape_unicode());
                }
            }
            let display = escaped;
            if display.len() > 16 * 1024 {
                return Err(EnqueueError::RequestTooLarge);
            }
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

        // Only messages from the code-authenticated app reach these methods.
        pub(crate) fn poll(&self) -> Vec<Prompt> {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let now = Instant::now();
            state.last_app_poll = Some(now);
            state.expire(now);
            state
                .waiting
                .iter()
                .map(|(id, item)| Prompt {
                    id: id.clone(),
                    display: item.display.clone(),
                })
                .collect()
        }

        pub(crate) fn respond(&self, id: &EventId, approved: bool) -> bool {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.expire(Instant::now());
            let Some(pending) = state.waiting.remove(id) else {
                return false;
            };
            pending
                .result
                .send(approved.then_some(pending.request))
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
                queue.enqueue(Request::Ping),
                Err(EnqueueError::AppUnavailable)
            ));
            assert!(queue.poll().is_empty());
            let mut first = queue.enqueue(Request::Ping).unwrap();
            let mut second = queue.enqueue(Request::Status).unwrap();
            assert_ne!(first.id, second.id);
            assert!(!queue.respond(&EventId::new(), true));
            assert!(queue.respond(&first.id, true));
            assert!(matches!(first.reply.try_recv(), Ok(Some(Request::Ping))));
            assert!(!queue.respond(&first.id, true));
            assert!(queue.respond(&second.id, false));
            assert!(matches!(second.reply.try_recv(), Ok(None)));
        }

        #[test]
        fn dropped_or_expired_requests_cannot_be_approved() {
            let queue = Arc::new(Queue::default());
            queue.poll();
            let dropped = queue.enqueue(Request::Ping).unwrap();
            let id = dropped.id.clone();
            drop(dropped);
            assert!(!queue.respond(&id, true));
            let expired = queue.enqueue(Request::Ping).unwrap();
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
    }
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

#[cfg(target_os = "macos")]
fn call(paths: &Paths, command: AppCommand) -> Option<AppReply> {
    let message = AppMessage {
        home: paths.home().canonicalize().ok()?,
        command,
    };
    macos::call(
        &service_name(&message.home),
        &serde_json::to_vec(&message).ok()?,
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
    use std::path::PathBuf;
    use std::ptr::NonNull;
    use std::sync::Arc;

    struct Context {
        home: PathBuf,
        queue: Arc<Queue>,
    }

    unsafe extern "C" {
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
        ) -> *mut u8;
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

    fn requirement(identifier: &str) -> Option<CString> {
        let team = team()?;
        CString::new(format!(
            "anchor apple generic and identifier \"{identifier}\" and certificate leaf[subject.OU] = \"{team}\""
        )).ok()
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
        _audit: *const u32,
    ) {
        // libxpc owns the input and reply objects for the duration of this
        // callback. The listener's context outlives its peer connections.
        let context = unsafe { &*context.cast::<Context>() };
        let bytes = unsafe { std::slice::from_raw_parts(bytes, len) };
        let Ok(message) = serde_json::from_slice::<AppMessage>(bytes) else {
            return;
        };
        let response = if message.home != context.home {
            AppReply::WrongHome
        } else {
            match message.command {
                AppCommand::Poll => AppReply::Prompts {
                    prompts: context.queue.poll(),
                },
                AppCommand::Respond { id, approved } => AppReply::Recorded {
                    recorded: context.queue.respond(&id, approved),
                },
            }
        };
        if let Ok(data) = serde_json::to_vec(&response) {
            // xpc_dictionary_set_data copies these bytes into the reply.
            unsafe { senate_xpc_send_reply(reply, data.as_ptr(), data.len()) };
        }
    }

    pub(crate) fn call<T: serde::de::DeserializeOwned>(service: &str, message: &[u8]) -> Option<T> {
        let name = CString::new(service).ok()?;
        let requirement = requirement("dev.senate.daemon")?;
        let mut size = 0;
        // The arguments remain live until the synchronous XPC call returns.
        let data = unsafe {
            senate_xpc_call(
                name.as_ptr(),
                requirement.as_ptr(),
                message.as_ptr(),
                message.len(),
                &mut size,
            )
        };
        let data = NonNull::new(data)?;
        // C allocated the response with malloc, and `size` is its length.
        let decoded =
            unsafe { serde_json::from_slice(std::slice::from_raw_parts(data.as_ptr(), size)) }.ok();
        unsafe { libc::free(data.as_ptr().cast()) };
        decoded
    }
}

pub(crate) fn required(request: &crate::ipc::Request) -> bool {
    use crate::ipc::Request;
    matches!(request, Request::RebindHost { .. })
        || matches!(
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
