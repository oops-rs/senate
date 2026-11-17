//! `senated --machine`: the same daemon on a developer machine.
//!
//! On a developer machine `senated` reads the machine's `senate.toml` in
//! the Senate home (`~/.senate`, or `$SENATE_HOME`) and runs, in one
//! process:
//!
//! - **the Senates it hosts**: a local top Senate (`[local]`, for using
//!   Senate alone: no Senate above, one of the person's agents named its
//!   Speaker), and one Senate one level down per presiding agent
//!   (`[[preside]]`: `alice/claude` presides, `alice/claude/*` sit there).
//!   Each is a full assembly (server, roster, log) with no built-in
//!   Speaker: its Speaker is the presiding agent's session. Its log is an
//!   SQLite file under `senates/<senate>/` in the Senate home; it listens on
//!   loopback only, on a port kept across restarts; it knows one person,
//!   authenticated by a token generated here, kept in that directory (mode
//!   0600) and never sent anywhere else;
//! - **the developer-machine side** (`senate_bridge::daemon`): sessions,
//!   delivery, MCP and hooks, across every Senate the machine serves (the
//!   one above at `server_url`, if any, and each hosted one), as a
//!   supervised [`Component`] of the first hosted Senate's assembly; with
//!   no hosted Senate, on its own under the same supervision.
//!
//! There is no synchronisation between the levels: the link between them is
//! the presiding agent's session, which sits in one and presides over the
//! other.
//!
//! Directory layout under the Senate home:
//!
//! | Path | What |
//! | --- | --- |
//! | `senate.toml` | the machine's configuration |
//! | `senates/top/` | the local top Senate (`[local]`) |
//! | `senates/under-<agent>/` | the Senate `<person>/<agent>` presides over |
//! | `senates/*/log.db` | its log |
//! | `senates/*/token` | the person's token there, generated here |
//! | `senates/*/listen` | the loopback address it serves, kept for restarts |

use std::io::Write;
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use senate_bridge::config::{Hosted, SenateLink};
use senate_bridge::daemon::{self, DaemonOptions, Timing};
use senate_bridge::{BridgeConfig, Paths};
use senate_core::{EventId, RosterConfig, SenatorPath, Senior};
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
    #[error("the Senate under {key}: {source}")]
    Senate {
        key: String,
        #[source]
        source: AssemblyError,
    },
}

/// One Senate this machine hosts, running.
pub struct HostedSenate {
    /// Its directory under `senates/`.
    pub key: String,
    /// Its presider; `None` for the local top Senate.
    pub presider: Option<SenatorPath>,
    /// `http://127.0.0.1:<port>`.
    pub url: String,
    /// The person's token there.
    pub token: String,
    pub daemon: Senated,
}

/// A developer machine's `senated`: the Senates it hosts and its side of
/// every Senate it serves.
pub struct Machine {
    senates: Vec<HostedSenate>,
    /// The developer-machine side when no hosted Senate carries it.
    alone: Option<Supervisor>,
    bridge: BridgeConfig,
}

/// What one hosted Senate is, before it starts.
struct Plan {
    key: String,
    presider: Option<SenatorPath>,
    speaker: Option<SenatorPath>,
    listen: String,
    token: String,
    dir: std::path::PathBuf,
}

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
        let person = config.person.clone();
        let mut plans = Vec::new();
        if let Some(local) = &hosted.local {
            plans.push(plan(
                &paths,
                "top".into(),
                None,
                local.speaker.as_ref().map(|s| s.under(&person)),
                local.listen.as_deref(),
            )?);
        }
        for preside in &hosted.preside {
            let key = format!("under-{}", preside.agent.to_string().replace('/', "-"));
            plans.push(plan(
                &paths,
                key,
                Some(preside.agent.under(&person)),
                None,
                preside.listen.as_deref(),
            )?);
        }

        // The developer-machine side serves the Senate above (or the local
        // top one) and each Senate one level down.
        let mut bridge = config.clone();
        for p in &plans {
            let url = format!("http://{}", p.listen);
            match &p.presider {
                None => {
                    bridge.server_url = url;
                    bridge.token = p.token.clone();
                }
                Some(presider) => bridge.senates.push(SenateLink {
                    server_url: url,
                    token: p.token.clone(),
                    presider: senate_bridge::agent::RelPath::of(presider),
                }),
            }
        }
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
        let carrier = plans.first().map(|p| p.key.clone());
        let mut ordered: Vec<Plan> = plans;
        if !ordered.is_empty() {
            ordered.rotate_left(1);
        }
        for p in ordered {
            let carries = Some(&p.key) == carrier.as_ref();
            let config = senate_config(&p, &config)?;
            let mut assembly = Assembly::new(config).without_speaker();
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
                senate = %p.key,
                url = %daemon.base_url(),
                presider = ?p.presider,
                speaker = ?p.speaker,
                "hosting a Senate on this machine"
            );
            senates.push(HostedSenate {
                url: daemon.base_url(),
                key: p.key,
                presider: p.presider,
                token: p.token,
                daemon,
            });
        }
        // Back in configuration order: the local top Senate first.
        if let Some(last) = senates.pop() {
            senates.insert(0, last);
        }
        let alone = senates
            .is_empty()
            .then(|| Supervisor::start(crate::empty_hub::empty(), vec![side]));
        Ok(Self {
            senates,
            alone,
            bridge,
        })
    }

    /// The Senates this machine hosts: the local top one first, then one
    /// per presiding agent.
    pub fn senates(&self) -> &[HostedSenate] {
        &self.senates
    }

    /// The hosted Senate presided over by `presider` (`None`: the local top
    /// Senate).
    pub fn senate(&self, presider: Option<&SenatorPath>) -> Option<&HostedSenate> {
        self.senates
            .iter()
            .find(|s| s.presider.as_ref() == presider)
    }

    /// What the developer-machine side was started with: every Senate it
    /// serves, with their addresses and tokens.
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

    /// Stop the developer-machine side, then every hosted Senate.
    pub async fn shutdown(mut self) {
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
    presider: Option<SenatorPath>,
    speaker: Option<SenatorPath>,
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
        presider,
        speaker,
        listen,
        token,
        dir,
    })
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

