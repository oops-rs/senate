//! `senated --machine`: the same daemon on a developer machine.
//!
//! On a developer machine `senated` reads the machine's `senate.toml` in
//! the Senate home (`~/.senate`, or `$SENATE_HOME`) and runs, in one
//! process:
//!
//! - **the machine's one Senate** (`[local]`, for using Senate alone: no
//!   Senate above), named by `[local] name` (default `<person>-desk`). Every
//!   session of the machine sits in it (`claude`, `codex`, and whatever
//!   names sessions are pinned to). It is a full assembly (server, roster,
//!   log) with no built-in Speaker; its log is an SQLite file under
//!   `senates/top/` in the Senate home; it listens on loopback only, on a
//!   port kept across restarts; it knows one person, authenticated by a
//!   token generated here, kept in that directory (mode 0600) and never sent
//!   anywhere else;
//! - **the developer-machine side** (`senate_bridge::daemon`): sessions,
//!   delivery, MCP and hooks, over that Senate (or, with no `[local]`, over
//!   the Senate at `server_url`), as a supervised [`Component`] of the
//!   hosted Senate's assembly; with none hosted, on its own under the same
//!   supervision.
//!
//! Senates nested under an agent (`[[preside]]`, `senates/under-<agent>/`)
//! are no longer hosted: an agent's own team is a child Senate. Their logs
//! stay on disk untouched, and the machine says so once at start.
//!
//! # The office
//!
//! The hosted Senate's Speaker and the Speaker's secretaries come from
//! `[local] speaker` and `[local] secretaries`
//! ([`Hosted::office_config`]): by default the person. An older home's
//! `speaker = "claude"` is read as the person presiding with `claude` as
//! the first secretary, and said once, in the machine's language (a marker,
//! `OFFICE-NOTE`, in the hosted Senate's directory remembers it was said).
//!
//! # A machine under a parent
//!
//! `[local]` together with `server_url` and `token`: the machine hosts its
//! Senate as above and registers it with the Senate at `server_url` as a
//! child ([`uplink`]). At start it asks that Senate's `GET /v1/me` (a short
//! timeout) for its path, the hosted Senate's `parent`; a Senate above that
//! does not answer leaves the hosted Senate with no parent (said at start;
//! the next start fixes it). The Speaker is then the person as a senator of
//! the Senate above (`/<parent>::senator::<person>`); a parent's agent as
//! the Speaker is refused at start ("not supported yet"). The
//! developer-machine side serves the hosted Senate and is given the Senate
//! above as its uplink ([`BridgeConfig::uplink`]).
//!
//! Directory layout under the Senate home:
//!
//! | Path | What |
//! | --- | --- |
//! | `senate.toml` | the machine's configuration |
//! | `senates/top/` | the machine's Senate (`[local]`) |
//! | `senates/top/log.db` | its log |
//! | `senates/top/token` | the person's token there, generated here |
//! | `senates/top/listen` | the loopback address it serves, kept for restarts |
//! | `senates/top/OFFICE-NOTE` | the compatibility note on `[local] speaker`, said once |
//! | `senates/top/uplink.json` | the registration with the Senate above, for `senate status` |

pub mod uplink;

use std::io::Write;
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use senate_bridge::config::{Hosted, OfficeNote, Uplink};
use senate_bridge::daemon::{self, DaemonOptions, Timing};
use senate_bridge::paths::HOSTED_KEY;
use senate_bridge::{BridgeConfig, Paths};
use senate_core::{
    EventId, Locale, Name, Principal, RosterConfig, SenatePath, SenatorName, SpeakerName,
};
use senate_server::{ServerConfig, TokenConfig};

use crate::assembly::{Assembly, AssemblyError, Senated};
use crate::component::{Component, ComponentContext, GaveUp, Supervisor};
use crate::config::SenatedConfig;

