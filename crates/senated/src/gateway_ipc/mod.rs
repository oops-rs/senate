//! A private platform process attached to this Senate via a local Unix socket.
mod bind;
#[cfg(test)]
mod conformance;
mod transport;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, ensure};
use async_trait::async_trait;
use senate_core::{ChatSurface, Locale, Name};
use senate_log::LogStore;
use serde::{Deserialize, Serialize};

use crate::{ChatGateway, Component, ComponentContext, ParkedGroup, SenatedConfig};
use bind::InboundHandler;
use transport::RemoteSurface;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    socket: PathBuf,
    platform: Name,
    token_env: String,
}

/// Whether a platform adapter is attached to the gateway socket, and which
/// bound groups the gateway cannot join by itself, as the running server
/// last wrote it to [`GatewayStatus::FILE`] in its `data_dir`: on start, on
/// every attach and detach, and whenever the parked groups change.
/// `senated doctor` reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayStatus {
    pub platform: String,
    pub connected: bool,
    pub at: chrono::DateTime<chrono::Utc>,
    /// Bound groups whose membership waits on someone
    /// ([`ChatGateway::parked_groups`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parked: Vec<ParkedGroup>,
}

impl GatewayStatus {
    pub const FILE: &'static str = "chat_gateway.json";

    fn now(platform: &str, connected: bool, parked: Vec<ParkedGroup>) -> Self {
        Self {
            platform: platform.to_owned(),
            connected,
            at: chrono::Utc::now(),
            parked,
        }
    }

    /// Best effort: the status is informational.
    fn write(&self, path: &Path) {
        let text = serde_json::to_string_pretty(self).unwrap_or_default();
        if let Err(e) = std::fs::write(path, text) {
            tracing::debug!("{}: {e}", path.display());
        }
    }

    /// What was last written in `data_dir`, if anything readable.
    pub fn read(data_dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(data_dir.join(Self::FILE)).ok()?;
        serde_json::from_str(&text).ok()
    }
}

pub(crate) struct RemoteGateway {
    settings: Settings,
    token: String,
    surface: Arc<RemoteSurface>,
    pub(crate) gateway: ChatGateway,
}

impl RemoteGateway {
    pub(crate) fn from_config(config: &SenatedConfig) -> anyhow::Result<Option<Self>> {
        let Some(value) = config.server.extra.get("chat_gateway") else {
            return Ok(None);
        };
        let settings: Settings = value
            .clone()
            .try_into()
            .context("invalid chat_gateway settings")?;
        ensure!(
            settings.socket.is_absolute(),
            "socket must be an absolute path"
        );
        let token = std::env::var(&settings.token_env)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .context("the configured gateway token environment variable is empty or missing")?;
        let surface = RemoteSurface::new(settings.platform.to_string());
        let gateway = ChatGateway::new(surface.clone());
        Ok(Some(Self {
            settings,
            token,
            surface,
            gateway,
        }))
    }
}

impl RemoteGateway {
    /// Keep [`GatewayStatus`] in `data_dir` from now on. Only the server
    /// that serves the socket does: reading the configuration
    /// ([`Self::from_config`]) writes nothing.
    pub(crate) fn record_status_in(&self, data_dir: &Path) {
        self.surface.record_to(data_dir.join(GatewayStatus::FILE));
    }

    /// The socket the adapter connects to.
    pub(crate) fn socket(&self) -> &Path {
        &self.settings.socket
    }
}

pub(crate) struct Adapter {
    pub(crate) remote: RemoteGateway,
    pub(crate) store: Arc<dyn LogStore>,
    pub(crate) surface: Arc<dyn ChatSurface>,
    pub(crate) locale: Locale,
    pub(crate) builtin: bool,
}

#[async_trait]
impl Component for Adapter {
    fn name(&self) -> &str {
        "chat-gateway-ipc"
    }

    async fn run(&self, ctx: ComponentContext) -> anyhow::Result<()> {
        let handler = Arc::new(InboundHandler::new(
            self.remote.gateway.clone(),
            ctx.hub,
            self.store.clone(),
            self.remote.settings.platform.to_string(),
            self.surface.clone(),
            self.locale,
            self.builtin,
        ));
        // Keep the status file's parked groups current while serving.
        let mut parked = self.remote.gateway.watch_parked();
        let surface = self.remote.surface.clone();
        let stop = ctx.shutdown.clone();
        tokio::spawn(async move {
            loop {
                surface.record_parked(parked.borrow_and_update().clone());
                tokio::select! {
                    () = stop.cancelled() => break,
                    changed = parked.changed() => if changed.is_err() { break },
                }
            }
        });
        transport::serve(
            &self.remote.settings.socket,
            &self.remote.token,
            self.remote.surface.clone(),
            handler,
            ctx.shutdown,
        )
        .await
    }
}
