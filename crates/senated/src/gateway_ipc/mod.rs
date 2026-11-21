//! A private platform process attached to this Senate via a local Unix socket.
mod bind;
#[cfg(test)]
mod conformance;
mod transport;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, ensure};
use async_trait::async_trait;
use senate_core::{ChatSurface, Locale, Name};
use senate_log::LogStore;
use serde::Deserialize;

use crate::{ChatGateway, Component, ComponentContext, SenatedConfig};
use bind::InboundHandler;
use transport::RemoteSurface;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    socket: PathBuf,
    platform: Name,
    token_env: String,
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
