//! Starting and stopping the server.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::http::HeaderValue;
use ractor::{Actor, ActorCell};
use senate_core::{Roster, SenatorName};
use senate_log::LogStore;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinHandle;

use crate::actors::presence::Presence;
use crate::actors::supervisor::{Registry, SenateSupervisor, SupervisorArgs};
use crate::auth::{Authenticate, StaticTokens};
use crate::config::{ConfigError, ServerConfig};
use crate::http::{self, AppState};
use crate::hub::{BROADCAST_CAPACITY, HubParts, ServerHub};
use crate::office::{self, default_built_in_name};
use crate::registry;
use crate::seats::Seats;

/// How long [`Running::shutdown`] waits for each stage.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("binding {addr}: {source}")]
    Bind {
        addr: String,
        source: std::io::Error,
    },
    #[error("starting the supervision tree: {0}")]
    Spawn(String),
    #[error("loading senator aliases: {0}")]
    Aliases(String),
    #[error("reading the log's epoch: {0}")]
    Epoch(String),
    #[error("HTTP server: {0}")]
    Serve(std::io::Error),
    #[error("HTTP server task: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("shutdown timed out waiting for {0}")]
    ShutdownTimeout(&'static str),
}

pub struct Server;

impl Server {
    /// Start with everything from `config`: its tokens authenticate
    /// ([`StaticTokens`]) and its roster says who exists. Shorthand for
    /// `Server::builder(config, store).start()`.
    pub async fn start(
        config: ServerConfig,
        store: Arc<dyn LogStore>,
    ) -> Result<Running, ServerError> {
        Self::builder(config, store).start().await
    }

    /// A server over `store`, with the two seams a private layer may replace:
    /// [`ServerBuilder::auth`] and [`ServerBuilder::roster`].
    pub fn builder(config: ServerConfig, store: Arc<dyn LogStore>) -> ServerBuilder {
        ServerBuilder {
            config,
            store,
            auth: None,
            roster: None,
            triaged: false,
            built_in: None,
        }
    }
}

/// Assembles a [`Server`].
///
/// - **Authentication** ([`Authenticate`]): bearer token to person. Default:
///   [`StaticTokens`] from `config.tokens`.
/// - **Roster**: the people, and the agents seated from the start. Default:
///   `config.roster`. A layer whose people live elsewhere builds the
///   [`Roster`] itself and passes it here, and can change the people while
///   the server runs with [`ServerHub::set_principals`]. Agents are seated as
///   they connect either way.
pub struct ServerBuilder {
    config: ServerConfig,
    store: Arc<dyn LogStore>,
    auth: Option<Arc<dyn Authenticate>>,
    roster: Option<Roster>,
    triaged: bool,
    built_in: Option<SenatorName>,
}

impl ServerBuilder {
    /// Authenticate with `auth` instead of the configured tokens. With this
    /// set, `config.tokens` is not read.
    pub fn auth(mut self, auth: Arc<dyn Authenticate>) -> Self {
        self.auth = Some(auth);
        self
    }

    /// Start from `roster` instead of the configured one. With this set,
    /// `config.roster` is not read.
    pub fn roster(mut self, roster: Roster) -> Self {
        self.roster = Some(roster);
        self
    }

    /// Say that a Speaker attached to this server's hub judges task roots
    /// (`/v1/me`'s [`triaged`](senate_core::api::Me::triaged)). Off by
    /// default: the server alone judges nothing, and a bridge that waited
    /// for a verdict would hold every question for nothing. Whoever attaches
    /// the built-in Speaker turns it on.
    pub fn triaged(mut self, triaged: bool) -> Self {
        self.triaged = triaged;
        self
    }

    /// Say that the daemon runs the built-in session beside this server's
    /// hub, under the flat name `name` (`None`: it does not run). It is a
    /// senator of the roster ([`Roster::with_built_in`]), run by the
    /// Speaker's principal; it is the Speaker when the configuration names
    /// it, or names no Speaker at all (an unattended root, as a Senate
    /// configured before Speakers were named had it); else it works the
    /// desk of whoever lists it as a secretary.
    pub fn built_in_session(mut self, name: Option<SenatorName>) -> Self {
        self.built_in = name;
        self
    }

    /// The older form of [`ServerBuilder::built_in_session`]: the built-in
    /// session runs under its default name, `secretary`.
    pub fn built_in_speaker(self, built_in: bool) -> Self {
        self.built_in_session(built_in.then(default_built_in_name))
    }

