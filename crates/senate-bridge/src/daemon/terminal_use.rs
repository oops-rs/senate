//! Terminal use: who may read and drive which terminal.
//!
//! Two things are served here ([`Request::Terminal`]):
//!
//! - **Scratch terminals** ([`crate::host::scratch`]). A session opens one
//!   from its own shell and owns it: every later action names it by id,
//!   and the id resolves only in the record this daemon wrote when it
//!   reserved it, and only for the session that opened it (or the person).
//!   Typing, keys, resizing, waiting and closing exist for scratch
//!   terminals alone. The host of a hosted session is never a scratch
//!   terminal, so no request can type into an agent's session: an agent
//!   that could would be able to answer that session's permission prompts.
//! - **A hosted session's screen, read only**, for the chair of its
//!   committee: what a stuck senator is showing. It is read as a watcher
//!   reads it ([`crate::host::look`]), which can type nothing.
//!
//! The daemon starts no program for any of this. A scratch terminal's host
//! is started by the process that asked for it, under that process's own
//! rights; this module reserves, records, checks and relays.

use std::hash::{Hash as _, Hasher as _};

use tokio::time::Instant as Clock;

use super::*;
use crate::host;
use crate::host::look::{Look, Seen, Shown};
use crate::host::scratch::{self, Control, Record, Sent, State};
use crate::ipc::{TerminalAction, TerminalReply, TerminalRow, TerminalScreen, TerminalTarget};

/// The most scratch terminals one session may have at a time.
pub(crate) const SESSION_LIMIT: usize = 4;
/// The most the person may have: a bound on a script that never closes.
const PERSON_LIMIT: usize = 16;
/// The longest a `wait` may be asked to last.
pub(crate) const MAX_WAIT: Duration = Duration::from_secs(300);
/// How long a program has to start answering what was typed.
const REACT: Duration = Duration::from_millis(500);
/// How long the screen must be still to count as settled after input, and
/// the most that is waited for it.
const STILL: Duration = Duration::from_millis(150);
const SETTLE_LIMIT: Duration = Duration::from_secs(2);
/// How often a `wait` looks for the program's exit while nothing is written.
const EXIT_POLL: Duration = Duration::from_millis(250);
/// A reservation whose host never took it is forgotten after this.
const START_GRACE: Duration = Duration::from_secs(20);
const ROLE_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Default)]
pub(super) struct Monitor {
    /// One reservation at a time, so the limit counts what it should.
    opening: tokio::sync::Mutex<()>,
    /// What each scratch terminal last showed the one who opened it, for
    /// a `wait` that waits for anything else.
    shown: Mutex<HashMap<EventId, u64>>,
}

/// Who asks.
enum Caller {
    /// No session: the person's own terminal or app, past the person guard.
    Person,
    Session(Box<Session>),
}

impl Caller {
    fn owner(&self) -> Option<&str> {
        match self {
            Self::Person => None,
            Self::Session(session) => Some(&session.session_id),
        }
    }

    /// Whether `record`'s terminal is the caller's to read and drive.
    fn owns(&self, record: &Record) -> bool {
        match self {
            Self::Person => true,
            Self::Session(session) => record.owner.as_deref() == Some(&session.session_id),
        }
    }
}

/// A scratch terminal a caller may use.
struct Mine<'a> {
    paths: &'a Paths,
    id: &'a EventId,
    /// The caller opened it: what it is shown is what its later `wait`
    /// for a change compares with. The person looking at a session's
    /// terminal leaves that as it was.
    opener: bool,
}

fn fingerprint(shown: &Shown) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (shown.rows, shown.cols, &shown.lines).hash(&mut hasher);
    hasher.finish()
}

fn check_size(rows: u16, cols: u16) -> Result<(), IpcError> {
    if (1..=scratch::MAX_ROWS).contains(&rows) && (1..=scratch::MAX_COLS).contains(&cols) {
        return Ok(());
    }
    Err(IpcError::bad_request(format!(
        "a terminal is 1 to {} rows and 1 to {} columns",
        scratch::MAX_ROWS,
        scratch::MAX_COLS
    )))
}

/// A person is attached and has the keyboard: said so the caller stops.
fn held(id: &EventId) -> IpcError {
    IpcError::bad_request(format!(
        "a person is attached to terminal {id} and has its keyboard, so nothing was sent. \
         Do not retry: the terminal is theirs until they detach (`list` shows it as attached)."
    ))
}

