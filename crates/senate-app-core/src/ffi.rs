//! The facade the native apps call, exported with UniFFI.
//!
//! [`AppCore`] owns a tokio runtime on background threads and, once
//! [`AppCore::start`]ed, a [`Driver`] holding the daemon's `watch` stream.
//! Every change reaches the app as a full [`AppState`] snapshot through its
//! [`CoreListener`]; the app keeps no state of its own beyond drafts.
//!
//! Every exported call returns quickly: actions are `async` (they run on
//! the core's runtime; the foreign side awaits them), the rest only spawn
//! work or read what is already in memory. Listener calls arrive on the
//! core's threads; the app hops to its UI thread itself.

use std::sync::{Arc, Mutex, OnceLock};

use chrono::Utc;
use senate_bridge::Paths;
use senate_bridge::approvals::{Approval, Decision};
use senate_bridge::delivery::codex::CodexConfig;
use senate_bridge::ipc::{
    DefaultSecretary, DefaultSecretaryState, DeskDecision, DeskItemDetail, DeskOutcome,
    EnsureAction, Request, SessionRow, SessionSelector, ThreadReply, TimelineDirection,
    TimelineReply,
};
use senate_core::alias::AliasName;
use senate_core::{CommitteeId, EventId, Locale, Mention, Seq};
use tokio::runtime::{Handle, Runtime};

use crate::link::{Backoff, Driver, Link, Sink};
use crate::model::{self, Action, Failure, Model, Refresh};
use crate::names;
use crate::reads::{self, DeskItemView, DeskOutcomeView, ThreadView};
use crate::state::{AppState, ApprovalRequest, HostedSession};
use crate::text;
use crate::timeline::{self, TimelineDelta, TimelinePage};

/// Why a call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, uniffi::Error)]
pub enum CoreError {
    /// An action before [`AppCore::start`]. The app words this itself.
    #[error("the core is not started")]
    NotStarted,
    /// [`AppCore::start`] a second time. The app words this itself.
    #[error("the core is already started")]
    AlreadyStarted,
    /// The arguments are wrong (an empty reply, a bad committee id);
    /// `message` is in the app's language.
    #[error("{message}")]
    Invalid { message: String },
    /// The daemon refused, or could not be reached; `message` is the
    /// daemon's own, or the core's in the app's language.
    #[error("{message}")]
    Failed { message: String },
    /// The Senate home or `senate.toml` cannot be read, or the runtime
    /// cannot start. These are diagnostics and stay in English.
    #[error("{message}")]
    Setup { message: String },
}

/// What the app implements to hear from the core. Called on the core's
/// threads; implementations should hand off to their UI thread and return.
#[uniffi::export(foreign)]
pub trait CoreListener: Send + Sync {
    /// Everything the app shows, after any change.
    fn on_state(&self, state: AppState);
    /// A Codex approval request seen for the first time (for a notification);
    /// it is in the next [`AppState::approvals`] too.
    fn on_approval_requested(&self, approval: ApprovalRequest);
    /// Something came back to the person (an answer, a note, an escalation
    /// addressed to them): post a notification. The daemon posts none while
    /// the app is connected, so this is the only one.
    fn on_for_person(&self, note: PersonNote);
    /// A permission prompt waits in a hosted terminal, seen for the first
    /// time (for a notification); it is in the next
    /// [`AppState::sessions`] too.
    fn on_prompt_waiting(&self, session: HostedSession);
}

/// A note the person sent a session from the app, as the committee log
/// took it: the session's conversation shows it as a card with this
/// committee and seq once delivered.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SentNote {
    pub committee: String,
    pub seq: u64,
}

/// What to notify when something comes back to the person.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PersonNote {
    pub committee: String,
    /// The thread's root seq.
    pub thread: u64,
    /// `Senate · refund-v2`.
    pub title: String,
    /// `[senator::claude] answered: the first line`, in the app's
    /// language.
    pub body: String,
}

/// An answer to an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ApprovalDecision {
    Allow,
    /// Allow, and stop asking for the same thing this session.
    AllowForSession,
    /// Refuse; the turn goes on.
    Deny,
    /// Refuse and interrupt the turn.
    DenyAndStop,
}

impl From<ApprovalDecision> for Decision {
    fn from(d: ApprovalDecision) -> Self {
        match d {
            ApprovalDecision::Allow => Decision::Allow,
            ApprovalDecision::AllowForSession => Decision::AllowForSession,
            ApprovalDecision::Deny => Decision::Deny,
            ApprovalDecision::DenyAndStop => Decision::Cancel,
        }
    }
}

/// What to run in a terminal for a Codex thread. The app runs it (the core
/// does not drive other apps): `apple_script` for Terminal.app, or `argv`
/// in `cwd` for any other terminal.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TerminalCommand {
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    /// `cd <cwd> && <argv>`, shell-quoted.
    pub shell_line: String,
    /// Opens a Terminal.app window running `shell_line`.
    pub apple_script: String,
}