    /// Spawn the supervision tree over the store, bind `config.listen`, and
    /// serve the HTTP API. The returned [`Running::hub`] is the same hub the
    /// HTTP layer uses; attach the gateway and the Speaker to it.
    pub async fn start(self) -> Result<Running, ServerError> {
        self.serve(None).await
    }

    /// Start on the listener already bound by a local embedded Senate.
    pub async fn start_with_listener(self, listener: TcpListener) -> Result<Running, ServerError> {
        self.serve(Some(listener)).await
    }

    async fn serve(self, listener: Option<TcpListener>) -> Result<Running, ServerError> {
        let Self {
            config,
            store,
            auth,
            roster,
            triaged,
            built_in,
        } = self;
        config.validate_intervals()?;
        let auth: Arc<dyn Authenticate> = match auth {
            Some(auth) => auth,
            // Only what is not replaced is read from, and so checked in,
            // the configuration.
            None => Arc::new(StaticTokens::new(&config.tokens)?),
        };
        let roster = match roster {
            Some(roster) => roster.to_config(),
            None => config.roster.clone(),
        };
        let (mut roster, notes) = office::settle(
            roster,
            config.parent.as_ref(),
            built_in.clone(),
            config.locale,
        )?;
        // What was registered at runtime, over the configuration's: who
        // runs each agent, the secretaries, the child Senates.
        let loaded = load_registry(store.as_ref(), &mut roster).await;
        let speaker = roster
            .speaker_seat(built_in.is_some())
            .map_err(|e| ConfigError::Invalid(format!("the Speaker: {e}")))?;
        // Only the built-in session judges roots, where it works the desk;
        // a desk worked by agents never makes a bridge wait.
        let triaged = triaged || roster.office().is_some_and(|o| o.triages());
        if let Some(office) = roster.office() {
            tracing::info!(speaker = %office.speaker, kind = ?office.kind, desk = ?office.desk, "the Speaker's office");
        }
        let mut said = Vec::with_capacity(notes.len());
        for note in notes {
            if registry::first_time(store.as_ref(), &note.key).await {
                match note.level {
                    office::Level::Info => tracing::info!("{}", note.text),
                    office::Level::Warn => tracing::warn!("{}", note.text),
                }
            } else {
                tracing::debug!("{}", note.text);
            }
            said.push(note.text);
        }
        let aliases = registry::aliases(store.as_ref())
            .await
            .map_err(|e| ServerError::Aliases(e.to_string()))?;
        for (alias, primary) in &aliases {
            if roster.knows(alias) || !roster.is_agent(primary) {
                return Err(ServerError::Aliases(format!(
                    "invalid stored alias {alias} for {primary}"
                )));
            }
        }
        let seats = Arc::new(Seats::new(roster));
        let epoch = store
            .epoch()
            .await
            .map_err(|e| ServerError::Epoch(e.to_string()))?;
        let epoch_header = HeaderValue::from_str(&epoch)
            .map_err(|_| ServerError::Epoch(format!("{epoch:?} is not a valid header value")))?;

        let (events, _) = broadcast::channel(BROADCAST_CAPACITY);
        let presence = Arc::new(Presence::new(config.presence_grace()));
        let registry = Arc::new(Registry::default());
        let (supervisor, supervisor_task) = Actor::spawn(
            None,
            SenateSupervisor,
            SupervisorArgs {
                store: store.clone(),
                events: events.clone(),
                presence: presence.clone(),
                registry: registry.clone(),
                leases: config.lease_timing(),
                seats: seats.clone(),
            },
        )
        .await
        .map_err(|e| ServerError::Spawn(e.to_string()))?;
        tracing::info!(%epoch, "log epoch");
        let hub = Arc::new(ServerHub::new(HubParts {
            store,
            epoch,
            supervisor: supervisor.clone(),
            registry,
            events,
            presence,
            seats,
            locale: config.locale,
            speaker,
            built_in,
            notes: said,
            children: loaded,
            aliases,
            senate: config.senate_name(),
            parent: config.parent.clone(),
        }));

        // Every committee's writer runs its presence leases, so start them all
        // now: after a restart an agent that never comes back must still go
        // `disconnected` and `absent`. Starting one also seats again the
        // agents its log has seen, so this finishes before anything is
        // served: the roster the hub hands the Speaker, and the agents
        // `/v1/me` lists, are complete from the first request.
        hub.clone().start_writers().await;

        let listener = match listener {
            Some(listener) => Ok(listener),
            None => TcpListener::bind(&config.listen).await,
        }
        .map_err(|source| {
            supervisor.stop(None);
            ServerError::Bind {
                addr: config.listen.clone(),
                source,
            }
        })?;
        let local_addr = listener.local_addr().map_err(|source| ServerError::Bind {
            addr: config.listen.clone(),
            source,
        })?;

        let app = http::router(AppState {
            hub: hub.clone(),
            auth,
            epoch: epoch_header,
            ping_every: config.sse_ping_interval(),
            triaged,
        });
        let (stop_http, stopped) = oneshot::channel::<()>();
        let http_task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    // Either an explicit shutdown or `Running` was dropped.
                    if stopped.await.is_err() {
                        tracing::debug!("server handle dropped; shutting down HTTP");
                    }
                })
                .await
        });
        tracing::info!(%local_addr, "senate server listening");

        Ok(Running {
            hub,
            local_addr,
            supervisor: supervisor.get_cell(),
            stop_http: Some(stop_http),
            http_task: Some(http_task),
            supervisor_task: Some(supervisor_task),
        })
    }
}

