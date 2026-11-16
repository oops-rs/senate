//! Starting and stopping the server.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::http::HeaderValue;
use ractor::{Actor, ActorCell};
use senate_core::Roster;
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
///   the server runs with [`ServerHub::set_seniors`]. Agents are seated as
///   they connect either way.
pub struct ServerBuilder {
    config: ServerConfig,
    store: Arc<dyn LogStore>,
    auth: Option<Arc<dyn Authenticate>>,
    roster: Option<Roster>,
    triaged: bool,
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

    /// Spawn the supervision tree over the store, bind `config.listen`, and
    /// serve the HTTP API. The returned [`Running::hub`] is the same hub the
    /// HTTP layer uses; attach the gateway and the Speaker to it.
    pub async fn start(self) -> Result<Running, ServerError> {
        let Self {
            config,
            store,
            auth,
            roster,
            triaged,
        } = self;
        config.validate_intervals()?;
        let (auth, roster): (Arc<dyn Authenticate>, Roster) = match (auth, roster) {
            (Some(auth), Some(roster)) => (auth, roster),
            (auth, roster) => {
                // Only what is not replaced is read from, and so checked in,
                // the configuration.
                let roster = match roster {
                    Some(roster) => roster,
                    None => config.roster()?,
                };
                let auth = match auth {
                    Some(auth) => auth,
                    None => Arc::new(StaticTokens::new(&config.tokens)?),
                };
                (auth, roster)
            }
        };
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
        }));

        // Every committee's writer runs its presence leases, so start them all
        // now: after a restart an agent that never comes back must still go
        // `disconnected` and `absent`. Starting one also seats again the
        // agents its log has seen, so this finishes before anything is
        // served: the roster the hub hands the Speaker, and the agents
        // `/v1/me` lists, are complete from the first request.
        hub.clone().start_writers().await;

        let listener = match TcpListener::bind(&config.listen).await {
            Ok(listener) => listener,
            Err(source) => {
                supervisor.stop(None);
                return Err(ServerError::Bind {
                    addr: config.listen.clone(),
                    source,
                });
            }
        };
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