/// What a bracketed id in a text names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum MentionKind {
    /// A person: `[senator::alice]`.
    Senior,
    /// An agent: `[senator::claude]`.
    Junior,
    /// Whichever of a person's agents, the wildcard form of an agent's id.
    /// (Written out in a comment this would open a nested one in the generated
    /// Swift, so the example lives in the tests.)
    AnyAgent,
    /// `[speaker]`.
    Speaker,
    /// The Speaker answering for a senator: `[speaker::claude]`.
    SpeakerFor,
    /// `[committee]`.
    Committee,
    /// Someone who is not a senator: `[guest::chat/u_8f3a]`.
    Guest,
}

/// A piece of a text: plain words, or one mention.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TextPart {
    /// As written (a mention with its brackets).
    pub text: String,
    /// `None` for plain words.
    pub mention: Option<MentionKind>,
    /// A mention as people are shown it: `senate-chair`, `alice`,
    /// `copilot (Speaker)` ([`crate::names`]). `None` for plain words.
    pub display: Option<String>,
    /// A mention's id, without brackets: `senator::claude-a956…`, what a
    /// reader copies into a tool. `None` for plain words.
    pub id: Option<String>,
}

/// Split `text` into plain words and mentions, in order, each mention with
/// the name people are shown on `committee` (the one the text belongs to;
/// `None` when there is none). Brackets that hold anything else (`[1]`,
/// `[draft]`) stay plain. The pieces' `text` joined give `text` back.
#[uniffi::export]
pub fn split_mentions(text: String, committee: Option<String>) -> Vec<TextPart> {
    let names = names::current();
    let committee = committee.and_then(|c| CommitteeId::new(c).ok());
    let mut parts = Vec::new();
    let mut at = 0;
    let plain = |s: &str| TextPart {
        text: s.to_string(),
        mention: None,
        display: None,
        id: None,
    };
    for found in Mention::scan(&text) {
        if found.range.start > at {
            parts.push(plain(&text[at..found.range.start]));
        }
        let written = &text[found.range.clone()];
        parts.push(TextPart {
            text: written.to_string(),
            mention: Some(kind_of(&found.mention, &names.people)),
            display: Some(names.display(&found.mention, committee.as_ref())),
            id: Some(names::id_of(written)),
        });
        at = found.range.end;
    }
    if at < text.len() {
        parts.push(plain(&text[at..]));
    }
    parts
}

/// Changes whenever what [`split_mentions`] knows of names does: the app
/// keeps split texts until it moves.
#[uniffi::export]
pub fn mention_names_version() -> u64 {
    names::version()
}

fn kind_of(mention: &Mention, people: &senate_core::People) -> MentionKind {
    match mention {
        Mention::Senator(p) if people.is_person(p) => MentionKind::Senior,
        Mention::Senator(_) => MentionKind::Junior,
        Mention::AnyAgent => MentionKind::AnyAgent,
        // The chair is a link to a role, as the Speaker is.
        Mention::Speaker | Mention::Chair => MentionKind::Speaker,
        Mention::SpeakerFor(_) => MentionKind::SpeakerFor,
        Mention::Committee => MentionKind::Committee,
        Mention::Guest(_) => MentionKind::Guest,
        // A qualified name looks like what it names.
        Mention::Locator(l) => match Mention::of(l.address()) {
            Some(inner) => kind_of(&inner, people),
            None => MentionKind::Junior,
        },
    }
}

/// Told what a timeline watch sees, on the core's threads.
#[uniffi::export(foreign)]
pub trait TimelineListener: Send + Sync {
    fn on_delta(&self, delta: TimelineDelta);
    /// The watch ended: the daemon closed it, a `reset` asked for a fresh
    /// read, or it failed (`error`).
    fn on_end(&self, error: Option<String>);
}

/// The core: a runtime, the Senate home, and the driver once started.
#[derive(uniffi::Object)]
pub struct AppCore {
    paths: Paths,
    runtime: Mutex<Option<Runtime>>,
    handle: Handle,
    locale: Mutex<Locale>,
    driver: OnceLock<Arc<Driver<ListenerSink>>>,
    /// The timeline watches running, by session id.
    watches: Mutex<std::collections::HashMap<String, tokio::task::JoinHandle<()>>>,
}