/// One answer for an id that is no scratch terminal and for one that is
/// someone else's, so a caller learns nothing about terminals not its own.
fn unknown(id: &EventId) -> IpcError {
    IpcError::new(
        codes::NO_SESSION,
        format!(
            "no scratch terminal {id} of yours. `list` shows the ones you opened; \
             the terminal of a hosted session is not one and cannot be driven"
        ),
    )
}

async fn close(paths: Paths, id: EventId) {
    if let Err(error) = host::terminate(&paths, &id, Duration::from_secs(3)).await {
        tracing::warn!(%error, %id, "closing a scratch terminal");
    }
    // A host that was killed, or never started, leaves its directory.
    if let Err(error) = scratch::remove(&paths, &id) {
        tracing::warn!(%error, %id, "removing a closed scratch terminal");
    }
}

/// What a `wait` waits for.
enum Until {
    /// The screen contains this.
    Text(String),
    /// Nothing was written for this long.
    Still(Duration),
    /// The screen is other than its opener last saw.
    Changed,
}

fn screen(shown: Shown) -> TerminalScreen {
    TerminalScreen {
        rows: shown.rows,
        cols: shown.cols,
        cursor_row: shown.cursor.0,
        cursor_col: shown.cursor.1,
        cursor_hidden: shown.cursor_hidden,
        alternate: shown.alternate,
        scrollback: shown.scrollback,
        lines: shown.lines,
        ..TerminalScreen::default()
    }
}

impl Shared {
    pub(super) async fn on_terminal(
        &self,
        selector: &SessionSelector,
        action: TerminalAction,
    ) -> Result<TerminalReply, IpcError> {
        let caller = if *selector == SessionSelector::default() {
            Caller::Person
        } else {
            let session = self.resolve(selector)?;
            if !session.is_live() {
                return Err(IpcError::new(
                    codes::NO_SESSION,
                    "the calling session is not live",
                ));
            }
            Caller::Session(Box::new(session))
        };
        let paths = Paths::new(&self.home);
        match action {
            TerminalAction::Open {
                command,
                cwd,
                rows,
                cols,
            } => {
                self.open_terminal(&paths, &caller, command, cwd, rows, cols)
                    .await
            }
            TerminalAction::List => {
                let mut terminals = Vec::new();
                for (id, record, state) in self.terminals(&paths)? {
                    if !caller.owns(&record) {
                        continue;
                    }
                    terminals.push(TerminalRow {
                        owner: record.owner.as_deref().map(|id| self.session_label(id)),
                        command: record.command,
                        cwd: record.cwd,
                        opened_at: record.opened_at,
                        state: match state {
                            State::Exited(status) => format!("exited ({status})"),
                            State::Running => "running".to_owned(),
                            State::Starting | State::Ended => "starting".to_owned(),
                        },
                        attached: host::viewer_activity(&paths, &id)
                            .is_some_and(|viewer| viewer.attached),
                        id,
                    });
                }
                Ok(TerminalReply::List { terminals })
            }
            TerminalAction::Screen {
                target: TerminalTarget::Id(id),
                scrollback,
            } => {
                let mine = self.scratch_of(&paths, &caller, &id)?;
                let look = Look::open(&paths, &id).await?;
                Ok(TerminalReply::Screen(self.scratch_screen(
                    &mine,
                    &look,
                    usize::from(scrollback),
                )))
            }
            TerminalAction::Screen {
                target: TerminalTarget::Session(alias),
                scrollback,
            } => self
                .session_screen(&paths, &caller, &alias, usize::from(scrollback))
                .await
                .map(TerminalReply::Screen),
            TerminalAction::Type { id, text } => {
                let mine = self.scratch_of(&paths, &caller, &id)?;
                if text.len() > scratch::MAX_TEXT {
                    return Err(IpcError::bad_request(format!(
                        "one `type` takes at most {} bytes; send the text in parts",
                        scratch::MAX_TEXT
                    )));
                }
                self.send_input(&mine, Control::Text(text)).await
            }
            TerminalAction::Key { id, keys } => {
                let mine = self.scratch_of(&paths, &caller, &id)?;
                host::keys::parse(&keys).map_err(IpcError::bad_request)?;
                self.send_input(&mine, Control::Keys(keys)).await
            }
            TerminalAction::Resize { id, rows, cols } => {
                let mine = self.scratch_of(&paths, &caller, &id)?;
                check_size(rows, cols)?;
                self.send_input(&mine, Control::Resize { rows, cols }).await
            }
            TerminalAction::Wait {
                id,
                text,
                still_ms,
                timeout_ms,
            } => {
                let mine = self.scratch_of(&paths, &caller, &id)?;
                let until = match (text, still_ms) {
                    (Some(text), None) if !text.is_empty() => Until::Text(text),
                    (None, Some(still)) => Until::Still(Duration::from_millis(still)),
                    (None, None) => Until::Changed,
                    (Some(_), None) => {
                        return Err(IpcError::bad_request("the text to wait for is empty"));
                    }
                    (Some(_), Some(_)) => {
                        return Err(IpcError::bad_request(
                            "wait for a text or for stillness, not both",
                        ));
                    }
                };
                let timeout = Duration::from_millis(timeout_ms).min(MAX_WAIT);
                self.wait_terminal(&mine, until, timeout)
                    .await
                    .map(TerminalReply::Screen)
            }
            TerminalAction::Close { id } => {
                // A reservation whose host never started is closed too.
                if !self
                    .scratch_record(&paths, &id)?
                    .is_some_and(|record| caller.owns(&record))
                {
                    return Err(unknown(&id));
                }
                close(paths, id.clone()).await;
                lock(&self.terminal_use.shown).remove(&id);
                Ok(TerminalReply::Closed { id })
            }
        }
    }

