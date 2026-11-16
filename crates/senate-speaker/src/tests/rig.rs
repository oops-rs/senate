//! Shared scaffolding: a Speaker over the in-memory Hub and fake surface,
//! plus builders for the events the gateway and the bridges would append.

use std::sync::Arc;

use ractor::ActorRef;
use senate_core::{
    Account, Address, BindingAction, BindingPayload, CommitteeId, Event, Guest, Hub, Kind, Locale,
    Mirror, Name, NewEvent, Roster, SenatorPath, Senior, Seq, Source, UnboundMention,
};
use serde_json::json;
use tokio::sync::mpsc;

use super::hub::MemoryHub;
use super::surface::{FakeSurface, PLATFORM};
use crate::{CURSOR, ScriptedBrain, Speaker, SpeakerArgs, SpeakerConfig, SpeakerMsg};

pub(crate) const GROUP: &str = "backend";
pub(crate) const GROUP_NAME: &str = "退款 v2 · 后端群";
/// Not on the roster: what the gateway makes of an unknown sender.
pub(crate) const STRANGER: &str = "u_0123456789";

pub(crate) fn committee(name: &str) -> CommitteeId {
    CommitteeId::new(name).expect("committee id")
}

pub(crate) fn refund() -> CommitteeId {
    committee("refund-v2")
}

pub(crate) fn name(handle: &str) -> Name {
    Name::new(handle).expect("handle")
}

pub(crate) fn path(text: &str) -> SenatorPath {
    text.parse().expect("senator path")
}

/// `<handle>/claude`: each person's one agent unless a test seats more.
pub(crate) fn agent_path(handle: &str) -> SenatorPath {
    path(&format!("{handle}/claude"))
}

/// `senator::<handle>/claude`.
pub(crate) fn agent(handle: &str) -> Address {
    Address::Senator(agent_path(handle))
}

/// `senator::<handle>/<agent>`: one of several agents of a person.
pub(crate) fn agent_named(handle: &str, agent: &str) -> Address {
    Address::Senator(path(&format!("{handle}/{agent}")))
}

/// `senator::<handle>`: the person.
pub(crate) fn user(handle: &str) -> Address {
    Address::senior(&name(handle))
}

/// `guest::chat/<id>`: someone in chat who is not on the roster.
pub(crate) fn guest(id: &str) -> Address {
    Address::Guest(Guest::new(PLATFORM, id).expect("guest"))
}

/// What a sender is in the log: a handle on the roster is that person,
/// anything else (like [`STRANGER`]) a guest, as the gateway records them.
pub(crate) fn sender(from: &str) -> Address {
    let known = Name::new(from).is_ok_and(|handle| roster().senior(&handle).is_some());
    if known { user(from) } else { guest(from) }
}

/// pm, alice and bob, each with one agent `claude` seated, and each with an
/// account on the chat platform (`u_<handle>`).
pub(crate) fn roster() -> Roster {
    let senior = |handle: &str, names: &[&str]| Senior {
        handle: name(handle),
        names: names.iter().map(|n| (*n).to_owned()).collect(),
        accounts: vec![Account {
            platform: PLATFORM.into(),
            id: format!("u_{handle}"),
        }],
    };
    Roster::top(
        vec![
            senior("pm", &["PM"]),
            senior("alice", &["Alice", "爱丽丝"]),
            senior("bob", &["Bob"]),
        ],
        vec![agent_path("pm"), agent_path("alice"), agent_path("bob")],
    )
    .expect("roster")
}

pub(crate) fn source() -> Source {
    Source {
        platform: PLATFORM.into(),
        group_id: GROUP.into(),
        group_name: Some(GROUP_NAME.into()),
    }
}

pub(crate) fn bind_event(committee_id: &CommitteeId, group: &str) -> NewEvent {
    NewEvent::new(
        committee_id.clone(),
        Kind::Binding,
        Address::Speaker,
        "bind",
    )
    .with_binding(&BindingPayload {
        action: BindingAction::Bind,
        platform: PLATFORM.into(),
        group_id: group.into(),
        group_name: Some(GROUP_NAME.into()),
        role: senate_core::GroupRole::People,
    })
}

/// The Agent group `group` bound to `committee_id`, as the Speaker records an
/// adopted one.
pub(crate) fn agents_bind_event(committee_id: &CommitteeId, group: &str) -> NewEvent {
    NewEvent::new(
        committee_id.clone(),
        Kind::Binding,
        Address::Speaker,
        "bind",
    )
    .with_binding(&BindingPayload {
        action: BindingAction::Bind,
        platform: PLATFORM.into(),
        group_id: group.into(),
        group_name: Some("[Agents] refund-v2".into()),
        role: senate_core::GroupRole::Agents,
    })
}

/// A `chat` exactly as the gateway records a mention of the Speaker.
pub(crate) fn mention(from: &str, text: &str, message_ref: &str) -> NewEvent {
    mention_in(&refund(), GROUP, from, text, message_ref)
}