#[uniffi::export]
impl AppCore {
    /// A core for the Senate home `senate_home` (where `bridge.sock` and
    /// `senate.toml` live); `None` or empty means `$SENATE_HOME`, else
    /// `~/.senate`. `language` is the app's language as a tag (`zh-Hans`,
    /// `en-US`): Chinese or English, and anything else, or `None`, is
    /// English. Starts the runtime; connects nothing until
    /// [`AppCore::start`].
    #[uniffi::constructor]
    pub fn new(
        senate_home: Option<String>,
        language: Option<String>,
    ) -> Result<Arc<Self>, CoreError> {
        init_logging();
        let paths = match senate_home.filter(|h| !h.trim().is_empty()) {
            Some(home) => Paths::new(home),
            None => Paths::from_env().map_err(|e| CoreError::Setup {
                message: e.to_string(),
            })?,
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("senate-core")
            .enable_all()
            .build()
            .map_err(|e| CoreError::Setup {
                message: format!("starting the runtime: {e}"),
            })?;
        let locale = locale_of(language.as_deref());
        names::update(|n| n.locale = locale);
        Ok(Arc::new(Self {
            paths,
            handle: runtime.handle().clone(),
            runtime: Mutex::new(Some(runtime)),
            locale: Mutex::new(locale),
            driver: OnceLock::new(),
            watches: Mutex::default(),
        }))
    }

    /// The Senate home in use.
    pub fn senate_home(&self) -> String {
        self.paths
            .socket()
            .parent()
            .map_or_else(String::new, |p| p.display().to_string())
    }

    /// Write texts in another language from now on; the next state follows.
    pub fn set_language(&self, language: Option<String>) {
        let locale = locale_of(language.as_deref());
        names::update(|n| n.locale = locale);
        *self.locale.lock().unwrap_or_else(|e| e.into_inner()) = locale;
        if let Some(driver) = self.driver.get() {
            driver.set_locale(locale);
        }
    }

    /// Connect to the daemon and keep connecting: from now on every change
    /// is a [`CoreListener::on_state`]. The first one (offline, or online)
    /// follows within a moment.
    pub fn start(&self, listener: Arc<dyn CoreListener>) -> Result<(), CoreError> {
        let driver = Driver::new(
            Link::new(self.paths.socket()),
            ListenerSink { listener },
            Backoff::default(),
            self.locale(),
        );
        self.driver
            .set(Arc::clone(&driver))
            .map_err(|_| CoreError::AlreadyStarted)?;
        self.handle.spawn(driver.run());
        Ok(())
    }

    /// The current state, without waiting for the listener.
    pub fn snapshot(&self) -> AppState {
        match self.driver.get() {
            Some(d) => AppState::from_model(&d.model(), Utc::now()),
            None => AppState::from_model(&Model::new(self.locale()), Utc::now()),
        }
    }

    /// Re-read every inbox, the approvals and the docket; the result arrives
    /// as state.
    pub fn refresh(&self) -> Result<(), CoreError> {
        let driver = Arc::clone(self.driver()?);
        self.handle.spawn(async move {
            driver.refresh(Refresh::Approvals).await;
            driver.refresh(Refresh::AllInboxes).await;
            driver.refresh(Refresh::Docket).await;
            driver.refresh_aliases(true).await;
            driver.changed();
        });
        Ok(())
    }

    /// While the daemon is unreachable, try again now rather than after the
    /// current wait (0.5 s doubling to 10 s).
    pub fn reconnect(&self) -> Result<(), CoreError> {
        self.driver()?.reconnect_now();
        Ok(())
    }

    /// Reply in a thread as the person (`senator::<person>`).
    pub async fn reply(
        &self,
        committee: String,
        thread: u64,
        body: String,
    ) -> Result<(), CoreError> {
        let committee = self.committee_id(&committee)?;
        if body.trim().is_empty() {
            return Err(CoreError::Invalid {
                message: text::empty_reply(self.locale()),
            });
        }
        self.act(Action::Answer {
            committee,
            thread: Seq(thread),
            body,
        })
        .await
    }

    /// Take the thread over: the daemon cancels queued deliveries of it to
    /// the agent.
    pub async fn take(&self, committee: String, thread: u64) -> Result<(), CoreError> {
        let committee = self.committee_id(&committee)?;
        self.act(Action::Take {
            committee,
            thread: Seq(thread),
        })
        .await
    }

    /// The person has seen the thread up to `seen` (its `updated` seq, or the
    /// last event they were shown): read state on this machine, nothing is
    /// appended to the log. Replying and taking mark it too.
    pub async fn mark_read(
        &self,
        committee: String,
        thread: u64,
        seen: u64,
    ) -> Result<(), CoreError> {
        let committee = self.committee_id(&committee)?;
        self.act(Action::MarkRead {
            committee,
            thread: Seq(thread),
            seen: Seq(seen),
        })
        .await
    }

    /// Archive the thread, as the person.
    pub async fn close(&self, committee: String, thread: u64) -> Result<(), CoreError> {
        let committee = self.committee_id(&committee)?;
        self.act(Action::Close {
            committee,
            thread: Seq(thread),
        })
        .await
    }

    /// Answer an approval request. The first answer wins across the TUI,
    /// the app and any chat platform; a late one fails with the daemon's
    /// reason.
    pub async fn approve(&self, id: String, decision: ApprovalDecision) -> Result<(), CoreError> {
        self.act(Action::Approve {
            id,
            decision: decision.into(),
        })
        .await
    }

    /// One thread, root first, read as the person: showing it marks it
    /// seen up to its last event, and the inboxes on its committee are read
    /// again afterwards (the result arrives as state).
    pub async fn thread(&self, committee: String, thread: u64) -> Result<ThreadView, CoreError> {
        let committee = self.committee_id(&committee)?;
        let request = Request::Thread {
            session: SessionSelector::default(),
            committee: Some(committee.clone()),
            thread: Seq(thread),
            mark_seen: true,
        };
        let locale = self.locale();
        let reply: ThreadReply = self
            .on_runtime(move |driver| async move {
                let reply = driver.read::<ThreadReply>(&request).await;
                if reply.is_ok() {
                    // Its read state changed: the inboxes follow, in the
                    // background, so the thread shows without waiting.
                    let again = Arc::clone(&driver);
                    tokio::spawn(async move { again.refresh_committee(&committee).await });
                }
                reply
            })
            .await?;
        let person = self
            .driver()?
            .model()
            .senates()
            .next()
            .map(|s| s.person.clone());
        Ok(reads::thread_view(reply, person.as_ref(), locale))
    }

    pub async fn desk_item(&self, committee: String, seq: u64) -> Result<DeskItemView, CoreError> {
        let committee = self.committee_id(&committee)?;
        let request = Request::DeskItem {
            committee,
            seq: Seq(seq),
        };
        let detail: DeskItemDetail = self
            .on_runtime(move |driver| async move { driver.read(&request).await })
            .await?;
        let person = self
            .driver()?
            .model()
            .senates()
            .next()
            .map(|s| s.person.clone());
        Ok(reads::desk_item_view(
            detail,
            person.as_ref(),
            self.locale(),
        ))
    }

    /// Keep this ID across retries of a single decision.
    pub fn new_desk_action_id(&self) -> String {
        EventId::new().to_string()
    }

    /// A missing reason forwards the item. A present reason refuses it.
    pub async fn decide_desk_item(
        &self,
        committee: String,
        seq: u64,
        item_id: String,
        id: String,
        reason: Option<String>,
    ) -> Result<DeskOutcomeView, CoreError> {
        let committee = self.committee_id(&committee)?;
        let event_id = |value: &str| {
            value
                .parse::<EventId>()
                .map_err(|error| CoreError::Invalid {
                    message: error.to_string(),
                })
        };
        let item_id = event_id(&item_id)?;
        let id = event_id(&id)?;
        let decision = reason.map_or(DeskDecision::Forward, |reason| DeskDecision::Refuse {
            reason,
        });
        let request = Request::DeskDecide {
            committee,
            seq: Seq(seq),
            item_id,
            id,
            decision,
        };
        let outcome: DeskOutcome = self
            .on_runtime(move |driver| async move {
                let outcome = driver.read(&request).await?;
                tokio::spawn(async move { driver.refresh_desk().await });
                Ok(outcome)
            })
            .await?;
        Ok(DeskOutcomeView::new(outcome, self.locale()))
    }

    /// The machine's default secretary, as it stands: where it sits, its
    /// senator once there is one, and its state. Changes nothing.
    pub async fn default_secretary(&self) -> Result<SecretaryView, CoreError> {
        let s: DefaultSecretary = self
            .on_runtime(|driver| async move { driver.read(&Request::DefaultSecretary).await })
            .await?;
        Ok(s.into())
    }

    /// Make sure the default secretary is running, as the person: start it
    /// (none yet), resume it (paused) or restore it (its terminal ended).
    /// It may take as long as a spawn. Refused, with the daemon's message,
    /// when it was stopped on purpose or its seat is someone else's.
    pub async fn ensure_secretary(&self) -> Result<SecretaryView, CoreError> {
        let s: DefaultSecretary = self
            .on_runtime(|driver| async move { driver.read(&Request::EnsureSecretary).await })
            .await?;
        Ok(s.into())
    }

    /// A page of ended and stopped sessions, newest first, on `committee`
    /// (all when `None`), after the page `before` named.
    pub async fn history(
        &self,
        committee: Option<String>,
        before: Option<String>,
        limit: u32,
    ) -> Result<crate::history::HistoryPage, CoreError> {
        let committee =
            committee
                .map(CommitteeId::new)
                .transpose()
                .map_err(|e| CoreError::Invalid {
                    message: e.to_string(),
                })?;
        let request = Request::SessionHistory {
            committee,
            before,
            limit: Some(limit as usize),
        };
        let reply: senate_bridge::ipc::SessionHistoryReply = self
            .on_runtime(move |driver| async move { driver.read(&request).await })
            .await?;
        Ok(crate::history::HistoryPage {
            rows: reply.rows.into_iter().map(Into::into).collect(),
            next: reply.next_before,
        })
    }

    /// Search past and live conversations for `query`.
    pub async fn search_sessions(
        &self,
        query: String,
        committee: Option<String>,
        cursor: Option<String>,
        limit: u32,
    ) -> Result<crate::history::SearchPage, CoreError> {
        let committee =
            committee
                .map(CommitteeId::new)
                .transpose()
                .map_err(|e| CoreError::Invalid {
                    message: e.to_string(),
                })?;
        let request = Request::SessionSearch {
            query,
            committee,
            limit: Some(limit as usize),
            cursor,
        };
        let reply: senate_bridge::ipc::SessionSearchReply = self
            .on_runtime(move |driver| async move { driver.read(&request).await })
            .await?;
        Ok(crate::history::SearchPage {
            hits: reply.hits.into_iter().map(Into::into).collect(),
            next: reply.next_cursor,
            indexing: reply.indexing,
            indexed: reply.progress.map(|p| p.indexed as u64),
            total: reply.progress.map(|p| p.total as u64),
        })
    }

    /// Forget an ended or stopped session's record (not the agent's own
    /// transcript, which stays where its runtime keeps it).
    pub async fn forget_session(&self, session_id: String) -> Result<(), CoreError> {
        let request = Request::SessionForget { session_id };
        let _: serde_json::Value = self
            .on_runtime(move |driver| async move { driver.read(&request).await })
            .await?;
        Ok(())
    }

    /// The archived committees, by name (`senate committees --archived`):
    /// what "Show archived" lists. The roster leaves them out.
    pub async fn archived_committees(&self) -> Result<Vec<String>, CoreError> {
        let list: Vec<senate_core::CommitteeId> = self
            .on_runtime(|driver| async move { driver.read(&Request::ArchivedCommittees).await })
            .await?;
        Ok(list.into_iter().map(|c| c.to_string()).collect())
    }

    /// Unarchive a committee, as the person (`senate committees unarchive
    /// <name>`): the Speaker in person only, which the server checks. The
    /// roster is read again after.
    pub async fn unarchive_committee(&self, committee: String) -> Result<(), CoreError> {
        let committee =
            senate_core::CommitteeId::new(committee.trim()).map_err(|e| CoreError::Invalid {
                message: e.to_string(),
            })?;
        self.on_runtime(|driver| async move {
            let _: serde_json::Value = driver
                .read(&Request::ArchiveCommittee {
                    committee,
                    archived: false,
                    id: None,
                })
                .await?;
            driver.refresh(crate::model::Refresh::Roster).await;
            Ok(())
        })
        .await
    }

    /// End a hosted session's terminal, as the person (`senate session
    /// stop <label>`). Never forced: the daemon spares a terminal someone
    /// is typing in, and says so.
    pub async fn stop_session(&self, label: String) -> Result<(), CoreError> {
        let senator = self.session_label(&label)?;
        self.act(Action::StopSession { senator }).await
    }

    /// Resume a stopped hosted session in the background (`senate session
    /// restore <label>`), with the model it last had.
    pub async fn restore_session(&self, label: String) -> Result<(), CoreError> {
        let senator = self.session_label(&label)?;
        self.act(Action::RestoreSession { senator }).await
    }

    /// Answer the permission prompt waiting in a hosted session's terminal
    /// (`senate session approve <label>`), as the person: allow once,
    /// always (`AllowForSession`) or deny. The daemon answers it as they
    /// would at the terminal and notes it in the committee log; it refuses
    /// a prompt it only saw on the screen, which is answered by attaching.
    pub async fn approve_session(
        &self,
        label: String,
        decision: ApprovalDecision,
    ) -> Result<(), CoreError> {
        let senator = self.session_label(&label)?;
        self.act(Action::ApproveSession {
            senator,
            decision: decision.into(),
        })
        .await
    }

    /// Resume a paused hosted session where it was (`senate session resume
    /// <label>`).
    pub async fn resume_session(&self, label: String) -> Result<(), CoreError> {
        let senator = self.session_label(&label)?;
        self.act(Action::ResumeSession { senator }).await
    }

    /// Pause a hosted session (`senate session pause <label>`): it is asked
    /// to checkpoint (the daemon waits up to 90 s), its terminal stops and
    /// it is recorded paused; Resume brings it back where it was.
    pub async fn pause_session(&self, label: String) -> Result<(), CoreError> {
        let senator = self.session_label(&label)?;
        self.act(Action::PauseSession { senator }).await
    }

    /// Say something to the senator of a session, as the person: a note
    /// to it in its committee's log (`senate tell`), which the daemon
    /// delivers to the session. Nothing is typed into its terminal.
    pub async fn say(&self, session_id: String, body: String) -> Result<SentNote, CoreError> {
        let (to, committee) = {
            let driver = self.driver()?;
            let model = driver.model();
            let reply = model.sessions.as_ref();
            let hosted = reply
                .and_then(|r| r.rows.iter().find(|x| x.session_id == session_id))
                .map(|x| (x.primary.clone(), x.committee.clone()));
            let local = || {
                reply
                    .and_then(|r| r.local_rows.iter().find(|x| x.session_id == session_id))
                    .map(|x| (x.primary.clone(), x.committee.clone()))
            };
            hosted.or_else(local).ok_or_else(|| CoreError::Invalid {
                message: text::no_session(self.locale(), &session_id),
            })?
        };
        let committee = committee.ok_or_else(|| CoreError::Invalid {
            message: text::say_needs_committee(self.locale()),
        })?;
        if body.trim().is_empty() {
            return Err(CoreError::Invalid {
                message: text::empty_reply(self.locale()),
            });
        }
        let posted = self
            .on_runtime(move |driver| async move {
                driver
                    .act(Action::Say {
                        committee,
                        to,
                        body,
                    })
                    .await
            })
            .await?;
        let posted: senate_bridge::ipc::Posted =
            serde_json::from_value(posted).map_err(|e| CoreError::Failed {
                message: e.to_string(),
            })?;
        Ok(SentNote {
            committee: posted.committee.to_string(),
            seq: posted.seq.0,
        })
    }

    /// Stage one attachment for a running hosted session, returning the
    /// path that can be included in a note. This does not post the note.
    pub async fn session_drop(
        &self,
        session_id: String,
        name: String,
        bytes: Vec<u8>,
    ) -> Result<String, CoreError> {
        self.on_runtime(move |driver| async move {
            driver
                .link()
                .session_drop(session_id, name, &bytes)
                .await
                .map_err(|error| Failure::Daemon(model::message(&error)))
        })
        .await
    }

    /// A page of a session's conversation: the latest `limit` items (60 by
    /// default) when `before` is `None`, else the `limit` before that seq.
    pub async fn timeline(
        &self,
        session_id: String,
        before: Option<u64>,
        limit: Option<u32>,
    ) -> Result<TimelinePage, CoreError> {
        let request = Request::Timeline {
            session_id,
            direction: if before.is_some() {
                TimelineDirection::Before
            } else {
                TimelineDirection::Tail
            },
            cursor: before,
            limit: limit.map(|l| l as usize),
        };
        let locale = self.locale();
        let reply: TimelineReply = self
            .on_runtime(move |driver| async move { driver.read::<TimelineReply>(&request).await })
            .await?;
        Ok(timeline::page(reply, locale))
    }

    /// Follow a session's conversation from after `after` (the last seq
    /// read): each change is a [`TimelineListener::on_delta`] until
    /// [`AppCore::stop_timeline`], or [`TimelineListener::on_end`] says it
    /// ended (on a `reset` delta, read the tail again and watch anew). A
    /// second watch of the same session replaces the first.
    pub fn watch_timeline(
        &self,
        session_id: String,
        after: u64,
        listener: Arc<dyn TimelineListener>,
    ) -> Result<(), CoreError> {
        let key = session_id.clone();
        let link = self.driver()?.link().clone();
        let locale = self.locale();
        let task = self.handle.spawn(async move {
            let mut stream = match link.timeline_watch(&session_id, after).await {
                Ok(s) => s,
                Err(e) => {
                    listener.on_end(Some(model::message(&e)));
                    return;
                }
            };
            loop {
                match stream.next().await {
                    Ok(Some(update)) => {
                        let reset = update.reset;
                        listener.on_delta(timeline::delta(update, locale));
                        if reset {
                            listener.on_end(None);
                            return;
                        }
                    }
                    Ok(None) => {
                        listener.on_end(None);
                        return;
                    }
                    Err(e) => {
                        listener.on_end(Some(model::message(&e)));
                        return;
                    }
                }
            }
        });
        let old = self
            .watches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, task);
        if let Some(old) = old {
            old.abort();
        }
        Ok(())
    }

