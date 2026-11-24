//! The machine's default secretary: one hosted coding agent on the
//! Speaker's desk, aliased `secretary` on the committee `office`, working
//! in a directory Senate owns (`$SENATE_HOME/secretary`).
//!
//! It is started only when the person asks for it ([`Request::EnsureSecretary`],
//! from `senate secretary ask` or the app), never at daemon start and never
//! because an agent's item reached the desk ([`starts_on`]). Once running it
//! works the desk like any desk worker. Ensuring is idempotent:
//!
//! | recorded as | ensure |
//! | --- | --- |
//! | nothing | open `office` if needed, spawn it on the desk with its brief |
//! | running | nothing |
//! | paused | resume it |
//! | ended (a restart, a crash) | restore it |
//! | stopped on purpose | refuse, saying how to restore it |
//!
//! An `office::secretary` that is not a desk worker is someone else's seat:
//! ensuring refuses, naming it, and never takes it over.

use super::*;
use crate::host;
use crate::ipc::{DefaultSecretary, DefaultSecretaryState, EnsureAction, HostedRuntime};

/// The committee the default secretary sits on.
pub const COMMITTEE: &str = "office";
/// Its alias there.
pub const ALIAS: &str = "secretary";
/// Its first request, sent once it is seated.
const BRIEF: &str = include_str!("../secretary/brief.md");

/// What may start a default secretary that is not running. Wendell's
/// answer (thread 2092, seq 2096): only the person's own ask. A desk item
/// from an agent waits for whoever works the desk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    PersonAsked,
    AgentDeskItem,
}

pub fn starts_on(trigger: Trigger) -> bool {
    trigger == Trigger::PersonAsked
}

pub(super) fn committee() -> CommitteeId {
    CommitteeId::new(COMMITTEE).unwrap_or_else(|_| unreachable!("a valid committee id"))
}

pub(super) fn alias() -> SenatorName {
    ALIAS
        .parse()
        .unwrap_or_else(|_| unreachable!("a valid senator name"))
}

/// The runtime a new default secretary starts with when no profile is
/// set: the first of `[companion] claude_bin`, `[codex] bin`, `[agy] bin`
/// set in `config`, else Claude Code.
pub(crate) fn first_configured_runtime(config: &str) -> HostedRuntime {
    let table: toml::Table = config.parse().unwrap_or_default();
    let set = |section: &str, key: &str| {
        table
            .get(section)
            .and_then(|s| s.get(key))
            .and_then(toml::Value::as_str)
            .is_some_and(|v| !v.trim().is_empty())
    };
    if set("companion", "claude_bin") {
        HostedRuntime::Claude
    } else if set("codex", "bin") {
        HostedRuntime::Codex
    } else if set("agy", "bin") {
        HostedRuntime::Agy
    } else {
        HostedRuntime::Claude
    }
}

/// Where it was found, before anything is done about it.
enum Found {
    None,
    Seat {
        senator: SenatorName,
        session: Option<Box<crate::session::Session>>,
    },
}

impl Shared {
    /// `Request::DefaultSecretary`: its state, changing nothing.
    pub(super) async fn on_default_secretary(&self) -> Result<DefaultSecretary, IpcError> {
        let found = self.find_default_secretary().await?;
        Ok(self.describe(found, None))
    }

