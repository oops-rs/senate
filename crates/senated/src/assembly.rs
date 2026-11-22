//! Putting a daemon together: [`Assembly`] and the running [`Senated`].
//!
//! # When the built-in session runs
//!
//! The built-in session (the engine of `senate-speaker`, a senator named by
//! `secretary.toml`'s `name`, `secretary` by default) runs when both hold:
//!
//! - **a complete model is configured**: `[model]`, `secretary.toml`'s
//!   `[model]`, or (for compatibility) a `[speaker]` / `[secretary]` table,
//!   and the model they make passes its check
//!   ([`SecretaryConfigs::model_in_force`]);
//! - **something refers to it**: the roster's `speaker` names it (an
//!   unattended root), or it is listed among a senator's secretaries (in
//!   `[roster.secretaries]`, or registered at runtime and kept in the
//!   store), or (compatibility) the configuration has `[speaker]` and names
//!   no Speaker, as before Speakers were named: it is then the Speaker of
//!   an unattended root.
//!
//! `speaker` naming it with no complete model is a configuration error. A
//! reference with no model, or a model with no reference, leaves it off,
//! and the log says why. Its name is read once, at start: a changed `name`
//! applies at the next start (the log says so when the file changes).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use senate_core::{ChatSurface, Hub, Locale, Roster, RosterConfig, SenatorName};
use senate_log::LogStore;
use senate_server::{Authenticate, Running, Server, ServerError, ServerHub, redact_url};
use senate_speaker::{
    BasisBrain, Brain, ModelCheck, SecretaryConfigs, SpeakerArgs, SpeakerConfig, SpeakerMsg,
};
use senate_work::{Forge, Runner};
use tokio::net::TcpListener;
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
    #[error("[chat_gateway]: {0:#}")]
    GatewayConfig(#[source] anyhow::Error),
    #[error("a gateway's surface can be mounted only once")]
    GatewayMountedTwice,
    /// The roster's `speaker` names the built-in session, and no complete
    /// model is configured for it. The message is in the deployment's
    /// language.
    #[error("{0}")]
    BuiltInWithoutModel(String),
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
    /// The gateway mounted with [`Assembly::chat_gateway`]: it serves
    /// simulated chat (`senate chat send`).
    gateway: Option<ChatGateway>,
    components: Vec<Arc<dyn Component>>,
    /// Where `secretary.toml` is read, and the file the configuration came
    /// from.
    secretary_dir: Option<PathBuf>,
    config_file: Option<PathBuf>,
    secretary: Option<Arc<SecretaryConfigs>>,
    model_check: Option<ModelCheck>,
    /// No built-in session, whatever the configuration says.
    without_session: bool,
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
            gateway: None,
            components: Vec::new(),
            secretary_dir: None,
            config_file: None,
            secretary: None,
            model_check: None,
            without_session: false,
        }
    }

    /// The configuration was read from `file`: `secretary.toml` is read
    /// beside it ([`crate::config::secretary_dir`]), and what is shown names
    /// it.
    pub fn config_file(mut self, file: impl Into<PathBuf>) -> Self {
        let file = file.into();
        self.secretary_dir = Some(crate::config::secretary_dir(&file));
        self.config_file = Some(file);
        self
    }

    /// Read `secretary.toml` and the committees' files in `dir`. Without
    /// this (or [`Assembly::config_file`]) there are none: the
    /// configuration's `[model]` and `[speaker]` are all.
    pub fn secretary_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.secretary_dir = Some(dir.into());
        self
    }

    /// Run the built-in session on `secretary`, a configuration loaded
    /// elsewhere (a machine's: [`crate::config::load_secretary_home`]),
    /// instead of the one this configuration and its directory make.
    pub fn secretary(mut self, secretary: Arc<SecretaryConfigs>) -> Self {
        self.secretary = Some(secretary);
        self
    }

    /// Check models with `check` instead of basis's rules (a host with
    /// providers of its own, or a test).
    pub fn model_check(mut self, check: ModelCheck) -> Self {
        self.model_check = Some(check);
        self
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

    /// Configure the built-in session with `config`, replacing `[speaker]`
    /// (read, as that table is, as if it were `secretary.toml`; it also
    /// configures a model).
    pub fn speaker(mut self, config: SpeakerConfig) -> Self {
        self.config.speaker = Some(config);
        self
    }

    /// No built-in session, whatever the configuration says.
    pub fn without_speaker(mut self) -> Self {
        self.config.speaker = None;
        self.without_session = true;
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
    pub fn chat_gateway(mut self, gateway: ChatGateway) -> Result<Self, AssemblyError> {
        let mount = gateway.mount().ok_or(AssemblyError::GatewayMountedTwice)?;
        self.gateway = Some(gateway.clone());
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

    /// Open the log, start the server, then the built-in session (when it
    /// runs: see the module docs) and every component.
    pub async fn start(self) -> Result<Senated, AssemblyError> {
        self.start_with_listener(None).await
    }

    pub(crate) async fn start_with_listener(
        self,
        listener: Option<TcpListener>,
    ) -> Result<Senated, AssemblyError> {
        let Self {
            config,
            store,
            auth,
            roster,
            brain,
            hands,
            mut chats,
            gateway,
            components,
            secretary_dir,
            config_file,
            secretary,
            model_check,
            without_session,
        } = self;
        let remote = crate::gateway_ipc::RemoteGateway::from_config(&config)
            .map_err(AssemblyError::GatewayConfig)?;
        let surfaces = chats.len() + usize::from(remote.is_some());
        if surfaces > 1 {
            return Err(AssemblyError::SeveralSurfaces(surfaces));
        }
        let secretary = secretary.unwrap_or_else(|| {
            let base = config.secretary_base(config_file.as_deref());
            Arc::new(match model_check {
                Some(check) => SecretaryConfigs::with_check(secretary_dir, base, check),
                None => SecretaryConfigs::new(secretary_dir, base),
            })
        });
        let store = match store {
            Some(store) => store,
            None => senate_log::connect(&config.server.database_url)
                .await
                .map_err(|source| AssemblyError::Store {
                    url: redact_url(&config.server.database_url),
                    source,
                })?,
        };
        let locale = config.server.locale;
        let roster_config = roster
            .as_ref()
            .map_or_else(|| config.server.roster.clone(), Roster::to_config);
        let registered = registered_secretaries(store.as_ref()).await;
        let session = if without_session {
            None
        } else {
            decide_session(
                &secretary,
                &roster_config,
                &registered,
                secretary.compat_section().is_some(),
                locale,
            )?
        };
        if !chats.is_empty() && session.is_none() {
            return Err(AssemblyError::SurfaceWithoutSpeaker);
        }
        let simulator: Option<Arc<dyn senate_core::chat_sim::ChatSimulator>> = gateway
            .or_else(|| remote.as_ref().map(|r| r.gateway.clone()))
            .map(|g| Arc::new(g) as Arc<dyn senate_core::chat_sim::ChatSimulator>);
        let adapter = if let Some(remote) = remote {
            remote.record_status_in(&config.server.data_dir);
            let mount = remote
                .gateway
                .mount()
                .ok_or(AssemblyError::GatewayMountedTwice)?;
            let surface = mount.surface.clone();
            if session.is_some() {
                chats.push(mount);
            }
            Some(crate::gateway_ipc::Adapter {
                remote,
                store: store.clone(),
                surface,
                locale,
                builtin: session.is_some(),
            })
        } else {
            None
        };
        if session.is_some() {
            for note in secretary.notes() {
                say_once(store.as_ref(), note).await;
            }
        }
        // The built-in session is a senator of the roster: the Speaker, or
        // a secretary of whoever lists it; only it judges roots, where it
        // works the desk.
        let mut server =
            Server::builder(config.server.clone(), store).built_in_session(session.clone());
        if let Some(auth) = auth {
            server = server.auth(auth);
        }
        if let Some(roster) = roster {
            server = server.roster(roster);
        }
        if let Some(simulator) = simulator.filter(|_| session.is_some()) {
            // Simulated chat is answered by the Speaker: none, no chat.
            server = server.chat_simulator(simulator);
        }
        let running = match listener {
            Some(listener) => server.start_with_listener(listener).await?,
            None => server.start().await?,
        };
        let hub: Arc<dyn Hub> = running.hub.clone();

        let slot = SpeakerSlot::default();
        let mut all: Vec<Arc<dyn Component>> = Vec::new();
        if session.is_some() {
            let speaker_config = secretary.effective(None).speaker.clone();
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
                // Once at start, off the critical path: logged, never fatal.
                tokio::spawn(crate::speaker::log_model_check(BasisBrain::for_probe(
                    secretary.clone(),
                    hub.clone(),
                )));
                Arc::new(BasisBrain::on_secretary(secretary.clone(), hub.clone())) as Arc<dyn Brain>
            });
            let mut args = SpeakerArgs::new(hub.clone(), surface, brain, speaker_config, unbound)
                .with_secretary(secretary.clone());
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
        if let Some(adapter) = adapter {
            all.push(Arc::new(adapter.remote.gateway.clone()));
            all.push(Arc::new(adapter));
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
            speaker: session.is_some().then_some(slot),
            session,
            secretary,
        })
    }
}

