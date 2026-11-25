//! The Senate server: the supervision tree over the log, the HTTP/SSE API
//! for bridges (`senate_core::api`), and [`ServerHub`], the
//! [`senate_core::Hub`] that in-process components (a gateway, the Speaker)
//! share with the HTTP layer.
//!
//! # Who is calling, and as whom
//!
//! A bearer token names a **person**. The person then acts as themselves or
//! as one of their own agents (`claude`); each request names which,
//! and the server checks it with
//! [`Roster::may_act_as`](senate_core::Roster::may_act_as). An agent not yet
//! on the roster is seated on its first contact. Presence, leases, cursors
//! and heartbeats are all per agent path.
//!
//! # Who presides
//!
//! The Speaker's seat ([`SpeakerSeat`](senate_core::SpeakerSeat)) is the
//! roster's, plus whether the built-in Speaker is attached
//! ([`ServerBuilder::built_in_speaker`]). When a senator holds it, its
//! person's session may write as `speaker` over HTTP: what only the Speaker
//! writes, `on_behalf_of` answers, relays. A person's message for one of
//! several agents is then put to the Speaker to route, and
//! `GET .../docket` ([`ServerHub::docket`]) lists what awaits it.
//!
//! # Seams
//!
//! - [`Authenticate`]: bearer token to person. Default [`StaticTokens`].
//! - The roster: [`ServerBuilder::roster`] takes a ready
//!   [`Roster`](senate_core::Roster); [`ServerHub::set_principals`] changes the
//!   people later. Default: the configuration's `[roster]`.
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use senate_server::{Server, ServerConfig};
//!
//! let config = ServerConfig::load("senate-server.toml".as_ref())?;
//! let store = senate_log::connect(&config.database_url).await?;
//! let running = Server::start(config, store).await?;
//! // In-process components share `running.hub`; `senated::Assembly` wires them.
//! running.shutdown().await?;
//! # Ok(())
//! # }
//! ```

mod actors;
mod auth;
mod chairs;
mod config;
mod http;
mod hub;
mod office;
mod registry;
mod rules;
mod seats;
mod server;

pub use auth::{Authenticate, StaticTokens, bearer_token};
pub use config::{ConfigError, DEFAULT_PRESENCE_GRACE_SECS, ServerConfig, TokenConfig, redact_url};
pub use hub::{BROADCAST_CAPACITY, LiveStream, STREAM_BUFFER, ServerHub};
pub use office::{DEFAULT_BUILT_IN_NAME, default_built_in_name};
pub use server::{LocalBridge, Running, Server, ServerBuilder, ServerError};

mod observe;
