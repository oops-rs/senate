//! The chat gateway's endpoint on Windows: a named pipe through
//! [`senate_bridge::local`], owner-only and refusing a second holder of
//! its name, so the Unix version's directory and stale-socket checks have
//! nothing left to guard. The listener removes its marker when dropped.

use std::path::Path;

use anyhow::{Context, Result};
use senate_bridge::local::Listener as UnixListener;

pub(super) struct BoundSocket {
    pub(super) listener: UnixListener,
}

impl BoundSocket {
    pub(super) async fn bind(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let listener = UnixListener::bind(path).context("binding chat gateway socket")?;
        Ok(Self { listener })
    }
}
