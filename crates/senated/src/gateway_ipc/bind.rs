//! Model-free first binding. The saved receipt records the authorization
//! decision and exact reply before either a log append or a platform post.
//! A replay then resumes that decision even after the group has been bound.

use std::sync::Arc;

use anyhow::{Context, Result};
use senate_core::{
    Address, BindingAction, BindingPayload, ChatSurface, CommitteeId, EventId, GroupRole, Hub,
    Kind, Locale, Mirror, MirrorRef, NewEvent, PostTarget, Source, SurfaceError,
};
use senate_log::LogStore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Addressing, ChatGateway, Inbound, sender_address};

/// An immutable defect in this input. Retrying it cannot succeed.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(super) struct RejectedInput(pub(super) &'static str);

pub(super) struct InboundHandler {
    gateway: ChatGateway,
    hub: Arc<dyn Hub>,
    store: Arc<dyn LogStore>,
    platform: String,
    surface: Arc<dyn ChatSurface>,
    locale: Locale,
    builtin: bool,
    // A reconnect can overlap a task from the closing connection.
    delivery: tokio::sync::Mutex<()>,
}

#[derive(Serialize, Deserialize)]
struct Pending {
    message: Inbound,
    event_id: EventId,
    locale: Locale,
    binding: Option<(CommitteeId, Address)>,
    reply: String,
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    fingerprint: String,
    outcome: Outcome,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Outcome {
    Posted {
        message_ref: String,
        thread_ref: Option<String>,
    },
    Refused {
        reason: String,
    },
}

impl InboundHandler {
    pub(super) fn new(
        gateway: ChatGateway,
        hub: Arc<dyn Hub>,
        store: Arc<dyn LogStore>,
        platform: String,
        surface: Arc<dyn ChatSurface>,
        locale: Locale,
        builtin: bool,
    ) -> Self {
        Self {
            gateway,
            hub,
            store,
            platform,
            surface,
            locale,
            builtin,
            delivery: Default::default(),
        }
    }

    // There is one authenticated, ordered adapter stream per platform. One
    // pending slot suffices until both the remote result and mirrors commit.
    fn pending_key(&self) -> String {
        format!("gateway:pending-bind:v1:{}", self.platform)
    }

    fn receipt_key(&self, message: &Inbound) -> Result<String> {
        let identity = digest(&[&self.platform, &message.group_id, &message.message_ref])?;
        Ok(format!("gateway:bind:v1:{identity}"))
    }

    async fn completed(&self, key: &str, message: &Inbound) -> Result<bool> {
        let Some(value) = self.store.registry_value(key).await? else {
            return Ok(false);
        };
        let receipt: Receipt = serde_json::from_value(value).context("invalid bind receipt")?;
        if receipt.fingerprint != digest(message)? {
            return Err(RejectedInput("conflicting replay of platform message").into());
        }
        Ok(true)
    }

    /// An authenticated adapter is back. The original message may never
    /// arrive again, so resume before processing fresh deliveries.
    pub(super) async fn resume(&self) -> Result<()> {
        let _delivery = self.delivery.lock().await;
        self.resume_locked().await
    }

    async fn resume_locked(&self) -> Result<()> {
        let Some(value) = self.store.registry_value(&self.pending_key()).await? else {
            return Ok(());
        };
        let pending = serde_json::from_value(value).context("invalid pending bind")?;
        self.settle(&pending).await
    }

    async fn settle(&self, pending: &Pending) -> Result<()> {
        let key = self.receipt_key(&pending.message)?;
        self.gateway.observe_inbound(&pending.message).await?;
        if !self.completed(&key, &pending.message).await? {
            let outcome = self.finish(pending).await?;
            let receipt = Receipt {
                fingerprint: digest(&pending.message)?,
                outcome,
            };
            self.store
                .put_registry(&key, Some(serde_json::to_value(receipt)?))
                .await?;
        }
        // The durable receipt wins if a crash leaves these two rows present.
        self.store.put_registry(&self.pending_key(), None).await?;
        Ok(())
    }