#[derive(Debug, thiserror::Error)]
pub enum MachineError {
    #[error(transparent)]
    Config(#[from] senate_bridge::config::ConfigError),
    #[error("{0}")]
    Invalid(String),
    #[error("{path}: {source}")]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("the Senate in {key}: {source}")]
    Senate {
        key: String,
        #[source]
        source: AssemblyError,
    },
}

/// The Senate this machine hosts, running.
pub struct HostedSenate {
    /// Its directory under `senates/`.
    pub key: String,
    /// Its name.
    pub name: Name,
    /// `http://127.0.0.1:<port>`.
    pub url: String,
    /// The person's token there.
    pub token: String,
    pub daemon: Senated,
}

/// A developer machine's `senated`: the Senate it hosts and its side of the
/// Senate it serves.
pub struct Machine {
    senates: Vec<HostedSenate>,
    /// The developer-machine side when no hosted Senate carries it.
    alone: Option<Supervisor>,
    bridge: BridgeConfig,
    /// The hosted Senate's link to the Senate above, when it is a child.
    uplink: Option<uplink::Uplink>,
}

/// What the hosted Senate is, before it starts.
struct Plan {
    key: String,
    name: Name,
    /// Its Speaker, a senator id.
    speaker: SpeakerName,
    /// The Speaker's secretaries, in order.
    secretaries: Vec<SenatorName>,
    /// The Senate above, when it is a child and the parent answered.
    parent: Option<SenatePath>,
    listen: String,
    token: String,
    dir: std::path::PathBuf,
}

/// How long the machine waits for the Senate above's `/v1/me` at start.
pub const PARENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

impl Machine {
    /// Read `senate.toml` in `paths`' home and start.
    pub async fn start(paths: Paths, timing: Timing) -> Result<Self, MachineError> {
        let path = paths.config();
        let config = BridgeConfig::load_unchecked(&path)?;
        let hosted = Hosted::load(&path)?;
        Self::start_with(paths, config, hosted, timing).await
    }

    /// [`Machine::start`], posting desktop notifications through `desktop`
    /// for what is addressed to the person (`[notify] desktop = false`
    /// turns them off). `start` posts none: only the real daemon should
    /// reach the desktop.
    pub async fn start_desktop(
        paths: Paths,
        timing: Timing,
        desktop: senate_bridge::notify::Desktop,
    ) -> Result<Self, MachineError> {
        let path = paths.config();
        let config = BridgeConfig::load_unchecked(&path)?;
        let hosted = Hosted::load(&path)?;
        Self::start_full(paths, config, hosted, timing, Some(desktop)).await
    }

    /// Start from a configuration already read: the hosted Senates first,
    /// then the developer-machine side over all of them.
    pub async fn start_with(
        paths: Paths,
        config: BridgeConfig,
        hosted: Hosted,
        timing: Timing,
    ) -> Result<Self, MachineError> {
        Self::start_full(paths, config, hosted, timing, None).await
    }

