//! The Senate daemon, `senated`, and the library a private layer builds its
//! own daemon from.
//!
//! The daemon is the same at every level. On a server (`senated --config`)
//! it is one Senate: the assembly below. On a developer machine
//! (`senated --machine`) it is a [`Machine`]: the Senates hosted there (a
//! local top Senate for using Senate alone, a Senate one level down per
//! presiding agent), each an assembly with no built-in Speaker, and the
//! machine's side of every Senate it serves as a supervised component.
//!
//! Senate has no chat platform or agent gateway of its own. Layers that
//! connect it to a company's systems are built on this crate instead of
//! forking it: they hand [`Assembly`] what they bring, and get a daemon that
//! is the `senated` binary plus their parts.
//!
//! An assembled daemon is:
//!
//! - **the log and the server** (`senate-log`, `senate-server`): the store
//!   `database_url` names (`memory:`, `sqlite:<path>`, `postgres://…`), the
//!   HTTP/SSE API bridges connect to, authentication ([`Authenticate`],
//!   default static tokens from `[[tokens]]`) and the roster (default
//!   `[roster]`, with agents seated as they connect);
//! - **the built-in Speaker** (`senate-speaker`) when `[speaker]` is
//!   configured, with its [`Brain`](senate_speaker::Brain) (default: basis)
//!   and, with `[work]`, its runner and forge;
//! - **at most one chat platform** ([`ChatMount`]: the surface the Speaker
//!   posts through and its unbound mentions), usually through
//!   [`ChatGateway`], which turns what the platform delivers into the log's
//!   `chat` events;
//! - **any number of [`Component`]s**: anything else that runs on the
//!   [`Hub`](senate_core::Hub), such as an agent-to-agent entry point.
//!
//! The Speaker and the components are supervised together: one that fails
//! is started again (each rebuilds from the log), and a set that keeps
//! failing stops the daemon (see [`component`]).
//!
//! # With no chat platform
//!
//! A daemon with a Speaker and no chat platform is complete: agents ask
//! each other through the log, the Speaker triages, answers for an absent
//! agent from its own checkpoint, and answers agents that address it. It
//! speaks through [`NoChat`], where every post is unsupported and so
//! skipped: without a platform, no group is ever bound to be posted into.
//!
//! # Several platforms
//!
//! Not supported: the built-in Speaker speaks on one platform, so
//! [`Assembly::start`] refuses a second [`ChatMount`]. A layer that needs
//! two can route both behind one [`ChatSurface`](senate_core::ChatSurface)
//! of its own.
//!
//! # Example
//!
//! ```no_run
//! # async fn run(
//! #     platform: std::sync::Arc<dyn senate_core::ChatSurface>,
//! #     my_entry_point: impl senated::Component,
//! # ) -> anyhow::Result<()> {
//! use senated::{Assembly, ChatGateway, SenatedConfig};
//!
//! let config = SenatedConfig::load("senated.toml".as_ref())?;
//! let gateway = ChatGateway::new(platform);
//! let daemon = Assembly::new(config)
//!     .chat_gateway(gateway.clone())?
//!     .component(my_entry_point)
//!     .start()
//!     .await?;
//! daemon.run_until_signal().await
//! # }
//! ```
//!
//! # Testing
//!
//! The `testing` feature adds [`testing::FakeChat`], a chat platform in
//! memory that records everything the Speaker does on it and lets a test
//! speak as anyone in any group.

mod assembly;
pub mod chat;
pub mod component;
pub mod config;
mod empty_hub;
pub mod machine;
mod speaker;
#[cfg(feature = "testing")]
pub mod testing;

pub use assembly::{Assembly, AssemblyError, Senated};
pub use chat::{
    Addressing, ChatGateway, ChatMount, Delivered, GatewayError, Inbound, PlatformMention,
    UNBOUND_CAPACITY, sender_address,
};
pub use component::{Component, ComponentContext, GaveUp, component_fn};
pub use config::{ForgeSection, SenatedConfig, SenatedConfigError, WorkSection};
pub use machine::{HostedSenate, Machine, MachineError};
pub use senate_server::{Authenticate, StaticTokens};
pub use speaker::{NO_CHAT_PLATFORM, NoChat};