pub(crate) fn mention_in(
    committee_id: &CommitteeId,
    group: &str,
    from: &str,
    text: &str,
    message_ref: &str,
) -> NewEvent {
    NewEvent::new(committee_id.clone(), Kind::Chat, sender(from), text)
        .to(Address::Speaker)
        .with_source(Source {
            group_id: group.into(),
            ..source()
        })
        .with_mirror(Mirror {
            platform: PLATFORM.into(),
            message_ref: message_ref.into(),
            thread_ref: None,
        })
        .with_meta("addressing", json!("mention"))
}

/// A human message in the platform thread of log thread `root`, quoting
/// `quoting`, as the gateway records it (not addressed to the Speaker).
pub(crate) fn in_thread(
    from: &str,
    text: &str,
    message_ref: &str,
    root: Seq,
    thread_ref: Option<String>,
    quoting: &str,
) -> NewEvent {
    NewEvent::new(refund(), Kind::Chat, sender(from), text)
        .in_thread(root)
        .with_source(source())
        .with_mirror(Mirror {
            platform: PLATFORM.into(),
            message_ref: message_ref.into(),
            thread_ref,
        })
        .with_meta("addressing", json!("reply"))
        .with_meta("reply_to_message_ref", json!(quoting))
}

/// A message in a group bound to no committee, as the gateway hands it over.
pub(crate) fn unbound(from: &str, text: &str, message_ref: &str) -> UnboundMention {
    UnboundMention {
        platform: PLATFORM.into(),
        group_id: "random".into(),
        group_name: Some("杂谈".into()),
        message_ref: message_ref.into(),
        thread_ref: None,
        reply_to_message_ref: None,
        thread_root_message_ref: None,
        addressing: "mention".into(),
        sender: sender(from),
        text: text.into(),
    }
}

pub(crate) fn question(from: Address, to: Address, body: &str) -> NewEvent {
    NewEvent::new(refund(), Kind::Question, from, body).to(to)
}

pub(crate) fn answer(from: Address, thread: Seq, to: Address, body: &str) -> NewEvent {
    NewEvent::new(refund(), Kind::Answer, from, body)
        .in_thread(thread)
        .to(to)
}

/// The carried tests read the Speaker in Chinese; English has its own.
pub(crate) fn config() -> SpeakerConfig {
    config_in(Locale::Zh)
}

pub(crate) fn config_in(locale: Locale) -> SpeakerConfig {
    let mut config = SpeakerConfig::new("/nonexistent/senate-speaker-tests");
    config.locale = locale;
    config
}

pub(crate) use crate::tests_support::assert_senate_vocabulary;

pub(crate) struct Rig {
    pub(crate) hub: Arc<MemoryHub>,
    pub(crate) surface: Arc<FakeSurface>,
    pub(crate) brain: Arc<ScriptedBrain>,
    pub(crate) speaker: ActorRef<SpeakerMsg>,
    pub(crate) unbound: mpsc::Sender<UnboundMention>,
    pub(crate) join: ractor::concurrency::JoinHandle<()>,
}

impl Rig {
    pub(crate) async fn start(hub: Arc<MemoryHub>) -> Self {
        Self::start_with(hub, FakeSurface::new(), ScriptedBrain::new()).await
    }

    pub(crate) async fn start_with(
        hub: Arc<MemoryHub>,
        surface: Arc<FakeSurface>,
        brain: Arc<ScriptedBrain>,
    ) -> Self {
        Self::start_config(hub, surface, brain, config()).await
    }

    pub(crate) async fn start_config(
        hub: Arc<MemoryHub>,
        surface: Arc<FakeSurface>,
        brain: Arc<ScriptedBrain>,
        config: SpeakerConfig,
    ) -> Self {
        let (unbound, rx) = mpsc::channel(16);
        let args = SpeakerArgs::new(hub.clone(), surface.clone(), brain.clone(), config, rx);
        Self::spawn(hub, surface, brain, args, unbound).await
    }

    /// A Speaker on prepared `args` (e.g. the ones a predecessor ran on).
    pub(crate) async fn spawn(
        hub: Arc<MemoryHub>,
        surface: Arc<FakeSurface>,
        brain: Arc<ScriptedBrain>,
        args: SpeakerArgs,
        unbound: mpsc::Sender<UnboundMention>,
    ) -> Self {
        let (speaker, join) = Speaker::spawn(args).await.expect("speaker spawns");
        let rig = Self {
            hub,
            surface,
            brain,
            speaker,
            unbound,
            join,
        };
        rig.settle().await;
        rig
    }

    /// Let the pumps forward what is pending, then wait until the Speaker
    /// has caught every committee up and no Brain call is in flight.
    pub(crate) async fn settle(&self) {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        ractor::call!(self.speaker, SpeakerMsg::Settle).expect("speaker settles");
    }

    pub(crate) async fn append(&self, event: NewEvent) -> Event {
        self.hub.append(event).await.expect("append").event
    }

    pub(crate) async fn mention_unbound(&self, mention: UnboundMention) {
        self.unbound.send(mention).await.expect("speaker listens");
        self.settle().await;
    }

    pub(crate) async fn cursor(&self, committee_id: &CommitteeId) -> Seq {
        self.hub.cursor(committee_id, CURSOR).await.expect("cursor")
    }

    pub(crate) async fn stop(self) {
        self.speaker.stop(None);
        self.join.await.expect("speaker stops");
    }
}