    /// Stop following a session's conversation.
    pub fn stop_timeline(&self, session_id: String) {
        let task = self
            .watches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session_id);
        if let Some(task) = task {
            task.abort();
        }
    }

    /// What to run to open a hosted session's terminal: `senate attach
    /// <label>` (Ctrl-] detaches). A prompt Senate cannot answer is decided
    /// there.
    pub fn attach_terminal(&self, label: String) -> Result<TerminalCommand, CoreError> {
        let senator = self.session_label(&label)?;
        Ok(self.senate_command(&["attach", &senator.to_string()]))
    }

    /// Open exactly this hosted session, even when its alias has changed.
    pub fn attach_terminal_for_session(&self, session_id: String) -> TerminalCommand {
        self.senate_command(&["attach", &format!("--session={session_id}")])
    }

    /// What to run to watch every hosted terminal side by side (`senate
    /// session watch`).
    pub fn watch_terminal(&self) -> TerminalCommand {
        self.senate_command(&["session", "watch"])
    }

    /// What to run to open Codex in a terminal: resuming `thread_id` in its
    /// directory, or a new Codex when `None`. Reads `[codex]` from
    /// `senate.toml` for the binary and the `--remote` address.
    pub fn codex_terminal(&self, thread_id: Option<String>) -> Result<TerminalCommand, CoreError> {
        let config = CodexConfig::load(&self.paths.config())
            .map_err(|message| CoreError::Setup { message })?;
        let cwd = thread_id.as_ref().and_then(|id| {
            self.driver.get().and_then(|d| {
                d.model()
                    .codex_sessions()
                    .into_iter()
                    .find(|x| &x.session_id == id)
                    .map(|x| x.cwd.display().to_string())
            })
        });
        Ok(terminal_command(&config, cwd, thread_id.as_deref()))
    }
}