/// The secretaries registered at runtime, as the server keeps them in the
/// store's registry (`secretaries:<senator>` → names): read before the
/// server starts, so a built-in session registered as someone's secretary
/// runs again after a restart.
async fn registered_secretaries(store: &dyn LogStore) -> Vec<SenatorName> {
    match store.registry("secretaries:").await {
        Ok(entries) => entries
            .into_iter()
            .filter_map(|(_, value)| serde_json::from_value::<Vec<SenatorName>>(value).ok())
            .flatten()
            .collect(),
        Err(error) => {
            tracing::debug!(%error, "cannot read the registered secretaries");
            Vec::new()
        }
    }
}

/// Say `note` once: logged as a warning the first time, as a debug line
/// after (the store's registry remembers it, as the server's start notes).
async fn say_once(store: &dyn LogStore, note: &str) {
    let key = format!("note:secretary:{note}");
    let said = match store.registry(&key).await {
        Ok(found) => found.iter().any(|(k, _)| *k == key),
        Err(_) => false,
    };
    if said {
        tracing::debug!("{note}");
        return;
    }
    tracing::warn!("{note}");
    if let Err(error) = store
        .put_registry(&key, Some(serde_json::Value::Bool(true)))
        .await
    {
        tracing::debug!(%error, "the note will be said again next start");
    }
}

