//! The Senate server: the supervision tree over the log, the HTTP/SSE API
//! for bridges (`senate_core::api`), and [`ServerHub`], the
//! [`senate_core::Hub`] that in-process components (a gateway, the Speaker)
//! share with the HTTP layer.
//!
//! # Who is calling, and as whom
//!
//! A bearer token names a **person**. The person then acts as themselves or
//! as one of their own agents (`wendell/claude`); each request names which,
//! and the server checks it with
//! [`Roster::may_act_as`](senate_core::Roster::may_act_as). An agent not yet
//! on the roster is seated on its first contact. Presence, leases, cursors
//! and heartbeats are all per agent path.
//!
//! # Seams
//!
//! - [`Authenticate`]: bearer token to person. Default [`StaticTokens`].
//! - The roster: [`ServerBuilder::roster`] takes a ready
//!   [`Roster`](senate_core::Roster); [`ServerHub::set_seniors`] changes the
//!   people later. Default: the configuration's `[roster]`.
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use senate_server::{Server, ServerConfig};
//!
//! let config = ServerConfig::load("senate-server.toml".as_ref())?;
//! let store = senate_log::connect(&config.database_url).await?;
//! let running = Server::start(config, store).await?;
//! // Hand `running.hub.clone()` to the gateway and the Speaker here.
//! running.shutdown().await?;
//! # Ok(())
//! # }
//! ```

mod actors;
mod auth;
mod config;
mod http;
mod hub;
mod rules;
mod seats;
mod server;

pub use auth::{Authenticate, StaticTokens, bearer_token};
pub use config::{ConfigError, DEFAULT_PRESENCE_GRACE_SECS, ServerConfig, TokenConfig};
pub use hub::{BROADCAST_CAPACITY, LiveStream, STREAM_BUFFER, ServerHub};
pub use server::{Running, Server, ServerBuilder, ServerError};