    async fn start_full(
        paths: Paths,
        config: BridgeConfig,
        hosted: Hosted,
        timing: Timing,
        desktop: Option<senate_bridge::notify::Desktop>,
    ) -> Result<Self, MachineError> {
        hosted.validate(&config).map_err(MachineError::Invalid)?;
        // Before anything is hosted or spawned: a socket path that cannot
        // bind would otherwise fail inside the supervised developer-machine
        // side and be started again forever.
        paths
            .check_socket(config.locale)
            .map_err(|e| MachineError::Invalid(e.to_string()))?;
        paths.ensure_home().map_err(|source| MachineError::Io {
            path: paths.home().to_path_buf(),
            source,
        })?;
        if !hosted.preside.is_empty() {
            notice_nested(&paths, &hosted, config.locale);
        }
        let locale = config.locale;
        let under = hosted.under_a_parent(&config);
        let mut plans = Vec::new();
        if let Some(local) = &hosted.local {
            // Under a parent: who the Senate above is, before the office
            // can be named.
            let above = if under {
                parent_me(&config).await
            } else {
                None
            };
            let parent = above.as_ref().map(senate_core::api::Me::senate_path);
            let (speaker, secretaries, note) = hosted
                .office_config(&config.person, parent.as_ref())
                .map_err(|e| MachineError::Invalid(e.text(locale)))?;
            let own_agents = [config.agents.claude.clone(), config.agents.codex.clone()];
            senate_bridge::config::check_speaker_above(
                &speaker.id,
                &config.person,
                above.as_ref().map_or(&[][..], |me| me.agents.as_slice()),
                &own_agents,
            )
            .map_err(|e| MachineError::Invalid(e.text(locale)))?;
            let p = plan(
                &paths,
                HOSTED_KEY.into(),
                hosted.name(&config.person),
                (speaker, secretaries, parent),
                local.listen.as_deref(),
            )?;
            if let Some(note) = note {
                notice_office(&p.dir, &note, locale);
            }
            plans.push(p);
        }

        // The developer-machine side serves the Senate this machine hosts,
        // or the one at `server_url`; under a parent, the Senate above is
        // its uplink.
        let mut bridge = config.clone();
        if let Some(p) = plans.first() {
            bridge.server_url = format!("http://{}", p.listen);
            bridge.token = p.token.clone();
            if under {
                bridge.uplink = Some(Uplink {
                    url: config.server_url.clone(),
                    token: config.token.clone(),
                });
            }
        }
        let uplink_plan = plans
            .first()
            .filter(|_| under)
            .map(|p| uplink::UplinkConfig {
                url: config.server_url.clone(),
                token: config.token.clone(),
                name: p.name.clone(),
                speaker: p.speaker.id.name().clone(),
                parent: p.parent.clone(),
                dir: p.dir.clone(),
                poll: uplink::POLL.min(timing.sweep),
                locale,
            });
        bridge.validate().map_err(MachineError::Invalid)?;
        let side = Arc::new(DeveloperMachine {
            options: DaemonOptions {
                paths: paths.clone(),
                config: bridge.clone(),
                timing,
                desktop,
            },
        });

        // The first hosted Senate carries the developer-machine side; it
        // starts last, so every Senate it serves is up when it starts.
        let mut senates = Vec::with_capacity(plans.len());
        let mut stores = Vec::with_capacity(plans.len());
        let carrier = plans.first().map(|p| p.key.clone());
        let mut ordered: Vec<Plan> = plans;
        if !ordered.is_empty() {
            ordered.rotate_left(1);
        }
        for p in ordered {
            let carries = Some(&p.key) == carrier.as_ref();
            let config = senate_config(&p, &config)?;
            // TODO(built-in session): the built-in session (the engine, as
            // a secretary or the Speaker of an unattended root) runs here
            // when a `[model]` is configured. Its configuration and its
            // place in the assembly are being built in `crate::speaker` /
            // `crate::assembly`; wire it through `built_in_session` below
            // once that lands. Until then the machine runs none.
            // The log is opened here so the uplink keeps its cursors in
            // the same store's registry.
            let store = senate_log::connect(&config.server.database_url)
                .await
                .map_err(|e| MachineError::Invalid(format!("the Senate {}: {e}", p.name)))?;
            stores.push(store.clone());
            let mut assembly = match built_in_session(&paths, &p) {
                Some(()) => Assembly::new(config),
                None => Assembly::new(config).without_speaker(),
            }
            .store(store);
            if carries {
                assembly = assembly.component_arc(side.clone());
            }
            let daemon = assembly
                .start()
                .await
                .map_err(|source| MachineError::Senate {
                    key: p.key.clone(),
                    source,
                })?;
            tracing::info!(
                senate = %daemon.hub.senate_path(),
                dir = %p.key,
                url = %daemon.base_url(),
                speaker = %p.speaker,
                "hosting this machine's Senate"
            );
            senates.push(HostedSenate {
                url: daemon.base_url(),
                key: p.key,
                name: p.name,
                token: p.token,
                daemon,
            });
        }

        let alone = senates
            .is_empty()
            .then(|| Supervisor::start(crate::empty_hub::empty(), vec![side]));
        let uplink = match (uplink_plan, senates.first(), stores.first()) {
            (Some(plan), Some(top), Some(store)) => Some(
                uplink::Uplink::spawn(top.daemon.hub.clone(), store.clone(), plan)
                    .map_err(|e| MachineError::Invalid(e.to_string()))?,
            ),
            _ => None,
        };
        Ok(Self {
            senates,
            alone,
            bridge,
            uplink,
        })
    }

    /// The Senate this machine hosts, when it hosts one (`[local]`).
    pub fn senate(&self) -> Option<&HostedSenate> {
        self.senates.first()
    }

    /// The Senates this machine hosts: none or one.
    pub fn senates(&self) -> &[HostedSenate] {
        &self.senates
    }

    /// What the developer-machine side was started with: the Senate it
    /// serves, its address and token.
    pub fn bridge_config(&self) -> &BridgeConfig {
        &self.bridge
    }

    /// Resolves when supervision gave up on a component (the
    /// developer-machine side, say); never otherwise.
    pub async fn failed(&self) -> GaveUp {
        let mut waits: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = GaveUp> + '_>>> =
            self.senates
                .iter()
                .map(|s| Box::pin(s.daemon.failed()) as _)
                .collect();
        if let Some(alone) = &self.alone {
            let mut gave_up = alone.gave_up();
            waits.push(Box::pin(async move {
                match gave_up.wait_for(Option::is_some).await {
                    Ok(why) => why
                        .clone()
                        .unwrap_or_else(|| unreachable!("waited for Some")),
                    Err(_) => std::future::pending().await,
                }
            }));
        }
        if waits.is_empty() {
            return std::future::pending().await;
        }
        futures::future::select_all(waits).await.0
    }