    pub(super) async fn deliver(&self, message: Inbound) -> Result<()> {
        if message.group_id.trim().is_empty() {
            return Err(RejectedInput("empty inbound group_id").into());
        }
        if message.message_ref.trim().is_empty() {
            return Err(RejectedInput("empty inbound message_ref").into());
        }
        if message.sender.trim().is_empty() {
            return Err(RejectedInput("empty inbound sender").into());
        }
        let _delivery = self.delivery.lock().await;
        self.resume_locked().await?;
        let key = self.receipt_key(&message)?;
        // The first decision still applies after the group or roster changes.
        if self.completed(&key, &message).await? {
            self.gateway.observe_inbound(&message).await?;
            return Ok(());
        }
        if self.gateway.observe_inbound(&message).await?.is_some() {
            self.gateway.deliver(message).await?;
            return Ok(());
        }

        let Some(raw) = bind_argument(&message) else {
            // Explicit first binds use the durable path even with a built-in
            // Speaker. Other unbound messages can still reach its inbox.
            if self.builtin {
                self.gateway.deliver(message).await?;
            }
            return Ok(());
        };
        let roster = self.hub.roster().await?;
        let sender = sender_address(&roster, &self.platform, &message.sender);
        let (binding, reply) = if !roster.is_person_address(&sender) {
            (
                None,
                self.locale
                    .pick(
                        "只有名册上的人可以绑定群。",
                        "Only a person on the roster can bind a group.",
                    )
                    .into(),
            )
        } else if let Ok(committee) = CommitteeId::new(raw) {
            let reply = self.locale.pick(
                format!("已绑定到委员会 {committee}。"),
                format!("Bound to committee {committee}."),
            );
            (Some((committee, sender)), reply)
        } else {
            (
                None,
                self.locale
                    .pick("用法：/bind <committee>。", "Usage: /bind <committee>.")
                    .into(),
            )
        };
        let pending = Pending {
            message,
            event_id: EventId::new(),
            locale: self.locale,
            binding,
            reply,
        };
        self.store
            .put_registry(&self.pending_key(), Some(serde_json::to_value(&pending)?))
            .await?;
        self.settle(&pending).await
    }

