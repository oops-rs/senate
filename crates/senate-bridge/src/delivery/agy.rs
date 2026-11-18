//! Opt-in local Antigravity delivery through its bundled agentapi client.

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::process::Command;

use super::{Batch, DeliveryAdapter, DeliveryOutcome};
use crate::session::{Runtime, Session};

/// SHA-256 of the per-launch token provisioned by the operator. Hooks
/// receive the proof, and the daemon never persists the token itself.
pub const PROOF_ENV: &str = "SENATE_AGY_ENDPOINT_PROOF";

pub fn is_proof(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Reported explicitly by the operator to the private bridge socket. Held
/// only in memory and deliberately redacted from derived IPC request debug.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Endpoint {
    pub address: SocketAddr,
    pub token: String,
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoint")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl Endpoint {
    pub fn matches_proof(&self, proof: Option<&str>) -> bool {
        proof.is_some_and(|value| value == hex::encode(Sha256::digest(self.token.as_bytes())))
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.address.ip().is_loopback() && self.address.port() != 0,
            "Antigravity delivery needs a loopback address and a nonzero port"
        );
        anyhow::ensure!(
            !self.token.is_empty()
                && self.token.len() <= 512
                && self.token.bytes().all(|b| b.is_ascii_graphic()),
            "invalid Antigravity endpoint token"
        );
        Ok(())
    }
}

#[derive(Clone)]
pub struct AgyAdapter {
    endpoints: Arc<Mutex<HashMap<String, Endpoint>>>,
    executable: PathBuf,
    timeout: Duration,
}

impl AgyAdapter {
    pub fn new(timeout: Duration) -> Self {
        Self {
            endpoints: Arc::default(),
            executable: std::env::var_os("SENATE_AGY_BIN")
                .unwrap_or_else(|| "agy".into())
                .into(),
            timeout,
        }
    }

    fn endpoint(&self, session_id: &str) -> Option<Endpoint> {
        self.endpoints
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .cloned()
    }

    pub fn register(&self, session_id: String, endpoint: Endpoint) {
        self.endpoints
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id, endpoint);
    }

    pub fn forget(&self, session_id: &str) {
        self.endpoints
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
    }

    async fn call(&self, endpoint: &Endpoint, args: &[&str]) -> anyhow::Result<Value> {
        let mut command = Command::new(&self.executable);
        command
            .arg("agentapi")
            .args(args)
            .env_clear()
            .env("ANTIGRAVITY_LS_ADDRESS", endpoint.address.to_string())
            .env("ANTIGRAVITY_CSRF_TOKEN", &endpoint.token)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        let output = tokio::time::timeout(self.timeout, command.output())
            .await
            .map_err(|_| anyhow::anyhow!("Antigravity agentapi timed out"))?
            .map_err(|_| anyhow::anyhow!("could not start Antigravity agentapi"))?;
        // Agentapi may echo argument text or local details in errors.
        anyhow::ensure!(
            output.status.success(),
            "Antigravity agentapi rejected the request"
        );
        let reply: Value = serde_json::from_slice(&output.stdout)
            .map_err(|_| anyhow::anyhow!("invalid Antigravity agentapi response"))?;
        anyhow::ensure!(
            reply.get("error").is_none_or(Value::is_null),
            "Antigravity agentapi rejected the request"
        );
        Ok(reply)
    }

    pub async fn verify(&self, endpoint: &Endpoint, id: &str) -> anyhow::Result<()> {
        endpoint.validate()?;
        let reply = self
            .call(endpoint, &["get-conversation-metadata", id])
            .await?;
        anyhow::ensure!(
            reply
                .pointer("/response/conversationMetadata/metadata/rootConversationId")
                .and_then(Value::as_str)
                == Some(id),
            "endpoint did not confirm the Antigravity root conversation"
        );
        Ok(())
    }
}