/// A hosted Senate's `senated` configuration: one person, its token, its
/// presider or named Speaker, SQLite under its directory, loopback.
fn senate_config(plan: &Plan, machine: &BridgeConfig) -> Result<SenatedConfig, MachineError> {
    let roster = RosterConfig {
        presider: plan.presider.clone(),
        speaker: plan.speaker.clone(),
        seniors: vec![Senior::new(machine.person.clone())],
        agents: Vec::new(),
    };
    let mut server = ServerConfig::new(
        roster,
        vec![TokenConfig::with_token(
            machine.person.clone(),
            plan.token.clone(),
        )],
    );
    server.listen = plan.listen.clone();
    server.database_url = format!("sqlite:{}", plan.dir.join("log.db").display());
    server.data_dir = plan.dir.join("data");
    server.locale = machine.locale;
    server
        .validate()
        .map_err(|e| MachineError::Invalid(format!("the Senate under {}: {e}", plan.key)))?;
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

    use senate_core::{Hub, Name, SpeakerSeat};

    use super::*;

    fn quiet() -> Timing {
        Timing::default()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn alone_the_machine_hosts_a_top_senate_presided_over_by_an_agent() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("senate"));
        let config: BridgeConfig = toml::from_str("person = \"alice\"\n").unwrap();
        let hosted: Hosted =
            toml::from_str("[local]\nspeaker = \"claude\"\n[[preside]]\nagent = \"claude\"\n")
                .unwrap();
        let machine = Machine::start_with(paths.clone(), config, hosted, quiet())
            .await
            .unwrap();
        let top = machine.senate(None).unwrap();
        let below = machine
            .senate(Some(&"alice/claude".parse().unwrap()))
            .unwrap();
        assert!(top.url.starts_with("http://127.0.0.1:"));
        let roster = top.daemon.hub.roster().await.unwrap();
        assert_eq!(roster.presider(), None);
        assert_eq!(
            top.daemon.hub.speaker_seat(),
            Some(&SpeakerSeat::Senator("alice/claude".parse().unwrap()))
        );
        assert_eq!(
            below.daemon.hub.speaker_seat(),
            Some(&SpeakerSeat::Senator("alice/claude".parse().unwrap()))
        );
        // The token is private, and the only person is alice.
        let token_file = paths.hosted("top").join("token");
        let mode = std::fs::metadata(&token_file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(roster.senior(&Name::new("alice").unwrap()).is_some());
        assert_eq!(roster.seniors().len(), 1);
        // The developer-machine side serves both.
        let bridge = machine.bridge_config();
        assert_eq!(bridge.server_url, top.url);
        assert_eq!(bridge.senates.len(), 1);
        assert_eq!(bridge.senates[0].server_url, below.url);
        let (top_url, top_token) = (top.url.clone(), top.token.clone());
        machine.shutdown().await;

        // A restart finds the same addresses and tokens.
        let config: BridgeConfig = toml::from_str("person = \"alice\"\n").unwrap();
        let hosted: Hosted =
            toml::from_str("[local]\nspeaker = \"claude\"\n[[preside]]\nagent = \"claude\"\n")
                .unwrap();
        let again = Machine::start_with(paths, config, hosted, quiet())
            .await
            .unwrap();
        assert_eq!(again.senate(None).unwrap().url, top_url);
        assert_eq!(again.senate(None).unwrap().token, top_token);
        again.shutdown().await;
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