    /// Stop the uplink, the developer-machine side, then every hosted
    /// Senate.
    pub async fn shutdown(mut self) {
        if let Some(uplink) = self.uplink.take() {
            uplink.shutdown().await;
        }
        if let Some(alone) = self.alone.take() {
            alone.stop().await;
        }
        // The carrier first: it stops the developer-machine side.
        for senate in self.senates {
            if let Err(e) = senate.daemon.shutdown().await {
                tracing::warn!(senate = %senate.key, "stopping a hosted Senate: {e}");
            }
        }
    }

    /// Serve until SIGINT or SIGTERM, or until a component kept failing.
    pub async fn run_until_signal(self) -> anyhow::Result<()> {
        let failed = tokio::select! {
            signal = crate::assembly::shutdown_signal() => {
                signal?;
                None
            }
            why = self.failed() => Some(why),
        };
        tracing::info!("shutting down");
        self.shutdown().await;
        match failed {
            Some(why) => Err(anyhow::anyhow!("{why}")),
            None => Ok(()),
        }
    }
}

/// Where one hosted Senate lives and how it is reached: its directory, its
/// token (generated once), and its address (configured, or the one it had,
/// or a free loopback port, kept for the next start so the
/// developer-machine side finds the same Senate at the same address).
fn plan(
    paths: &Paths,
    key: String,
    name: Name,
    (speaker, secretaries, parent): (SpeakerName, Vec<SenatorName>, Option<SenatePath>),
    listen: Option<&str>,
) -> Result<Plan, MachineError> {
    let dir = paths.hosted(&key);
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| MachineError::Io { path, source }
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(io(&dir))?;
    let token_file = dir.join("token");
    let token = match std::fs::read_to_string(&token_file) {
        Ok(token) if !token.trim().is_empty() => token.trim().to_owned(),
        _ => {
            // Two ULIDs: 160 random bits, never written anywhere else.
            let token = format!("local-{}{}", EventId::new(), EventId::new()).to_lowercase();
            write_private(&token_file, &token).map_err(io(&token_file))?;
            token
        }
    };
    let listen_file = dir.join("listen");
    let listen = match listen {
        Some(listen) => listen.to_owned(),
        None => {
            let kept = std::fs::read_to_string(&listen_file)
                .ok()
                .map(|l| l.trim().to_owned())
                .filter(|l| {
                    l.parse::<SocketAddr>()
                        .is_ok_and(|a| a.ip().is_loopback() && TcpListener::bind(a).is_ok())
                });
            match kept {
                Some(listen) => listen,
                None => {
                    let free = TcpListener::bind("127.0.0.1:0")
                        .and_then(|l| l.local_addr())
                        .map_err(io(&dir))?;
                    free.to_string()
                }
            }
        }
    };
    senate_bridge::config::check_loopback(&listen).map_err(MachineError::Invalid)?;
    write_private(&listen_file, &listen).map_err(io(&listen_file))?;
    Ok(Plan {
        key,
        name,
        speaker,
        secretaries,
        parent,
        listen,
        token,
        dir,
    })
}

/// The Senate above's `/v1/me`, as the person there, bounded by
/// [`PARENT_TIMEOUT`]. `None`, said once in the machine's language, when it
/// does not answer: the machine's Senate starts with no parent.
async fn parent_me(config: &BridgeConfig) -> Option<senate_core::api::Me> {
    let client = senate_bridge::client::ApiClient::new(&config.server_url, &config.token).ok()?;
    let error = match tokio::time::timeout(PARENT_TIMEOUT, client.me()).await {
        Ok(Ok(me)) => return Some(me),
        Ok(Err(e)) => e.to_string(),
        Err(_) => "timed out".to_owned(),
    };
    let text = parent_unreachable(&config.server_url, &error, config.locale);
    tracing::warn!("{text}");
    eprintln!("{text}");
    None
}

/// The Senate above did not answer at start.
pub fn parent_unreachable(url: &str, error: &str, locale: Locale) -> String {
    match locale {
        Locale::Zh => format!(
            "上一级 Senate（{url}）启动时没有应答（{error}）：这台机器的 Senate 先以没有上一级的方式启动，\
             连上后再注册为子 Senate；下次启动时补上它的上一级。"
        ),
        Locale::En => format!(
            "The Senate above ({url}) did not answer at start ({error}): this machine's Senate starts \
             with no parent and registers as a child once it answers; the next start sets its parent."
        ),
    }
}

/// The built-in session for the hosted Senate, when one should run.
///
/// TODO(built-in session): returns what the assembly needs to run the
/// built-in session (its configuration from `[model]` and
/// `secretary.toml` in the Senate home, `paths.home()`), from the function
/// the `crate::speaker` / `crate::assembly` work provides, when `[model]`
/// is configured and the session is referenced (named Speaker in `plan`,
/// or listed in its secretaries). `None` until then.
fn built_in_session(paths: &Paths, plan: &Plan) -> Option<()> {
    let _ = (paths, plan);
    None
}

/// Say once, in the machine's language, how `[local] speaker` was read.
/// "Once": a marker in the hosted Senate's directory keeps what was said
/// (in English); a different note is said again.
fn notice_office(dir: &Path, note: &OfficeNote, locale: Locale) {
    let marker = dir.join("OFFICE-NOTE");
    let key = format!("{}\n", note.text(Locale::En));
    if std::fs::read_to_string(&marker).is_ok_and(|said| said == key) {
        return;
    }
    let text = note.text(locale);
    tracing::warn!("{text}");
    eprintln!("{text}");
    if let Err(e) = std::fs::write(&marker, key) {
        tracing::debug!("{}: {e}", marker.display());
    }
}

fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    writeln!(file, "{text}")
}