impl AppCore {
    fn locale(&self) -> Locale {
        *self.locale.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn driver(&self) -> Result<&Arc<Driver<ListenerSink>>, CoreError> {
        self.driver.get().ok_or(CoreError::NotStarted)
    }

    fn session_label(&self, label: &str) -> Result<AliasName, CoreError> {
        label.parse().map_err(|e| CoreError::Invalid {
            message: match self.locale() {
                Locale::Zh => format!("会话 {label:?} 无效：{e}"),
                Locale::En => format!("session {label:?}: {e}"),
            },
        })
    }

    /// `senate <args>` in a terminal, with this app's Senate home.
    fn senate_command(&self, args: &[&str]) -> TerminalCommand {
        let home = self.senate_home();
        let mut argv = vec!["senate".to_string()];
        argv.extend(args.iter().map(ToString::to_string));
        let words: Vec<String> = argv.iter().map(|a| model::shell_quote(a)).collect();
        let shell_line = format!(
            "SENATE_HOME={} {}",
            model::shell_quote(&home),
            words.join(" ")
        );
        TerminalCommand {
            argv,
            cwd: None,
            apple_script: model::terminal_script(&shell_line),
            shell_line,
        }
    }

    fn committee_id(&self, committee: &str) -> Result<CommitteeId, CoreError> {
        CommitteeId::new(committee).map_err(|e| CoreError::Invalid {
            message: match self.locale() {
                Locale::Zh => format!("committee {committee:?} 无效：{e}"),
                Locale::En => format!("committee {committee:?}: {e}"),
            },
        })
    }

    async fn act(&self, action: Action) -> Result<(), CoreError> {
        self.on_runtime(move |driver| async move { driver.act(action).await })
            .await
            .map(drop)
    }

    /// Run `op` with the driver on the core's runtime (the caller's
    /// executor is not tokio); a failure in the app's language.
    async fn on_runtime<T, F, Fut>(&self, op: F) -> Result<T, CoreError>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Driver<ListenerSink>>) -> Fut,
        Fut: std::future::Future<Output = Result<T, Failure>> + Send + 'static,
    {
        let driver = Arc::clone(self.driver()?);
        let locale = self.locale();
        self.handle
            .spawn(op(driver))
            .await
            .map_err(|e| CoreError::Failed {
                message: format!("the action did not finish: {e}"),
            })?
            .map_err(|failure: Failure| CoreError::Failed {
                message: failure.text(locale),
            })
    }
}

