//! Putting a daemon together: [`Assembly`] and the running [`Senated`].

use std::net::SocketAddr;
use std::sync::Arc;

use senate_core::{ChatSurface, Hub, Roster};
use senate_log::LogStore;
use senate_server::{Authenticate, Running, Server, ServerError, ServerHub, redact_url};
use senate_speaker::{BasisBrain, Brain, SpeakerArgs, SpeakerConfig, SpeakerMsg};
use senate_work::{Forge, Runner};
use tokio::sync::{mpsc, watch};

use crate::chat::{ChatGateway, ChatMount};
use crate::component::{Component, GaveUp, Supervisor};
use crate::config::SenatedConfig;
use crate::speaker::{NoChat, SpeakerComponent, SpeakerSlot};

#[derive(Debug, thiserror::Error)]
pub enum AssemblyError {
    #[error("opening the log at {url}: {source}")]
    Store {
        url: String,
        #[source]
        source: senate_log::LogError,
    },
    #[error(transparent)]
    Server(#[from] ServerError),
    #[error("{0} chat surfaces are mounted; the built-in Speaker speaks on one platform at a time")]
    SeveralSurfaces(usize),
    #[error("a chat surface is mounted with no Speaker to speak through it; add [speaker]")]
    SurfaceWithoutSpeaker,
    #[error("a gateway's surface can be mounted only once")]
    GatewayMountedTwice,
}

/// The Speaker's hands: a runner, and the forge that opens its PRs.
type Hands = (Arc<dyn Runner>, Option<Arc<dyn Forge>>);

/// Builds a daemon: the log and the server, the built-in Speaker if it is
/// configured, and whatever a private layer mounts.
///
/// The common case, a private chat layer on top:
///
/// ```no_run
/// # async fn run(platform: std::sync::Arc<dyn senate_core::ChatSurface>)
/// # -> anyhow::Result<()> {
/// use senated::{Assembly, ChatGateway, SenatedConfig};
///
/// let config = SenatedConfig::load("senated.toml".as_ref())?;
/// let gateway = ChatGateway::new(platform);
/// let daemon = Assembly::new(config)
///     .chat_gateway(gateway.clone())?
///     .start()
///     .await?;
/// // The platform's transport calls `gateway.deliver(inbound)` per message.
/// daemon.run_until_signal().await
/// # }
/// ```
///
/// What is not given is taken from the configuration: the store from
/// `database_url`, authentication from `[[tokens]]`, the roster from
/// `[roster]`, the Speaker from `[speaker]` (its model through basis), its
/// hands from `[work]`.
pub struct Assembly {
    config: SenatedConfig,
    store: Option<Arc<dyn LogStore>>,
    auth: Option<Arc<dyn Authenticate>>,
    roster: Option<Roster>,
    brain: Option<Arc<dyn Brain>>,
    hands: Option<Hands>,
    chats: Vec<ChatMount>,
    components: Vec<Arc<dyn Component>>,
}

impl Assembly {
    pub fn new(config: SenatedConfig) -> Self {
        Self {
            config,
            store: None,
            auth: None,
            roster: None,
            brain: None,
            hands: None,
            chats: Vec::new(),
            components: Vec::new(),
        }
    }