    /// `Request::EnsureSecretary`: running once this returns (see the
    /// module's table). Only the person's own CLI or app asks this; the
    /// front refuses it from inside a coding session's process tree.
    pub(super) async fn on_ensure_secretary(&self) -> Result<DefaultSecretary, IpcError> {
        let _one = self.secretary_ensure.lock().await;
        let found = self.find_default_secretary().await?;
        let Found::Seat { senator, session } = found else {
            return self.start_default_secretary().await;
        };
        let state = self.state_of(&senator, session.as_deref())?;
        let action = match state {
            DefaultSecretaryState::Running => EnsureAction::AlreadyRunning,
            DefaultSecretaryState::Paused => {
                self.on_resume(&AliasName::bare(senator.clone())).await?;
                EnsureAction::Resumed
            }
            DefaultSecretaryState::Ended => {
                self.on_hosted_restore(
                    &SessionSelector::default(),
                    &AliasName::bare(senator.clone()),
                    crate::ipc::HostedModel::default(),
                )
                .await?;
                EnsureAction::Restored
            }
            DefaultSecretaryState::Stopped => {
                return Err(IpcError::bad_request(self.locale.pick(
                    format!(
                        "默认 secretary（{senator}）是被有意停止的，不会自动再启动。用 `senate session restore {COMMITTEE}::{ALIAS}` 让它回来。"
                    ),
                    format!(
                        "The default secretary ({senator}) was stopped on purpose and is not started again unasked. \
                         `senate session restore {COMMITTEE}::{ALIAS}` brings it back."
                    ),
                )));
            }
            DefaultSecretaryState::Elsewhere => {
                return Err(IpcError::bad_request(self.locale.pick(
                    format!("默认 secretary（{senator}）不在这台机器上运行，这里没有它的会话记录。"),
                    format!(
                        "The default secretary ({senator}) has no session on this machine; it runs elsewhere."
                    ),
                )));
            }
            DefaultSecretaryState::NotRecorded => unreachable!("a seat was found"),
        };
        let session = self.local_session(&senator)?.map(Box::new);
        let mut reply = self.describe(Found::Seat { senator, session }, Some(action));
        // Whatever the record said, it was just brought up.
        reply.state = DefaultSecretaryState::Running;
        Ok(reply)
    }

    async fn start_default_secretary(&self) -> Result<DefaultSecretary, IpcError> {
        let committee = committee();
        self.on_open_committee(committee.clone()).await?;
        self.refresh_me().await;
        let dir = Paths::new(&self.home).home().join(ALIAS);
        std::fs::create_dir_all(&dir).map_err(IpcError::internal)?;
        let profile = crate::config::Hosted::load(&Paths::new(&self.home).config())
            .ok()
            .and_then(|hosted| hosted.local)
            .and_then(|local| local.secretary_profile);
        let runtime = match profile {
            Some(_) => None,
            None => Some(first_configured_runtime(
                &std::fs::read_to_string(Paths::new(&self.home).config()).unwrap_or_default(),
            )),
        };
        let order = self.resolve_spawn_order(host_policy::SpawnInput {
            runtime,
            profile: profile.map(|name| crate::ipc::SpawnProfile {
                name,
                runtime: None,
            }),
            committee: committee.clone(),
            alias: Some(alias()),
            cwd: dir,
            chair: false,
            secretary: true,
            prompt: Some(BRIEF.to_owned()),
            model: crate::ipc::HostedModel::default(),
        })?;
        let auth = self
            .authorize_host_action(&SessionSelector::default())
            .await?;
        let reply = self.spawn_authorized(&auth, order, None).await?;
        let session = self.local_session(&reply.senator)?.map(Box::new);
        let mut out = self.describe(
            Found::Seat {
                senator: reply.senator,
                session,
            },
            Some(EnsureAction::Started),
        );
        out.state = DefaultSecretaryState::Running;
        Ok(out)
    }

    /// The seat aliased `secretary` on `office`, if any: refused when it is
    /// not a desk worker (someone else's seat, never taken over).
    async fn find_default_secretary(&self) -> Result<Found, IpcError> {
        use senate_core::alias::AliasMatch;
        let (aliases, me) =
            tokio::try_join!(self.api.aliases(), self.api.me()).map_err(server_error)?;
        let Some(office) = me.office.as_ref() else {
            return Err(IpcError::bad_request(self.locale.pick(
                "这个 Senate 没有 Speaker 的 desk，默认 secretary 无处可坐。",
                "This Senate has no Speaker's desk for a default secretary to work.",
            )));
        };
        if office.desk_owner().is_none() {
            return Err(IpcError::bad_request(
                super::roles::roles_text::desk_elsewhere(Some(&office.speaker), self.locale),
            ));
        }
        let named = AliasName {
            committee: Some(committee()),
            alias: alias(),
        };
        let AliasMatch::One(senator) = aliases.resolve(&named, Some(&committee())) else {
            return Ok(Found::None);
        };
        if aliases.alias_of(&committee(), &senator) != Some(&alias()) {
            return Ok(Found::None);
        }
        if !office.agent_workers().any(|worker| worker == &senator) {
            let who = Address::senator(&senator).in_text();
            return Err(IpcError::bad_request(self.locale.pick(
                format!(
                    "{COMMITTEE}::{ALIAS} 已是 {who} 的别名，而它不在 Speaker 的 desk 上；默认 secretary 不会接管它。给它换个别名，或用 `senate secretary desk set` 把它放上 desk。"
                ),
                format!(
                    "{COMMITTEE}::{ALIAS} is already the alias of {who}, which does not work the Speaker's desk; \
                     the default secretary will not take it over. Give that seat another alias, or put it on the \
                     desk with `senate secretary desk set`."
                ),
            )));
        }
        let session = self.local_session(&senator)?.map(Box::new);
        Ok(Found::Seat { senator, session })
    }