impl Drop for AppCore {
    fn drop(&mut self) {
        // Dropping a runtime blocks on its tasks, which panics inside
        // another runtime; let them go in the background instead.
        let runtime = self
            .runtime
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(rt) = runtime {
            rt.shutdown_background();
        }
    }
}

/// The app's language from a tag; English when there is none or it is not
/// Chinese or English.
fn locale_of(language: Option<&str>) -> Locale {
    language
        .and_then(|l| l.parse::<Locale>().ok())
        .unwrap_or_default()
}

/// The driver's [`Sink`]: snapshots to the foreign listener.
struct ListenerSink {
    listener: Arc<dyn CoreListener>,
}

impl Sink for ListenerSink {
    fn changed(&self, model: &Model) {
        self.listener
            .on_state(AppState::from_model(model, Utc::now()));
    }

    fn approval_arrived(&self, approval: &Approval) {
        self.listener.on_approval_requested(approval.into());
    }

    fn prompt_waiting(&self, row: &SessionRow) {
        self.listener.on_prompt_waiting(row.into());
    }

    fn for_person(&self, committee: &str, thread: u64, title: &str, body: &str) {
        self.listener.on_for_person(PersonNote {
            committee: committee.to_string(),
            thread,
            title: title.to_string(),
            body: body.to_string(),
        });
    }
}