/// Whether the built-in session runs, and under which name: a complete
/// model and a reference (see the module docs).
fn decide_session(
    secretary: &SecretaryConfigs,
    roster: &RosterConfig,
    registered: &[SenatorName],
    compat: bool,
    locale: Locale,
) -> Result<Option<SenatorName>, AssemblyError> {
    let name = secretary.name().clone();
    let names_it = roster
        .speaker
        .as_ref()
        .is_some_and(|s| s.id.senate().is_none() && *s.id.name() == name);
    let listed = roster
        .secretaries
        .values()
        .flatten()
        .chain(registered)
        .any(|s| *s == name);
    let unattended = compat && roster.speaker.is_none();
    let in_force = secretary.model_in_force();
    if names_it && !in_force {
        let problems: Vec<String> = secretary
            .problems()
            .iter()
            .map(ToString::to_string)
            .collect();
        return Err(AssemblyError::BuiltInWithoutModel(
            text::speaker_without_model(locale, &name, &problems),
        ));
    }
    if !(names_it || listed || unattended) {
        if in_force {
            tracing::info!("{}", text::model_unreferenced(locale, &name));
        } else {
            tracing::info!("{}", text::no_session(locale));
        }
        return Ok(None);
    }
    if !in_force {
        tracing::warn!("{}", text::referenced_without_model(locale, &name));
        return Ok(None);
    }
    let role = if names_it || unattended {
        "the Speaker"
    } else {
        "a secretary"
    };
    tracing::info!(name = %name, role, "the built-in session runs");
    Ok(Some(name))
}