    async fn finish(&self, pending: &Pending) -> Result<Outcome> {
        let message = &pending.message;
        let root = if let Some((committee, sender)) = &pending.binding {
            let source = Source {
                platform: self.platform.clone(),
                group_id: message.group_id.clone(),
                group_name: message.group_name.clone(),
                simulated: false,
                simulated_by: None,
            };
            let label = source.group_name.as_deref().unwrap_or(&source.group_id);
            let event = NewEvent::new(
                committee.clone(),
                Kind::Binding,
                Address::Speaker,
                pending
                    .locale
                    .pick(format!("绑定群 {label}"), format!("Bound group {label}")),
            )
            .with_id(pending.event_id.clone())
            .on_behalf_of(sender.clone())
            .with_binding(&BindingPayload {
                action: BindingAction::Bind,
                platform: source.platform.clone(),
                group_id: source.group_id.clone(),
                group_name: source.group_name.clone(),
                role: GroupRole::People,
            })
            // The membership reconciler can now adopt this group again after a
            // restart, even if nothing was said between the bind and shutdown.
            .with_mirror(Mirror {
                platform: self.platform.clone(),
                message_ref: message.message_ref.clone(),
                thread_ref: message.thread_ref.clone(),
            })
            .with_source(source);
            let seq = self.hub.append(event).await?.event.seq;
            // A follow-up may name the person's original message as its
            // thread root, or name the bot's confirmation as its parent.
            self.hub
                .put_mirror(MirrorRef {
                    committee: committee.clone(),
                    seq,
                    platform: self.platform.clone(),
                    target: format!("{}#bind-origin", message.group_id),
                    message_ref: message.message_ref.clone(),
                    thread_ref: message.thread_ref.clone(),
                })
                .await?;
            self.gateway.observe_inbound(message).await?;
            Some((committee.clone(), seq))
        } else {
            None
        };

        let target = PostTarget::Reply {
            message_ref: message.message_ref.clone(),
            in_thread: true,
        };
        let operation_id = format!("senate:bind:{}", pending.event_id);
        let outcome = match self
            .surface
            .post(target, &pending.reply, &operation_id)
            .await
        {
            Ok(posted) => {
                if let Some((committee, seq)) = root {
                    self.hub
                        .put_mirror(MirrorRef {
                            committee,
                            seq,
                            platform: self.platform.clone(),
                            target: format!("{}#bind-confirmation", message.group_id),
                            message_ref: posted.message_ref.clone(),
                            thread_ref: posted.thread_ref.clone(),
                        })
                        .await?;
                }
                Outcome::Posted {
                    message_ref: posted.message_ref,
                    thread_ref: posted.thread_ref,
                }
            }
            Err(error @ SurfaceError::Unavailable(_)) => return Err(error.into()),
            Err(error) => {
                // The provider returned a definitive failure. Recording it
                // prevents a permanently refused post from blocking delivery.
                tracing::warn!(%operation_id, %error, "could not post the binding reply");
                Outcome::Refused {
                    reason: error.to_string(),
                }
            }
        };
        Ok(outcome)
    }
}

fn bind_argument(message: &Inbound) -> Option<&str> {
    if message.addressing == Addressing::Ambient {
        return None;
    }
    let text = message.text.trim().strip_prefix(['/', '／'])?;
    let (verb, args) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    verb.eq_ignore_ascii_case("bind").then_some(args.trim())
}

fn digest(input: &impl Serialize) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(input)?)))
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::*;
    use crate::testing::{FakeChat, Message};
    use crate::{Assembly, SenatedConfig};

    #[tokio::test]
    async fn a_pending_receipt_preserves_the_decision_and_rejects_conflicting_replays() -> Result<()>
    {
        let dir = tempfile::tempdir()?;
        let config = SenatedConfig::from_toml_str(&format!(
            r#"
listen = "127.0.0.1:0"
database_url = "sqlite:{}"
[[roster.principals]]
handle = "alice"
accounts = [{{platform = "chat", id = "u_alice"}}]
"#,
            dir.path().join("log.db").display(),
        ))?;
        let inbound = Message::mention("group", "u_alice", "m1", "/bind work")
            .inbound()
            .clone();
        let committee = CommitteeId::new("work")?;

        // Shut down after the bind committed but before a confirmation was
        // possible. The next daemon has a new hub, gateway, adapter and locale.
        let store = senate_log::connect(&config.server.database_url).await?;
        let (surface, gateway) = FakeChat::new();
        let mounted = gateway.mount().context("expected fresh mount")?;
        let daemon = Assembly::new(config.clone())
            .store(store.clone())
            .component(gateway.clone())
            .start()
            .await?;
        let handler = InboundHandler::new(
            gateway,
            daemon.hub(),
            store,
            "chat".into(),
            mounted.surface,
            Locale::Zh,
            false,
        );
        surface.fail_next_posts(1);
        let retry = handler
            .deliver(inbound.clone())
            .await
            .err()
            .context("outage accepted")?;
        assert!(retry.is::<SurfaceError>());
        assert!(!retry.is::<RejectedInput>());
        assert_eq!(daemon.hub().bindings().await?.len(), 1);
        daemon.shutdown().await?;
        drop(handler);

        let store = senate_log::connect(&config.server.database_url).await?;
        let (surface, gateway) = FakeChat::new();
        let mounted = gateway.mount().context("expected fresh mount")?;
        let daemon = Assembly::new(config)
            .store(store.clone())
            .component(gateway.clone())
            .start()
            .await?;
        let handler = InboundHandler::new(
            gateway,
            daemon.hub(),
            store,
            "chat".into(),
            mounted.surface,
            Locale::En,
            false,
        );
        handler.resume().await?;
        assert_eq!(
            surface.posts().len(),
            1,
            "the adapter's reconnect alone finishes the confirmation"
        );
        let mut changed = inbound.clone();
        changed.text = "/bind other".into();
        let error = handler
            .deliver(changed)
            .await
            .err()
            .context("conflicting message was accepted")?;
        assert!(error.is::<RejectedInput>());
        assert!(error.to_string().contains("conflicting replay"));
        let mut invalid = inbound.clone();
        invalid.sender.clear();
        let invalid = handler
            .deliver(invalid)
            .await
            .err()
            .context("empty sender accepted")?;
        assert!(invalid.is::<RejectedInput>());
        assert_eq!(surface.posts().len(), 1);
        handler.deliver(inbound.clone()).await?;
        handler.deliver(inbound.clone()).await?;
        let bindings = daemon.hub().bindings().await?;
        assert_eq!(bindings.len(), 1);
        assert_eq!(
            bindings[0].mirror.as_ref().map(|m| m.message_ref.as_str()),
            Some("m1")
        );
        assert_eq!(surface.posts().len(), 1);
        assert_eq!(surface.posts()[0].markdown, "已绑定到委员会 work。");

        let reply = Message::reply(
            "group",
            "u_alice",
            "m2",
            "one follow-up",
            &surface.posts()[0].message_ref,
        );
        handler.deliver(reply.inbound().clone()).await?;
        handler.deliver(reply.inbound().clone()).await?;
        let events = daemon
            .hub()
            .read(&committee, senate_core::Seq::ZERO, 30)
            .await?;
        let chats: Vec<_> = events
            .iter()
            .filter(|event| event.kind == Kind::Chat)
            .collect();
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].thread, Some(bindings[0].seq));

        // Control: erase only the receipt from this temporary database.
        // The original command now follows the bound-group route into chat.
        let receipts = handler.store.registry("gateway:bind:v1:").await?;
        assert_eq!(receipts.len(), 1);
        handler.store.put_registry(&receipts[0].0, None).await?;
        handler.deliver(inbound).await?;
        let events = daemon
            .hub()
            .read(&committee, senate_core::Seq::ZERO, 30)
            .await?;
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == Kind::Chat && e.body == "/bind work")
                .count(),
            1
        );
        daemon.shutdown().await?;
        Ok(())
    }
}