    /// The record of scratch terminal `id`. The host of a session on
    /// record is that session's terminal and never a scratch one, whatever
    /// its directory says.
    fn scratch_record(&self, paths: &Paths, id: &EventId) -> Result<Option<Record>, IpcError> {
        if self.store.session_for_host(id).map_err(internal)?.is_some() {
            return Ok(None);
        }
        Ok(scratch::record(paths, id))
    }

    /// Scratch terminal `id`, if it is `caller`'s and its program has started.
    fn scratch_of<'a>(
        &self,
        paths: &'a Paths,
        caller: &Caller,
        id: &'a EventId,
    ) -> Result<Mine<'a>, IpcError> {
        let record = self
            .scratch_record(paths, id)?
            .filter(|record| caller.owns(record))
            .ok_or_else(|| unknown(id))?;
        match scratch::state(paths, id).map_err(IpcError::internal)? {
            State::Ended => Err(unknown(id)),
            State::Starting => Err(IpcError::bad_request(format!(
                "terminal {id} is still starting"
            ))),
            State::Running | State::Exited(_) => Ok(Mine {
                paths,
                id,
                opener: record.owner.as_deref() == caller.owner(),
            }),
        }
    }

    /// Every scratch terminal that is starting, running or keeping its last
    /// screen. What a killed host left, and a reservation nobody took, are
    /// removed on the way.
    fn terminals(&self, paths: &Paths) -> Result<Vec<(EventId, Record, State)>, IpcError> {
        let mut found = Vec::new();
        for (id, _) in scratch::all(paths).map_err(IpcError::internal)? {
            let Some(record) = self.scratch_record(paths, &id)? else {
                continue;
            };
            let state = scratch::state(paths, &id).map_err(IpcError::internal)?;
            let stale = chrono::Utc::now() - record.opened_at
                > chrono::Duration::from_std(START_GRACE).unwrap_or_default();
            match state {
                State::Ended | State::Starting if stale => {
                    if let Err(error) = scratch::remove(paths, &id) {
                        tracing::warn!(%error, %id, "removing an ended scratch terminal");
                    }
                    lock(&self.terminal_use.shown).remove(&id);
                }
                state => found.push((id, record, state)),
            }
        }
        Ok(found)
    }

    async fn open_terminal(
        &self,
        paths: &Paths,
        caller: &Caller,
        command: Vec<String>,
        cwd: PathBuf,
        rows: u16,
        cols: u16,
    ) -> Result<TerminalReply, IpcError> {
        if command.first().is_none_or(|program| program.is_empty()) {
            return Err(IpcError::bad_request(
                "name the program to run: senate terminal open -- <command>",
            ));
        }
        check_size(rows, cols)?;
        if !cwd.is_absolute() {
            return Err(IpcError::bad_request(
                "the terminal's directory must be absolute",
            ));
        }
        let limit = match caller {
            Caller::Person => PERSON_LIMIT,
            Caller::Session(_) => SESSION_LIMIT,
        };
        let _opening = self.terminal_use.opening.lock().await;
        let mine: Vec<String> = self
            .terminals(paths)?
            .into_iter()
            .filter(|(_, record, _)| record.owner.as_deref() == caller.owner())
            .map(|(id, ..)| id.to_string())
            .collect();
        if mine.len() >= limit {
            return Err(IpcError::bad_request(format!(
                "{limit} scratch terminals are open already, the most allowed at once. \
                 Close one first: {}",
                mine.join(", ")
            )));
        }
        let id = EventId::new();
        let record = Record {
            owner: caller.owner().map(str::to_owned),
            command,
            cwd,
            opened_at: chrono::Utc::now(),
        };
        scratch::reserve(paths, &id, &record).map_err(IpcError::internal)?;
        Ok(TerminalReply::Opened { id })
    }

    /// The session `session_id` as a command names it.
    fn session_label(&self, session_id: &str) -> String {
        let name = self.shown_session_name(session_id);
        let committee = self
            .store
            .session(session_id)
            .ok()
            .flatten()
            .and_then(|session| session.committee().cloned());
        match committee {
            Some(committee) if !name.contains("::") => format!("{committee}::{name}"),
            _ => name,
        }
    }

    /// The scratch terminal's screen as `look` has it.
    fn scratch_screen(&self, mine: &Mine<'_>, look: &Look, scrollback: usize) -> TerminalScreen {
        let shown = look.shown(scrollback);
        if mine.opener {
            lock(&self.terminal_use.shown).insert(mine.id.clone(), fingerprint(&shown));
        }
        TerminalScreen {
            id: Some(mine.id.clone()),
            exited: scratch::exit_status(mine.paths, mine.id),
            attached: host::viewer_activity(mine.paths, mine.id)
                .is_some_and(|viewer| viewer.attached),
            ..screen(shown)
        }
    }

    /// Type, press or resize, and return the screen once it settles.
    async fn send_input(
        &self,
        mine: &Mine<'_>,
        control: Control,
    ) -> Result<TerminalReply, IpcError> {
        let (paths, id) = (mine.paths, mine.id);
        // Looking first: what the program answers is not missed.
        let mut look = Look::open(paths, id).await?;
        match scratch::send(paths, id, control).await? {
            Sent::Done => {}
            Sent::Held => return Err(held(id)),
            Sent::Exited => {
                return Err(IpcError::bad_request(format!(
                    "the program in terminal {id} has exited ({}), so nothing was typed. \
                     `screen` still shows what it left; `close` the terminal when done.",
                    scratch::exit_status(paths, id).unwrap_or_else(|| "unknown".to_owned())
                )));
            }
        }
        if look.next(Clock::now() + REACT).await == Seen::Changed {
            look.settle(STILL, SETTLE_LIMIT).await;
        }
        Ok(TerminalReply::Screen(self.scratch_screen(mine, &look, 0)))
    }

    async fn wait_terminal(
        &self,
        mine: &Mine<'_>,
        until: Until,
        timeout: Duration,
    ) -> Result<TerminalScreen, IpcError> {
        let (paths, id) = (mine.paths, mine.id);
        let mut look = Look::open(paths, id).await?;
        let began = Clock::now();
        let deadline = began + timeout;
        let mut last_change = began;
        // "Anything else" than the opener last saw; than what is there now
        // for a reader who was shown nothing.
        let seen_before = lock(&self.terminal_use.shown)
            .get(id)
            .copied()
            .filter(|_| mine.opener);
        let baseline = seen_before.unwrap_or_else(|| fingerprint(&look.shown(0)));
        let timed_out = loop {
            let shown = look.shown(0);
            let met = match &until {
                Until::Text(text) => shown.text().contains(text.as_str()),
                Until::Still(still) => last_change.elapsed() >= *still,
                Until::Changed => fingerprint(&shown) != baseline,
            };
            if met {
                break false;
            }
            if scratch::exit_status(paths, id).is_some() {
                // Nothing more will come: what was still on its way is in.
                look.settle(Duration::from_millis(50), Duration::from_millis(500))
                    .await;
                break false;
            }
            let now = Clock::now();
            if now >= deadline {
                break true;
            }
            let next = match &until {
                Until::Still(still) => last_change + *still,
                _ => now + EXIT_POLL,
            };
            match look.next(next.min(deadline)).await {
                Seen::Changed => last_change = Clock::now(),
                Seen::Quiet => {}
                Seen::Gone => {
                    return Err(IpcError::new(
                        codes::NO_SESSION,
                        format!("terminal {id} was closed while waiting"),
                    ));
                }
            }
        };
        Ok(TerminalScreen {
            timed_out,
            ..self.scratch_screen(mine, &look, 0)
        })
    }

    /// What the hosted session `alias` shows: for the chair of its
    /// committee, and for the person.
    async fn session_screen(
        &self,
        paths: &Paths,
        caller: &Caller,
        alias: &AliasName,
        scrollback: usize,
    ) -> Result<TerminalScreen, IpcError> {
        let chairs = match caller {
            Caller::Person => None,
            Caller::Session(session) => {
                let committee = session.committee().ok_or_else(|| {
                    IpcError::new(
                        codes::NOT_CHAIRING,
                        "only the chair of a committee may read another session's screen, \
                         and this session is on no committee",
                    )
                })?;
                let chair = tokio::time::timeout(ROLE_TIMEOUT, self.api.chair(committee))
                    .await
                    .map_err(|_| {
                        IpcError::new(
                            codes::SERVER,
                            "could not verify the committee chair in time",
                        )
                    })?
                    .map_err(server_error)?;
                if chair.chair.as_ref() != Some(&session.agent) {
                    return Err(IpcError::new(
                        codes::NOT_CHAIRING,
                        format!(
                            "only the chair of {committee} may read another session's screen; \
                             this session does not hold the chair"
                        ),
                    ));
                }
                Some(committee.clone())
            }
        };
        let target = self.find_local_senator(alias, chairs.as_ref()).await?;
        if let Some(committee) = &chairs
            && target.committee() != Some(committee)
        {
            return Err(IpcError::new(
                codes::NOT_CHAIRING,
                format!(
                    "{alias} is not on {committee}: a chair reads the screens of its own \
                     committee's sessions only"
                ),
            ));
        }
        let host = self
            .store
            .session_host(&target.session_id)
            .map_err(internal)?
            .ok_or_else(|| {
                IpcError::bad_request(format!(
                    "{alias} runs in a terminal of its own, not one Senate hosts: \
                     there is no screen to read"
                ))
            })?;
        if !host::running(paths, &host).map_err(IpcError::internal)? {
            return Err(IpcError::new(
                codes::NO_SESSION,
                format!("{alias}'s terminal is not running"),
            ));
        }
        let look = Look::open(paths, &host).await?;
        Ok(TerminalScreen {
            of: Some(self.session_label(&target.session_id)),
            attached: host::viewer_activity(paths, &host).is_some_and(|viewer| viewer.attached),
            ..screen(look.shown(scrollback))
        })
    }

    /// Close every scratch terminal `session_id` opened: the session ended.
    pub(super) fn close_terminals_of(&self, session_id: &str) {
        let paths = Paths::new(&self.home);
        let terminals = match self.terminals(&paths) {
            Ok(terminals) => terminals,
            Err(error) => {
                tracing::warn!(error = %error.message, "listing an ended session's scratch terminals");
                return;
            }
        };
        for (id, record, _) in terminals {
            if record.owner.as_deref() == Some(session_id) {
                tracing::info!(%id, session_id, "closing a scratch terminal: its session ended");
                lock(&self.terminal_use.shown).remove(&id);
                tokio::spawn(close(paths.clone(), id));
            }
        }
    }

    /// Close the scratch terminals whose session is no longer live: one
    /// that ended without a hook, or whose process is gone.
    pub(super) fn sweep_terminals(&self, paths: &Paths) -> Result<(), StoreError> {
        let terminals = self
            .terminals(paths)
            .map_err(|error| StoreError::Io(std::io::Error::other(error.message)))?;
        for (id, record, _) in terminals {
            let Some(owner) = &record.owner else {
                continue;
            };
            if self
                .store
                .session(owner)?
                .is_some_and(|session| session.is_live())
            {
                continue;
            }
            tracing::info!(%id, session = %owner, "closing a scratch terminal: its session is gone");
            lock(&self.terminal_use.shown).remove(&id);
            tokio::spawn(close(paths.clone(), id));
        }
        Ok(())
    }
}