/// The hosted Senate's `senated` configuration: its name, one person, its
/// token, its named Speaker, SQLite under its directory, loopback.
fn senate_config(plan: &Plan, machine: &BridgeConfig) -> Result<SenatedConfig, MachineError> {
    let mut secretaries = std::collections::BTreeMap::new();
    if !plan.secretaries.is_empty() {
        secretaries.insert(plan.speaker.id.name().clone(), plan.secretaries.clone());
    }
    let roster = RosterConfig {
        speaker: Some(plan.speaker.clone()),
        secretaries,
        principals: vec![Principal::new(machine.person.clone())],
        ..RosterConfig::default()
    };
    let mut server = ServerConfig::new(
        roster,
        vec![TokenConfig::with_token(
            machine.person.clone(),
            plan.token.clone(),
        )],
    );
    server.name = Some(plan.name.clone());
    server.parent = plan.parent.clone();
    server.listen = plan.listen.clone();
    server.database_url = format!("sqlite:{}", plan.dir.join("log.db").display());
    server.data_dir = plan.dir.join("data");
    server.locale = machine.locale;
    server
        .validate()
        .map_err(|e| MachineError::Invalid(format!("the Senate {}: {e}", plan.name)))?;
    std::fs::create_dir_all(&server.data_dir).map_err(|source| MachineError::Io {
        path: server.data_dir.clone(),
        source,
    })?;
    Ok(SenatedConfig {
        server,
        speaker: None,
        work: None,
    })
}

/// Say once, in the machine's language, that Senates nested under an agent
/// are no longer hosted and where their logs are. Their directories are
/// left as they are. "Once": a marker in each directory remembers it was
/// said.
fn notice_nested(paths: &Paths, hosted: &Hosted, locale: Locale) {
    for preside in &hosted.preside {
        let key = format!("under-{}", preside.agent.replace('/', "-"));
        let dir = paths.hosted(&key);
        let marker = dir.join("NOT-HOSTED");
        if marker.exists() {
            continue;
        }
        let text = nested_notice(&preside.agent, &dir, locale);
        tracing::warn!("{text}");
        eprintln!("{text}");
        if dir.is_dir()
            && let Err(e) = std::fs::write(&marker, format!("{text}\n"))
        {
            tracing::debug!("{}: {e}", marker.display());
        }
    }
}

/// The notice for one nested Senate.
pub fn nested_notice(agent: &str, dir: &Path, locale: Locale) -> String {
    let dir = dir.display();
    match locale {
        Locale::Zh => format!(
            "[[preside]] agent = \"{agent}\"：嵌套在 agent 之下的 Senate 不再托管。一个 agent \
             自己的团队是一个子 Senate。它的日志原样留在 {dir}，不会被改动；不再需要时用 \
             `senate free --yes --delete-logs` 删除。把 [[preside]] 从 senate.toml 去掉，这条提示就不再出现。"
        ),
        Locale::En => format!(
            "[[preside]] agent = \"{agent}\": Senates nested under an agent are no longer hosted. \
             An agent's own team is a child Senate. Its log stays untouched in {dir}; remove it \
             with `senate free --yes --delete-logs` when it is no longer needed. Drop [[preside]] \
             from senate.toml and this notice stops."
        ),
    }
}