/// Load the store's registry over `roster` (who runs each agent, the
/// secretaries) and return the registered child Senates. An entry the
/// roster refuses now is logged and left out; a store that cannot be read
/// starts the server with the configuration alone.
async fn load_registry(
    store: &dyn LogStore,
    roster: &mut Roster,
) -> std::collections::BTreeMap<senate_core::Name, senate_core::api::ChildSenate> {
    match registry::seats(store).await {
        Ok(seats) => {
            for (agent, owner) in seats {
                if roster.built_in() == Some(&agent) {
                    continue;
                }
                if let Err(e) = roster.seat(agent.clone(), &owner) {
                    tracing::warn!(%agent, %owner, "a kept seat is not seated again: {e}");
                }
            }
        }
        Err(e) => tracing::warn!("reading the kept seats: {e}"),
    }
    match registry::secretaries(store).await {
        Ok(relations) => {
            for (principal, secretaries) in relations {
                if let Err(e) = roster.set_secretaries(principal.clone(), secretaries) {
                    tracing::warn!(%principal, "kept secretaries are not registered again: {e}");
                }
            }
        }
        Err(e) => tracing::warn!("reading the kept secretaries: {e}"),
    }
    match registry::children(store).await {
        Ok(children) => children,
        Err(e) => {
            tracing::warn!("reading the registered child Senates: {e}");
            Default::default()
        }
    }
}

/// A started server. Dropping it stops the server without waiting; prefer
/// [`Running::shutdown`].
pub struct Running {
    pub hub: Arc<ServerHub>,
    pub local_addr: SocketAddr,
    /// The root of the supervision tree.
    pub supervisor: ActorCell,
    stop_http: Option<oneshot::Sender<()>>,
    http_task: Option<JoinHandle<std::io::Result<()>>>,
    supervisor_task: Option<JoinHandle<()>>,
}

impl Running {
    /// `http://<local_addr>`.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.local_addr)
    }

    /// Stop accepting connections, stop the supervision tree (which ends
    /// every open SSE stream), then wait for in-flight requests to finish.
    pub async fn shutdown(mut self) -> Result<(), ServerError> {
        if let Some(stop) = self.stop_http.take() {
            // Err means the HTTP task already exited; awaiting it reports why.
            if stop.send(()).is_err() {
                tracing::debug!("HTTP server already stopped");
            }
        }
        self.supervisor.stop(Some("shutdown".into()));
        if let Some(task) = self.supervisor_task.take() {
            tokio::time::timeout(SHUTDOWN_GRACE, task)
                .await
                .map_err(|_| ServerError::ShutdownTimeout("the supervision tree"))??;
        }
        if let Some(task) = self.http_task.take() {
            tokio::time::timeout(SHUTDOWN_GRACE, task)
                .await
                .map_err(|_| ServerError::ShutdownTimeout("in-flight HTTP requests"))??
                .map_err(ServerError::Serve)?;
        }
        tracing::info!("senate server stopped");
        Ok(())
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if self.supervisor_task.is_some() {
            // Not shut down explicitly: dropping `stop_http` starts the HTTP
            // shutdown; stop the tree so open streams end too.
            self.supervisor.stop(Some("server handle dropped".into()));
        }
    }
}
