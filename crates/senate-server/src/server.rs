//! Starting and stopping the server.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::http::HeaderValue;
use ractor::{Actor, ActorCell};
use senate_core::chat_sim::ChatSimulator;
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
            simulator: None,
            observe: None,
            local_person: None,
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
    simulator: Option<Arc<dyn ChatSimulator>>,
    observe: Option<Arc<senate_observe::Registry>>,
    local_person: Option<senate_core::Name>,
}

impl ServerBuilder {
    /// Permit this persona on the in-memory router only.
    pub fn local_person(mut self, person: senate_core::Name) -> Self {
        self.local_person = Some(person);
        self
    }
    /// Mount the machine-owned observer on its embedded loopback listener.
    pub fn observe(mut self, registry: Arc<senate_observe::Registry>) -> Self {
        self.observe = Some(registry);
        self
    }

    /// Serve simulated chat (`senate chat send`, [`senate_core::chat_sim`])
    /// through `simulator`, the mounted chat gateway. Without one, those
    /// requests are answered that this Senate has no chat platform.
    pub fn chat_simulator(mut self, simulator: Arc<dyn ChatSimulator>) -> Self {
        self.simulator = Some(simulator);
        self
    }

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
            simulator,
            observe,
            local_person,
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
        for q in aliases.iter() {
            if roster.knows(&q.alias) || !roster.is_agent(&q.primary) {
                return Err(ServerError::Aliases(format!(
                    "invalid stored alias {q} for {}",
                    q.primary
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
        let workers = local_person
            .as_ref()
            .map(|_| Arc::new(http::worker::Book::new(store.clone())));
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
        // Seats are known once the writers run: only now can the
        // Senate-wide aliases of before move to their agents' committees.
        hub.migrate_global_aliases()
            .await
            .map_err(|e| ServerError::Aliases(e.to_string()))?;

        let embedded = listener.is_some();
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

        let mut app = http::router(AppState {
            machine: local_person.is_some(),
            workers,
            hub: hub.clone(),
            auth,
            epoch: epoch_header,
            ping_every: config.sse_ping_interval(),
            triaged,
            simulator,
        });
        let local_bridge = local_person.map(|person| LocalBridge {
            app: app
                .clone()
                .layer(axum::Extension(http::LocalPerson(person))),
        });
        let observe = observe.filter(|registry| embedded && registry.address() == local_addr);
        let observe_enabled = observe.is_some();
        if let Some(registry) = &observe {
            registry.attach_senate(Arc::new(crate::observe::Source::new(&hub, registry)));
            app = app.merge(senate_observe::router(registry.clone()));
        }
        let (stop_http, stopped) = oneshot::channel::<()>();
        let http_task = tokio::spawn(async move {
            let shutdown = async move {
                // Either an explicit shutdown or `Running` was dropped.
                if stopped.await.is_err() {
                    tracing::debug!("server handle dropped; shutting down HTTP");
                }
                if let Some(registry) = observe {
                    registry.shutdown_observation();
                }
            };
            if embedded && local_addr.ip().is_loopback() {
                serve_embedded_http(listener, app, observe_enabled, shutdown).await;
                Ok(())
            } else {
                axum::serve(listener, app)
                    .with_graceful_shutdown(shutdown)
                    .await
            }
        });
        tracing::info!(%local_addr, "senate server listening");

        Ok(Running {
            local_bridge,
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

// The bridge holds an open SSE stream per seat. Hyper's default limit of
// 200 streams queues excess seats forever when all the streams are SSE.
// Change that limit only on the machine-owned loopback listener.
const EMBEDDED_CONNECTION_LIMIT: usize = 64;
const FIRST_HTTP_REQUEST: Duration = Duration::from_secs(5);

async fn serve_embedded_http(
    mut listener: TcpListener,
    app: axum::Router,
    observe_enabled: bool,
    shutdown: impl Future<Output = ()>,
) {
    use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
    use hyper_util::server::{conn::auto, graceful::GracefulShutdown};
    use hyper_util::service::TowerToHyperService;

    let connections = Arc::new(tokio::sync::Semaphore::new(EMBEDDED_CONNECTION_LIMIT));
    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder.http2().max_concurrent_streams(8192);
    if observe_enabled {
        builder
            .http1()
            .timer(TokioTimer::new())
            .header_read_timeout(FIRST_HTTP_REQUEST);
        builder
            .http2()
            .timer(TokioTimer::new())
            .keep_alive_interval(Duration::from_secs(15))
            .keep_alive_timeout(Duration::from_secs(5));
    }
    let builder = Arc::new(builder);
    let graceful = GracefulShutdown::new();
    tokio::pin!(shutdown);
    loop {
        let (stream, address) = tokio::select! {
            // Axum's listener retries accept errors, including EMFILE/ENFILE.
            accepted = axum::serve::Listener::accept(&mut listener) => accepted,
            _ = &mut shutdown => break,
        };
        let permit = if observe_enabled {
            let Ok(permit) = connections.clone().try_acquire_owned() else {
                continue;
            };
            Some(permit)
        } else {
            None
        };
        let mut connection_app = app
            .clone()
            .layer(axum::Extension(axum::extract::ConnectInfo(address)));
        let first = Arc::new(tokio::sync::Notify::new());
        if observe_enabled {
            let first = first.clone();
            connection_app = connection_app.layer(axum::middleware::from_fn(
                move |request: axum::extract::Request, next: axum::middleware::Next| {
                    let first = first.clone();
                    async move {
                        first.notify_one();
                        next.run(request).await
                    }
                },
            ));
        }
        let builder = builder.clone();
        let service = TowerToHyperService::new(connection_app);
        let watcher = graceful.watcher();
        tokio::spawn(async move {
            let _permit = permit;
            let connection = watcher.watch(builder.serve_connection(TokioIo::new(stream), service));
            tokio::pin!(connection);
            if observe_enabled {
                tokio::select! {
                    _ = &mut connection => return,
                    _ = tokio::time::sleep(FIRST_HTTP_REQUEST) => return,
                    _ = first.notified() => {}
                }
            }
            if let Err(error) = connection.await {
                tracing::trace!(%error, "embedded Senate HTTP connection ended");
            }
        });
    }
    drop(listener);
    if tokio::time::timeout(SHUTDOWN_GRACE, graceful.shutdown())
        .await
        .is_err()
    {
        tracing::warn!("embedded Senate HTTP connections did not shut down in time");
    }
}

/// A started server. Dropping it stops the server without waiting; prefer
/// [`Running::shutdown`].
pub struct Running {
    pub local_bridge: Option<LocalBridge>,
    pub hub: Arc<ServerHub>,
    pub local_addr: SocketAddr,
    /// The root of the supervision tree.
    pub supervisor: ActorCell,
    stop_http: Option<oneshot::Sender<()>>,
    http_task: Option<JoinHandle<std::io::Result<()>>>,
    supervisor_task: Option<JoinHandle<()>>,
}

/// A handle to the machine-private router. No socket or bearer grants access.
#[derive(Clone)]
pub struct LocalBridge {
    app: axum::Router,
}

impl LocalBridge {
    pub async fn send(
        &self,
        request: reqwest::Request,
    ) -> Result<reqwest::Response, std::io::Error> {
        use tower::ServiceExt as _;
        let url = request.url();
        let mut target = url.path().to_owned();
        if let Some(query) = url.query() {
            target.push('?');
            target.push_str(query);
        }
        let body = match request.body() {
            None => axum::body::Body::empty(),
            Some(body) => axum::body::Body::from(
                body.as_bytes()
                    .ok_or_else(|| {
                        std::io::Error::other("The local bridge expects a buffered request body")
                    })?
                    .to_vec(),
            ),
        };
        let mut local = axum::http::Request::builder()
            .method(request.method())
            .uri(target)
            .body(body)
            .map_err(std::io::Error::other)?;
        *local.headers_mut() = request.headers().clone();
        local
            .headers_mut()
            .remove(axum::http::header::AUTHORIZATION);
        let response = match self.app.clone().oneshot(local).await {
            Ok(response) => response,
            Err(never) => match never {},
        };
        Ok(response
            .map(|body| reqwest::Body::wrap_stream(body.into_data_stream()))
            .into())
    }
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

#[cfg(test)]
mod embedded_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    #[tokio::test]
    async fn an_h2_preface_without_a_request_releases_its_connection()
    -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let (stop, stopped) = oneshot::channel();
        let server = tokio::spawn(serve_embedded_http(
            listener,
            axum::Router::new(),
            true,
            async move {
                let _ = stopped.await;
            },
        ));
        let mut client = TcpStream::connect(address).await?;
        client
            .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
            .await?;
        let mut bytes = Vec::new();
        let result = tokio::time::timeout(
            FIRST_HTTP_REQUEST + Duration::from_secs(2),
            client.read_to_end(&mut bytes),
        )
        .await?;
        assert!(
            result.is_ok()
                || result.is_err_and(|error| error.kind() == std::io::ErrorKind::ConnectionReset)
        );
        let _ = stop.send(());
        server.await?;
        Ok(())
    }
}

#[cfg(test)]
mod cap_test {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    #[tokio::test]
    async fn the_embedded_listener_rejects_connections_past_its_cap()
    -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let (stop, stopped) = oneshot::channel();
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(|| async { axum::http::StatusCode::NO_CONTENT }),
        );
        let server = tokio::spawn(serve_embedded_http(listener, app, true, async move {
            let _ = stopped.await;
        }));
        let request = b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        let mut clients = Vec::new();
        for _ in 0..EMBEDDED_CONNECTION_LIMIT {
            let mut client = TcpStream::connect(address).await?;
            client.write_all(request).await?;
            let mut prefix = [0; 12];
            tokio::time::timeout(Duration::from_secs(2), client.read_exact(&mut prefix)).await??;
            assert_eq!(&prefix, b"HTTP/1.1 204");
            clients.push(client);
        }
        let mut extra = TcpStream::connect(address).await?;
        let result = tokio::time::timeout(Duration::from_secs(2), extra.read(&mut [0; 1])).await?;
        assert!(
            matches!(result, Ok(0))
                || result.is_err_and(|error| error.kind() == std::io::ErrorKind::ConnectionReset)
        );
        drop(clients);
        let _ = stop.send(());
        server.await?;
        Ok(())
    }
}