/// The developer-machine side as a supervised component: the bridge
/// daemon over every Senate the machine serves. A start that fails (its
/// socket taken by another daemon, say) is a failure, retried under the
/// assembly's supervision.
struct DeveloperMachine {
    options: DaemonOptions,
}

#[async_trait]
impl Component for DeveloperMachine {
    fn name(&self) -> &str {
        "developer-machine"
    }

    async fn run(&self, ctx: ComponentContext) -> anyhow::Result<()> {
        let handle = daemon::start(self.options.clone()).await?;
        ctx.shutdown.cancelled().await;
        handle.shutdown().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use senate_bridge::client::ApiClient;
    use senate_core::{Hub, Name, SenatorName, SpeakerSeat};

    use super::*;

    fn quiet() -> Timing {
        Timing::default()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn alone_the_machine_hosts_one_senate_with_its_named_speaker_under_the_person() {
        // An agent as the Speaker of a root, named in full: today's alone.
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("senate"));
        let config: BridgeConfig = toml::from_str("person = \"alice\"\n").unwrap();
        let hosted: Hosted =
            toml::from_str("[local]\nname = \"north-desk\"\nspeaker = \"senator::claude\"\n")
                .unwrap();
        let machine = Machine::start_with(paths.clone(), config, hosted, quiet())
            .await
            .unwrap();
        assert_eq!(machine.senates().len(), 1, "one Senate per machine");
        let top = machine.senate().unwrap();
        assert_eq!(top.key, "top");
        assert_eq!(top.name.as_str(), "north-desk");
        assert!(top.url.starts_with("http://127.0.0.1:"));
        // Its name is what the Senate says it is.
        let me = ApiClient::new(&top.url, &top.token)
            .unwrap()
            .me()
            .await
            .unwrap();
        assert_eq!(me.person.as_str(), "alice");
        assert_eq!(me.senate.as_str(), "north-desk");
        assert_eq!(me.parent, None);
        // The named Speaker is seated, under the only person.
        let claude: SenatorName = "claude".parse().unwrap();
        let alice = Name::new("alice").unwrap();
        assert_eq!(
            top.daemon.hub.speaker_seat(),
            Some(&SpeakerSeat::Senator(claude.clone()))
        );
        let roster = top.daemon.hub.roster().await.unwrap();
        assert_eq!(roster.owner_of(&claude), Some(&alice));
        assert!(roster.is_agent(&claude));
        let office = me.office.clone().expect("an office");
        assert_eq!(office.speaker.to_string(), "senator::claude");
        assert_eq!(office.kind, senate_core::SenatorKind::Agent);
        assert_eq!(office.desk, std::slice::from_ref(&claude));
        assert!(
            !paths.hosted("top").join("OFFICE-NOTE").exists(),
            "written in full: nothing to note"
        );
        // The token is private, and the only person is alice.
        let token_file = paths.hosted("top").join("token");
        let mode = std::fs::metadata(&token_file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(roster.principal(&alice).is_some());
        assert_eq!(roster.principals().len(), 1);
        // The developer-machine side serves it.
        let bridge = machine.bridge_config();
        assert_eq!(bridge.server_url, top.url);
        assert_eq!(bridge.token, top.token);
        let (top_url, top_token) = (top.url.clone(), top.token.clone());
        machine.shutdown().await;

        // A restart finds the same address and token.
        let config: BridgeConfig = toml::from_str("person = \"alice\"\n").unwrap();
        let hosted: Hosted =
            toml::from_str("[local]\nname = \"north-desk\"\nspeaker = \"senator::claude\"\n")
                .unwrap();
        let again = Machine::start_with(paths, config, hosted, quiet())
            .await
            .unwrap();
        assert_eq!(again.senate().unwrap().url, top_url);
        assert_eq!(again.senate().unwrap().token, top_token);
        again.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_unnamed_hosted_senate_is_the_persons_desk() {
        let dir = tempfile::tempdir().unwrap();
        let config: BridgeConfig = toml::from_str("person = \"alice\"\n").unwrap();
        let hosted: Hosted = toml::from_str("[local]\n").unwrap();
        let machine = Machine::start_with(Paths::new(dir.path()), config, hosted, quiet())
            .await
            .unwrap();
        let top = machine.senate().unwrap();
        assert_eq!(top.name.as_str(), "alice-desk");
        let me = ApiClient::new(&top.url, &top.token)
            .unwrap()
            .me()
            .await
            .unwrap();
        assert_eq!(me.senate.as_str(), "alice-desk");
        assert_eq!(top.daemon.hub.speaker_seat(), None);
        // The person presides, and no one works the desk: it waits on her.
        let office = me.office.expect("an office");
        assert_eq!(office.speaker.to_string(), "senator::alice");
        assert_eq!(office.kind, senate_core::SenatorKind::Person);
        assert!(office.desk.is_empty());
        machine.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_older_homes_agent_speaker_is_the_persons_secretary_said_once() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("senate"));
        let start = |locale: &str| {
            let config: BridgeConfig =
                toml::from_str(&format!("person = \"alice\"\nlocale = \"{locale}\"\n")).unwrap();
            let hosted: Hosted = toml::from_str("[local]\nspeaker = \"claude\"\n").unwrap();
            Machine::start_with(paths.clone(), config, hosted, quiet())
        };
        let machine = start("en").await.expect("never refused");
        let top = machine.senate().unwrap();
        let me = ApiClient::new(&top.url, &top.token)
            .unwrap()
            .me()
            .await
            .unwrap();
        let office = me.office.expect("an office");
        assert_eq!(office.speaker.to_string(), "senator::alice");
        assert_eq!(office.kind, senate_core::SenatorKind::Person);
        let claude: SenatorName = "claude".parse().unwrap();
        assert_eq!(office.desk, std::slice::from_ref(&claude));
        // claude works the desk first, as the older single seat says.
        assert_eq!(
            top.daemon.hub.speaker_seat(),
            Some(&SpeakerSeat::Senator(claude.clone()))
        );
        let marker = paths.hosted("top").join("OFFICE-NOTE");
        let said = std::fs::read_to_string(&marker).expect("the note is remembered");
        let note = senate_bridge::config::OfficeNote::AgentWasSpeaker {
            agent: claude,
            speaker: "senator::alice".parse().unwrap(),
        };
        assert_eq!(said, format!("{}\n", note.text(Locale::En)));
        let modified = std::fs::metadata(&marker).unwrap().modified().unwrap();
        machine.shutdown().await;

        // Said once: a restart (in another language) leaves the marker be.
        let again = start("zh").await.unwrap();
        again.shutdown().await;
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), said);
        assert_eq!(
            std::fs::metadata(&marker).unwrap().modified().unwrap(),
            modified
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_person_speakers_secretaries_work_the_desk_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let config: BridgeConfig = toml::from_str("person = \"alice\"\n").unwrap();
        let hosted: Hosted = toml::from_str(
            "[local]\nspeaker = \"senator::alice\"\nsecretaries = [\"codex\", \"claude\"]\n",
        )
        .unwrap();
        let machine = Machine::start_with(Paths::new(dir.path()), config, hosted, quiet())
            .await
            .unwrap();
        let top = machine.senate().unwrap();
        let office = top.daemon.hub.office().expect("an office");
        assert_eq!(office.speaker.to_string(), "senator::alice");
        let names: Vec<_> = office.desk.iter().map(ToString::to_string).collect();
        assert_eq!(names, ["codex", "claude"]);
        assert!(!dir.path().join("senates/top/OFFICE-NOTE").exists());
        machine.shutdown().await;
    }

    #[tokio::test]
    async fn a_parents_agent_as_the_speaker_is_refused_at_start() {
        let dir = tempfile::tempdir().unwrap();
        for (locale, needle) in [("en", "not supported yet"), ("zh", "暂不支持")] {
            // The Senate above does not answer; claude is this machine's
            // own agent, so it is the parent's agent all the same.
            let config: BridgeConfig = toml::from_str(&format!(
                "person = \"alice\"\nlocale = \"{locale}\"\nserver_url = \"http://127.0.0.1:1\"\ntoken = \"t\"\n"
            ))
            .unwrap();
            let hosted: Hosted =
                toml::from_str("[local]\nspeaker = \"/acme::senator::claude\"\n").unwrap();
            let Err(MachineError::Invalid(message)) =
                Machine::start_with(Paths::new(dir.path()), config, hosted, quiet()).await
            else {
                panic!("a parent's agent cannot be a child's Speaker");
            };
            assert!(message.contains(needle), "{message}");
            assert!(message.contains("/acme::senator::claude"), "{message}");
            assert!(
                message.contains("Speaker") && message.contains("Senate"),
                "{message}"
            );
        }
        assert!(!dir.path().join("senates").exists(), "nothing is hosted");
    }

    /// Every file under `dir` with its bytes.
    fn files(dir: &Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
        let mut out = std::collections::BTreeMap::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let bytes = std::fs::read(&path).unwrap();
                    out.insert(path, bytes);
                }
            }
        }
        out
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_old_nested_senate_is_left_untouched_and_said_once() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("senate"));
        // A home from before: a Senate nested under claude, with its log.
        let under = paths.hosted("under-claude");
        std::fs::create_dir_all(under.join("data")).unwrap();
        std::fs::write(under.join("log.db"), b"SQLite format 3\0 not really").unwrap();
        std::fs::write(under.join("token"), "local-old\n").unwrap();
        std::fs::write(under.join("listen"), "127.0.0.1:1\n").unwrap();
        std::fs::write(under.join("data").join("blob"), [0u8, 1, 2, 255]).unwrap();
        let before = files(&under);
        let toml_hosted = "[local]\nspeaker = \"claude\"\n[[preside]]\nagent = \"claude\"\n";

        let config: BridgeConfig = toml::from_str("person = \"alice\"\nlocale = \"en\"\n").unwrap();
        let hosted: Hosted = toml::from_str(toml_hosted).unwrap();
        let machine = Machine::start_with(paths.clone(), config, hosted, quiet())
            .await
            .unwrap();
        // Only the machine's Senate is hosted.
        let keys: Vec<_> = machine.senates().iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, vec!["top"]);
        assert_eq!(machine.senate().unwrap().name.as_str(), "alice-desk");
        assert_eq!(
            machine.bridge_config().server_url,
            machine.senate().unwrap().url
        );
        machine.shutdown().await;

        // The old files are as they were; the one new file says why.
        let marker = under.join("NOT-HOSTED");
        let mut after = files(&under);
        let said = String::from_utf8(after.remove(&marker).expect("the notice is kept")).unwrap();
        assert_eq!(after, before, "the nested Senate's files are untouched");
        let en = nested_notice("claude", &under, Locale::En);
        assert_eq!(said, format!("{en}\n"));
        let zh = nested_notice("claude", &under, Locale::Zh);
        for text in [&en, &zh] {
            assert!(text.contains("[[preside]] agent = \"claude\""), "{text}");
            assert!(text.contains(&under.display().to_string()), "{text}");
            assert!(
                text.contains("Senate"),
                "Senate is never translated: {text}"
            );
        }
        assert!(en.contains("no longer hosted"), "{en}");
        assert!(zh.contains("不再托管"), "{zh}");

        // Said once: a restart (in another language) does not say it again.
        let config: BridgeConfig = toml::from_str("person = \"alice\"\nlocale = \"zh\"\n").unwrap();
        let hosted: Hosted = toml::from_str(toml_hosted).unwrap();
        let again = Machine::start_with(paths, config, hosted, quiet())
            .await
            .unwrap();
        assert_eq!(again.senates().len(), 1);
        again.shutdown().await;
        let mut after = files(&under);
        assert_eq!(
            after.remove(&marker).map(String::from_utf8),
            Some(Ok(format!("{en}\n")))
        );
        assert_eq!(after, before);
    }

    #[tokio::test]
    async fn a_home_too_long_for_the_socket_fails_once_and_hosts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("h".repeat(120));
        let config: BridgeConfig = toml::from_str("person = \"alice\"\nlocale = \"en\"\n").unwrap();
        let hosted: Hosted = toml::from_str("[local]\n").unwrap();
        let Err(MachineError::Invalid(message)) =
            Machine::start_with(Paths::new(&home), config, hosted, quiet()).await
        else {
            panic!("a home this long cannot hold the socket");
        };
        assert!(
            message.contains("SENATE_HOME") && message.contains("shorter"),
            "{message}"
        );
        assert!(message.contains(&home.display().to_string()), "{message}");
        assert!(!home.exists(), "nothing was created before the refusal");
    }

    #[tokio::test]
    async fn a_hosted_senate_never_leaves_loopback() {
        let dir = tempfile::tempdir().unwrap();
        let config: BridgeConfig = toml::from_str("person = \"alice\"\n").unwrap();
        let hosted: Hosted = toml::from_str("[local]\nlisten = \"0.0.0.0:7979\"\n").unwrap();
        assert!(matches!(
            Machine::start_with(Paths::new(dir.path()), config, hosted, quiet()).await,
            Err(MachineError::Invalid(_))
        ));
    }
}