    /// Keep the log in `store` instead of the one `database_url` names.
    pub fn store(mut self, store: Arc<dyn LogStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Authenticate bearer tokens with `auth` instead of `[[tokens]]`.
    pub fn auth(mut self, auth: Arc<dyn Authenticate>) -> Self {
        self.auth = Some(auth);
        self
    }

    /// Start from `roster` instead of `[roster]` (people from a directory of
    /// the layer's own, say). Agents are seated as they connect either way.
    pub fn roster(mut self, roster: Roster) -> Self {
        self.roster = Some(roster);
        self
    }

    /// Run the built-in Speaker with `config`, replacing `[speaker]`.
    pub fn speaker(mut self, config: SpeakerConfig) -> Self {
        self.config.speaker = Some(config);
        self
    }

    /// No built-in Speaker, whatever `[speaker]` says.
    pub fn without_speaker(mut self) -> Self {
        self.config.speaker = None;
        self
    }

    /// The Speaker's model. Default: basis, as `[speaker]` configures it.
    pub fn brain(mut self, brain: Arc<dyn Brain>) -> Self {
        self.brain = Some(brain);
        self
    }

    /// The Speaker's hands, replacing what `[work]` names. Work also needs
    /// the Speaker's rules, `[speaker.work]`.
    pub fn work(mut self, runner: Arc<dyn Runner>, forge: Option<Arc<dyn Forge>>) -> Self {
        self.hands = Some((runner, forge));
        self
    }

    /// Mount a chat platform: the surface the Speaker posts through and the
    /// mentions in groups bound to no committee. At most one: the Speaker
    /// speaks on one platform.
    pub fn chat(mut self, mount: ChatMount) -> Self {
        self.chats.push(mount);
        self
    }

    /// Mount a chat platform through [`ChatGateway`], which writes what
    /// people say there into the log: its surface for the Speaker, and the
    /// gateway itself as a component.
    pub fn chat_gateway(self, gateway: ChatGateway) -> Result<Self, AssemblyError> {
        let mount = gateway.mount().ok_or(AssemblyError::GatewayMountedTwice)?;
        Ok(self.chat(mount).component(gateway))
    }

    /// Run `component` beside the server, supervised with the rest.
    pub fn component(mut self, component: impl Component) -> Self {
        self.components.push(Arc::new(component));
        self
    }

    /// [`Assembly::component`] for one already shared.
    pub fn component_arc(mut self, component: Arc<dyn Component>) -> Self {
        self.components.push(component);
        self
    }

    /// Open the log, start the server, then the Speaker and every
    /// component.
    pub async fn start(self) -> Result<Senated, AssemblyError> {
        let Self {
            config,
            store,
            auth,
            roster,
            brain,
            hands,
            mut chats,
            components,
        } = self;
        if chats.len() > 1 {
            return Err(AssemblyError::SeveralSurfaces(chats.len()));
        }
        if !chats.is_empty() && config.speaker.is_none() {
            return Err(AssemblyError::SurfaceWithoutSpeaker);
        }
        let store = match store {
            Some(store) => store,
            None => senate_log::connect(&config.server.database_url)
                .await
                .map_err(|source| AssemblyError::Store {
                    url: redact_url(&config.server.database_url),
                    source,
                })?,
        };
        // The built-in Speaker holds the seat of the (top) Senate it runs
        // in, and only it judges roots; without it the roster says who
        // presides (a nested Senate's presider, a named agent, or nobody).
        let mut server = Server::builder(config.server.clone(), store)
            .built_in_speaker(config.speaker.is_some());
        if let Some(auth) = auth {
            server = server.auth(auth);
        }
        if let Some(roster) = roster {
            server = server.roster(roster);
        }
        let running = server.start().await?;
        let hub: Arc<dyn Hub> = running.hub.clone();

        let slot = SpeakerSlot::default();
        let mut all: Vec<Arc<dyn Component>> = Vec::new();
        if let Some(speaker_config) = config.speaker.clone() {
            let (surface, unbound) = match chats.pop() {
                Some(mount) => (mount.surface, mount.unbound),
                None => {
                    // Nothing will ever be mentioned in an unbound group.
                    let (_, unbound) = mpsc::channel(1);
                    let surface: Arc<dyn ChatSurface> = Arc::new(NoChat);
                    (surface, unbound)
                }
            };
            let brain = brain.unwrap_or_else(|| {
                Arc::new(BasisBrain::new(&speaker_config, hub.clone())) as Arc<dyn Brain>
            });
            let mut args = SpeakerArgs::new(hub.clone(), surface, brain, speaker_config, unbound);
            if let Some((runner, forge)) = hands.or_else(|| config.work.as_ref().map(|w| w.hands()))
            {
                args = args.with_work(runner, forge);
            }
            // First, so a gateway's first unbound mention has a reader.
            all.push(Arc::new(SpeakerComponent {
                args,
                slot: slot.clone(),
            }));
        }
        all.extend(components);
        let names: Vec<String> = all.iter().map(|c| c.name().to_owned()).collect();
        let supervisor = Supervisor::start(hub, all);
        tracing::info!(addr = %running.local_addr, components = ?names, "senated running");
        Ok(Senated {
            hub: running.hub.clone(),
            local_addr: running.local_addr,
            gave_up: supervisor.gave_up(),
            server: Some(running),
            supervisor: Some(supervisor),
            speaker: config.speaker.is_some().then_some(slot),
        })
    }
}

/// A running daemon.
pub struct Senated {
    /// The hub the server, the Speaker and every component share.
    pub hub: Arc<ServerHub>,
    pub local_addr: SocketAddr,
    gave_up: watch::Receiver<Option<GaveUp>>,
    server: Option<Running>,
    supervisor: Option<Supervisor>,
    speaker: Option<SpeakerSlot>,
}

impl Senated {
    /// `http://<local_addr>`: where bridges connect.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.local_addr)
    }

    /// The hub, as components see it.
    pub fn hub(&self) -> Arc<dyn Hub> {
        self.hub.clone()
    }

    /// Whether the built-in Speaker is part of this daemon.
    pub fn has_speaker(&self) -> bool {
        self.speaker.is_some()
    }

    /// Wait until the Speaker has caught every committee up and no model
    /// call is in flight. Returns at once when there is no Speaker, or it is
    /// between restarts.
    pub async fn settle_speaker(&self) {
        let Some(actor) = self.speaker.as_ref().and_then(SpeakerSlot::get) else {
            return;
        };
        if let Err(error) = ractor::call!(actor, SpeakerMsg::Settle) {
            tracing::debug!(%error, "the Speaker did not settle");
        }
    }

    /// Resolves when the components kept failing and supervision gave up;
    /// never otherwise.
    pub async fn failed(&self) -> GaveUp {
        let mut gave_up = self.gave_up.clone();
        match gave_up.wait_for(Option::is_some).await {
            Ok(why) => why
                .clone()
                .unwrap_or_else(|| unreachable!("waited for Some")),
            Err(_) => std::future::pending().await,
        }
    }

    /// Stop the components (the Speaker included), then the server.
    pub async fn shutdown(mut self) -> Result<(), ServerError> {
        if let Some(supervisor) = self.supervisor.take() {
            supervisor.stop().await;
        }
        match self.server.take() {
            Some(server) => server.shutdown().await,
            None => Ok(()),
        }
    }

    /// Serve until SIGINT or SIGTERM, then shut down gracefully. An error
    /// when the components kept failing: the daemon stops rather than serve
    /// without them.
    pub async fn run_until_signal(self) -> anyhow::Result<()> {
        let failed = tokio::select! {
            signal = shutdown_signal() => {
                signal?;
                None
            }
            why = self.failed() => Some(why),
        };
        tracing::info!("shutting down");
        self.shutdown().await?;
        match failed {
            Some(why) => Err(anyhow::anyhow!("{why}")),
            None => Ok(()),
        }
    }
}

pub(crate) async fn shutdown_signal() -> anyhow::Result<()> {
    use anyhow::Context;
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate =
            signal(SignalKind::terminate()).context("installing the SIGTERM handler")?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.context("waiting for ctrl-c")?,
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c()
        .await
        .context("waiting for ctrl-c")?;
    Ok(())
}