/// The command line for "open in terminal" from `[codex]` in `senate.toml`.
pub fn terminal_command(
    config: &CodexConfig,
    cwd: Option<String>,
    thread: Option<&str>,
) -> TerminalCommand {
    let bin = config
        .bin
        .as_ref()
        .map_or_else(|| "codex".to_string(), |b| b.display().to_string());
    let remote = model::codex_remote(config.listen.as_deref());
    let cwd = cwd.filter(|c| !c.is_empty());
    let mut argv = vec![bin.clone()];
    if thread.is_some() {
        argv.push("resume".into());
    }
    argv.push("--remote".into());
    argv.push(remote.clone());
    if let Some(id) = thread {
        argv.push(id.into());
    }
    let shell_line = model::codex_shell_line(&bin, &remote, cwd.as_deref(), thread);
    TerminalCommand {
        argv,
        cwd,
        apple_script: model::terminal_script(&shell_line),
        shell_line,
    }
}

/// Logs to stderr, `RUST_LOG` or `senate_app_core=info`; once per process.
fn init_logging() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "senate_app_core=info,senate_bridge=warn".into());
        // stderr for a run from a shell (`RUST_LOG`); the unified log for
        // every run, where Console and `log stream` find it.
        let stderr = tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()));
        let registry = tracing_subscriber::registry().with(filter).with(stderr);
        #[cfg(target_os = "macos")]
        let registry = registry.with(crate::oslog::OsLogLayer);
        let _ = registry.try_init();
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn decisions_map_to_the_daemon() {
        assert_eq!(
            Decision::from(ApprovalDecision::DenyAndStop),
            Decision::Cancel
        );
        assert_eq!(
            Decision::from(ApprovalDecision::AllowForSession),
            Decision::AllowForSession
        );
    }

    #[test]
    fn terminal_command_resumes_in_the_thread_directory() {
        let config = CodexConfig::default();
        let c = terminal_command(&config, Some("/w/my repo".into()), Some("019a"));
        assert_eq!(c.argv, ["codex", "resume", "--remote", "unix://", "019a"]);
        assert_eq!(c.cwd.as_deref(), Some("/w/my repo"));
        assert_eq!(
            c.shell_line,
            "cd '/w/my repo' && codex resume --remote unix:// 019a"
        );
        assert!(c.apple_script.contains("tell application \"Terminal\""));

        let config = CodexConfig {
            listen: Some("ws://127.0.0.1:4500".into()),
            bin: Some("/opt/codex".into()),
        };
        let c = terminal_command(&config, None, None);
        assert_eq!(c.argv, ["/opt/codex", "--remote", "ws://127.0.0.1:4500"]);
        assert_eq!(c.cwd, None);
    }

    #[test]
    fn offline_state_before_start_in_either_language() {
        let core =
            AppCore::new(Some("/nonexistent/senate".into()), Some("zh-Hans".into())).unwrap();
        let s = core.snapshot();
        assert!(!s.online);
        assert_eq!(s.menu.title, "Senate：离线");
        assert_eq!(s.counts.pending, 0);
        assert_eq!(core.senate_home(), "/nonexistent/senate");
        assert_eq!(core.refresh(), Err(CoreError::NotStarted));
        core.set_language(Some("en-US".into()));
        assert_eq!(core.snapshot().menu.title, "Senate: offline");
        // Anything that is neither language is English.
        core.set_language(Some("fr".into()));
        assert_eq!(core.snapshot().menu.title, "Senate: offline");
        core.set_language(None);
        assert_eq!(core.snapshot().menu.title, "Senate: offline");
    }

    #[test]
    fn mentions_are_split_out_of_running_text() {
        // Names are flat: who is a person is what the daemon reported.
        names::update(|n| {
            n.people = senate_core::People::new([senate_core::Name::new("alice").unwrap()]);
        });
        let parts = split_mentions("请[senator::claude]看一下 [1] 和 [speaker]".into(), None);
        let shown: Vec<_> = parts.iter().map(|p| (p.text.as_str(), p.mention)).collect();
        assert_eq!(
            shown,
            vec![
                ("请", None),
                ("[senator::claude]", Some(MentionKind::Junior)),
                ("看一下 [1] 和 ", None),
                ("[speaker]", Some(MentionKind::Speaker)),
            ]
        );
        let joined: String = parts.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(joined, "请[senator::claude]看一下 [1] 和 [speaker]");
        assert_eq!(split_mentions(String::new(), None), Vec::new());
        let kinds: Vec<_> = split_mentions(
            "[senator::alice] [senator::*] [speaker::claude] [committee] [guest::chat/u1] \
             [ledger::senator::codex]"
                .into(),
            None,
        )
        .into_iter()
        .filter_map(|p| p.mention)
        .collect();
        assert_eq!(
            kinds,
            vec![
                MentionKind::Senior,
                MentionKind::AnyAgent,
                MentionKind::SpeakerFor,
                MentionKind::Committee,
                MentionKind::Guest,
                MentionKind::Junior,
            ]
        );
    }
}