#[async_trait]
impl DeliveryAdapter for AgyAdapter {
    fn name(&self) -> &'static str {
        "agy-agentapi"
    }

    fn can_push(&self, session: &Session) -> bool {
        session.runtime == Runtime::Agy
            && self
                .endpoint(&session.session_id)
                .is_some_and(|endpoint| endpoint.matches_proof(session.token.as_deref()))
    }

    async fn deliver(&self, session: &Session, batch: &Batch) -> DeliveryOutcome {
        if session.runtime != Runtime::Agy {
            return DeliveryOutcome::unavailable("not an Antigravity session");
        }
        let Some(id) = session.runtime.native_id(&session.session_id) else {
            return DeliveryOutcome::unavailable("invalid Antigravity session key");
        };
        let Some(endpoint) = self
            .endpoint(&session.session_id)
            .filter(|e| e.matches_proof(session.token.as_deref()))
        else {
            return DeliveryOutcome::unavailable("no registered Antigravity endpoint");
        };
        match self
            .call(&endpoint, &["send-message", id, &batch.text])
            .await
        {
            Ok(reply)
                if reply
                    .pointer("/response/sendMessage/recipientId")
                    .and_then(Value::as_str)
                    == Some(id) =>
            {
                DeliveryOutcome::Injected
            }
            Ok(_) => DeliveryOutcome::unavailable("Antigravity did not confirm this recipient"),
            Err(e) => DeliveryOutcome::unavailable(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::os::unix::fs::PermissionsExt;

    use chrono::Utc;

    use super::*;
    use crate::agent::AgentSource;
    use crate::session::SessionState;

    #[tokio::test]
    async fn only_an_authenticated_matching_native_endpoint_can_acknowledge_a_push() {
        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("agy");
        std::fs::write(
            &helper,
            r#"#!/bin/sh
[ "$ANTIGRAVITY_LS_ADDRESS" = 127.0.0.1:47120 ] || exit 1
[ "$ANTIGRAVITY_CSRF_TOKEN" = ephemeral-test-value ] || exit 1
[ "$1" = agentapi ] || exit 1
case "$2" in
  get-conversation-metadata)
    printf '{"response":{"conversationMetadata":{"metadata":{"rootConversationId":"root"}}}}'
    ;;
  send-message)
    [ "$4" = "test payload" ] || exit 1
    printf '{"response":{"sendMessage":{"recipientId":"root"}}}'
    ;;
  *) exit 1 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut adapter = AgyAdapter::new(Duration::from_secs(2));
        adapter.executable = helper;
        let endpoint = Endpoint {
            address: "127.0.0.1:47120".parse().unwrap(),
            token: "ephemeral-test-value".into(),
        };
        assert!(adapter.verify(&endpoint, "root").await.is_ok());
        assert!(adapter.verify(&endpoint, "another").await.is_err());
        let invalid = Endpoint {
            token: "wrong".into(),
            ..endpoint.clone()
        };
        assert!(adapter.verify(&invalid, "root").await.is_err());
        let now = Utc::now();
        let mut session = Session {
            session_id: "agy:root".into(),
            runtime: Runtime::Agy,
            agent: "agy-root".parse().unwrap(),
            agent_source: AgentSource::Session,
            cwd: temp.path().to_path_buf(),
            git_root: None,
            branch: None,
            inferred_committee: None,
            committee_source: None,
            joined_committee: None,
            socket: None,
            token: Some(hex::encode(Sha256::digest(endpoint.token.as_bytes()))),
            claude_pid: None,
            transcript_path: None,
            state: SessionState::Live,
            started_at: now,
            last_seen: now,
            socket_ok: None,
        };
        let batch = Batch {
            committee: "committee".parse().unwrap(),
            events: vec![],
            text: "test payload".into(),
        };
        assert!(!adapter.can_push(&session));
        adapter.register(session.session_id.clone(), endpoint.clone());
        assert!(adapter.can_push(&session));
        assert_eq!(
            adapter.deliver(&session, &batch).await,
            DeliveryOutcome::Injected
        );

        session.session_id = "agy:another".into();
        assert!(!adapter.can_push(&session));
        adapter.register(session.session_id.clone(), endpoint);
        assert_ne!(
            adapter.deliver(&session, &batch).await,
            DeliveryOutcome::Injected
        );
        assert!(!format!("{invalid:?}").contains("wrong"));
        session.token = None;
        assert!(!adapter.can_push(&session));
    }
}