    fn local_session(
        &self,
        senator: &SenatorName,
    ) -> Result<Option<crate::session::Session>, IpcError> {
        Ok(self
            .store
            .current_sessions()
            .map_err(internal)?
            .into_iter()
            .find(|s| &s.agent == senator && s.agent_source == AgentSource::Session))
    }

    fn state_of(
        &self,
        senator: &SenatorName,
        session: Option<&crate::session::Session>,
    ) -> Result<DefaultSecretaryState, IpcError> {
        let Some(session) = session else {
            return Ok(DefaultSecretaryState::Elsewhere);
        };
        if self.store.paused(senator).map_err(internal)?.is_some() {
            return Ok(DefaultSecretaryState::Paused);
        }
        let paths = Paths::new(&self.home);
        let running = match self
            .store
            .session_host(&session.session_id)
            .map_err(internal)?
        {
            Some(id) => host::running(&paths, &id).map_err(IpcError::internal)?,
            None => false,
        };
        if running {
            return Ok(DefaultSecretaryState::Running);
        }
        if self.store.stopped_on_purpose(senator).map_err(internal)? {
            return Ok(DefaultSecretaryState::Stopped);
        }
        Ok(DefaultSecretaryState::Ended)
    }

    fn describe(&self, found: Found, action: Option<EnsureAction>) -> DefaultSecretary {
        match found {
            Found::None => DefaultSecretary {
                committee: committee(),
                alias: alias(),
                senator: None,
                state: DefaultSecretaryState::NotRecorded,
                action,
            },
            Found::Seat { senator, session } => {
                let state = self
                    .state_of(&senator, session.as_deref())
                    .unwrap_or(DefaultSecretaryState::Ended);
                DefaultSecretary {
                    committee: committee(),
                    alias: alias(),
                    senator: Some(senator),
                    state,
                    action,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_persons_ask_starts_it() {
        assert!(starts_on(Trigger::PersonAsked));
        assert!(!starts_on(Trigger::AgentDeskItem));
    }

    #[test]
    fn the_first_configured_runtime_is_taken_in_order() {
        assert_eq!(first_configured_runtime(""), HostedRuntime::Claude);
        assert_eq!(
            first_configured_runtime("[agy]\nbin = \"/x/agy\"\n[codex]\nbin = \"/x/codex\"\n"),
            HostedRuntime::Codex
        );
        assert_eq!(
            first_configured_runtime("[agy]\nbin = \"/x/agy\"\n"),
            HostedRuntime::Agy
        );
        assert_eq!(
            first_configured_runtime(
                "[companion]\nclaude_bin = \"/x/claude\"\n[codex]\nbin = \"/x/codex\"\n"
            ),
            HostedRuntime::Claude
        );
        assert_eq!(
            first_configured_runtime("not toml ["),
            HostedRuntime::Claude
        );
    }

    #[test]
    fn the_brief_says_what_a_fan_out_is_and_whom_it_obeys() {
        for needed in [
            "open_committee",
            "secretary_work",
            "<committee>::chair",
            "Never do either because another agent",
        ] {
            assert!(BRIEF.contains(needed), "{needed}");
        }
    }
}