/// The machine's default secretary ([`AppCore::default_secretary`]).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SecretaryView {
    /// Where it sits: `office`.
    pub committee: String,
    /// Its alias there: `secretary`.
    pub alias: String,
    /// Its senator's name, once there is one.
    pub senator: Option<String>,
    pub state: SecretaryState,
    /// What an ensure did: `started`, `resumed`, `restored`,
    /// `already_running`; none for a plain read.
    pub action: Option<String>,
}

/// Where the default secretary stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SecretaryState {
    /// None yet: the first message starts one.
    NotRecorded,
    Running,
    /// Paused: the next message resumes it.
    Paused,
    /// Its terminal ended (a restart): the next message restores it.
    Ended,
    /// Stopped on purpose: only a restore brings it back.
    Stopped,
    /// Its seat has no session on this machine.
    Elsewhere,
}

impl From<DefaultSecretary> for SecretaryView {
    fn from(s: DefaultSecretary) -> Self {
        Self {
            committee: s.committee.to_string(),
            alias: s.alias.to_string(),
            senator: s.senator.map(|n| n.to_string()),
            state: match s.state {
                DefaultSecretaryState::NotRecorded => SecretaryState::NotRecorded,
                DefaultSecretaryState::Running => SecretaryState::Running,
                DefaultSecretaryState::Paused => SecretaryState::Paused,
                DefaultSecretaryState::Ended => SecretaryState::Ended,
                DefaultSecretaryState::Stopped => SecretaryState::Stopped,
                DefaultSecretaryState::Elsewhere => SecretaryState::Elsewhere,
            },
            action: s.action.map(|a| {
                match a {
                    EnsureAction::Started => "started",
                    EnsureAction::Resumed => "resumed",
                    EnsureAction::Restored => "restored",
                    EnsureAction::AlreadyRunning => "already_running",
                }
                .to_string()
            }),
        }
    }
}