/// What the assembly says about the built-in session, in both languages.
mod text {
    use senate_core::{Locale, SenatorName};

    pub(super) fn speaker_without_model(
        l: Locale,
        name: &SenatorName,
        problems: &[String],
    ) -> String {
        let mut out = match l {
            Locale::Zh => format!(
                "配置错误：speaker 指定内置会话 [senator::{name}] 为 Speaker，但没有配置可用的模型。请在顶层 [model] 表（或 secretary.toml 的 [model]）中配置 provider、model 和 api_key_env（密钥所在环境变量的名字）。"
            ),
            Locale::En => format!(
                "configuration error: speaker names the built-in session [senator::{name}], but no usable model is configured for it. Configure provider, model and api_key_env (the NAME of the variable holding the key) in the top-level [model] table (or in secretary.toml's [model])."
            ),
        };
        for problem in problems {
            out.push_str(
                "
  ",
            );
            out.push_str(problem);
        }
        out
    }

    pub(super) fn model_unreferenced(l: Locale, name: &SenatorName) -> String {
        match l {
            Locale::Zh => format!(
                "已配置模型，但没有任何地方引用内置会话 [senator::{name}]（既不是 speaker，也不是任何 senator 的秘书）：它不运行。"
            ),
            Locale::En => format!(
                "A model is configured, but nothing refers to the built-in session [senator::{name}] (it is neither the speaker nor anyone's secretary): it does not run."
            ),
        }
    }

    pub(super) fn no_session(l: Locale) -> String {
        match l {
            Locale::Zh => "未配置模型，也没有引用内置会话：只提供日志和 bridge API。".into(),
            Locale::En => {
                "No model is configured and nothing refers to the built-in session: serving the log and the bridge API only."
                    .into()
            }
        }
    }

    pub(super) fn referenced_without_model(l: Locale, name: &SenatorName) -> String {
        match l {
            Locale::Zh => format!(
                "内置会话 [senator::{name}] 被列为秘书，但没有配置可用的模型：它不运行，Speaker 的待办由 Speaker 本人处理。请配置 [model]。"
            ),
            Locale::En => format!(
                "The built-in session [senator::{name}] is listed as a secretary, but no usable model is configured: it does not run, and the Speaker's desk waits on the Speaker. Configure [model]."
            ),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_text_reads_in_both_languages_with_ids_bracketed() {
            let name: SenatorName = "clerk".parse().expect("name");
            for l in Locale::ALL {
                for text in [
                    speaker_without_model(l, &name, &[]),
                    model_unreferenced(l, &name),
                    referenced_without_model(l, &name),
                ] {
                    assert!(text.contains("[senator::clerk]"), "{text}");
                }
            }
        }
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
    session: Option<SenatorName>,
    secretary: Arc<SecretaryConfigs>,
}

impl Senated {
    /// The built-in session's name, when it runs.
    pub fn built_in_session(&self) -> Option<&SenatorName> {
        self.session.as_ref()
    }

    /// The built-in session's configuration (its problems, what is in
    /// force), whether or not it runs.
    pub fn secretary(&self) -> &Arc<SecretaryConfigs> {
        &self.secretary
    }

    /// `http://<local_addr>`: where bridges connect.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.local_addr)
    }

    /// The hub, as components see it.
    pub fn hub(&self) -> Arc<dyn Hub> {
        self.hub.clone()
    }

    /// Whether the built-in session is part of this daemon.
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

/// SIGINT or SIGTERM: whether it was SIGTERM (a service manager or the
/// system stopping it, rather than a person at its terminal).
pub(crate) async fn shutdown_signal() -> anyhow::Result<bool> {
    use anyhow::Context;
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate =
            signal(SignalKind::terminate()).context("installing the SIGTERM handler")?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result.context("waiting for ctrl-c")?;
                Ok(false)
            }
            _ = terminate.recv() => Ok(true),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .context("waiting for ctrl-c")?;
        Ok(false)
    }
}
