//! Simulated messages ([`senate_core::chat_sim`]): injected at the gateway,
//! answered through the surface the Speaker already has.
//!
//! A simulated message is an [`Inbound`] like any other, from the person's
//! account on this platform, into a bound group; the only difference is
//! that its `chat` event's source is marked simulated. Each one opens (or,
//! with `reply_to`, continues) a *conversation*, which remembers:
//!
//! - every platform ref in it: the injected messages and what the Speaker
//!   posted for them (a post replying to one of them is the conversation's);
//! - every log event descended from it: its `chat` events, what the Speaker
//!   relayed or decided from them (`meta.relayed_from`), what it proposed
//!   from them (`meta.sources`), and the replies in those threads. A new
//!   root the Speaker posts for one of these (a relay's task card in the
//!   Agent group, a decision proposed in the group) carries that event's
//!   operation id ([`senate_speaker::operation_id`]), which is how it is
//!   recognised.
//!
//! What the Speaker posts into a conversation is recorded as its transcript
//! and, unless the conversation is `live`, never reaches the platform: the
//! surface answers with a `sim:` ref of its own, so the Speaker records its
//! copies and goes on as usual.

use std::collections::{HashMap, VecDeque};

use chrono::Utc;
use senate_core::chat_sim::{
    SIM_REF_PREFIX, SimEntry, SimError, SimRequest, SimTranscript, SimulatedChat,
};
use senate_core::{Address, CommitteeId, Event, EventId, Kind, PostTarget, Posted, Seq, meta};
use serde_json::Value;

use super::{Addressing, ChatGateway, Delivered, GatewayError, Inbound, Inner, lock};

/// Conversations remembered; the oldest is forgotten past this.
const CONVERSATIONS: usize = 256;

#[derive(Default)]
pub(super) struct Sims {
    conversations: HashMap<String, Conversation>,
    order: VecDeque<String>,
    /// Every platform ref of a conversation: which one, and the root of the
    /// platform thread the ref is in.
    refs: HashMap<String, SimRef>,
    /// Log events descended from a simulated message.
    seqs: HashMap<(CommitteeId, Seq), String>,
}

struct SimRef {
    conversation: String,
    thread_root: String,
}

struct Conversation {
    committee: CommitteeId,
    group_id: String,
    live: bool,
    entries: Vec<SimEntry>,
    /// Posts already made, by operation id: a replay is answered as a
    /// platform would, with the first post.
    ops: HashMap<String, Posted>,
}

/// Where a post the Speaker makes belongs, when it is a conversation's.
pub(super) struct SimRoute {
    conversation: String,
    pub(super) live: bool,
    reply_to: Option<String>,
    thread_root: Option<String>,
    group_id: Option<String>,
}

impl Sims {
    fn open(&mut self, id: &str, conversation: Conversation) {
        if self
            .conversations
            .insert(id.to_owned(), conversation)
            .is_none()
        {
            self.order.push_back(id.to_owned());
        }
        while self.order.len() > CONVERSATIONS {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            self.conversations.remove(&old);
            self.refs.retain(|_, r| r.conversation != old);
            self.seqs.retain(|_, c| *c != old);
        }
    }

    fn add_ref(&mut self, message_ref: &str, conversation: &str, thread_root: &str) {
        self.refs.insert(
            message_ref.to_owned(),
            SimRef {
                conversation: conversation.to_owned(),
                thread_root: thread_root.to_owned(),
            },
        );
    }

    /// A log event: remember it when it descends from a simulated message.
    pub(super) fn observe(&mut self, event: &Event) {
        let key = (event.committee.clone(), event.seq);
        if self.seqs.contains_key(&key) {
            return;
        }
        let conversation = if event.kind == Kind::Chat && event.is_simulated() {
            // The injected message itself, by its ref.
            event
                .mirror
                .as_ref()
                .and_then(|m| self.refs.get(&m.message_ref))
                .map(|r| r.conversation.clone())
        } else {
            // What it was relayed from, the thread it is in, or the
            // messages it cites (a decision proposed from a discussion).
            let cited = event
                .meta
                .get(meta::SOURCES)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_u64)
                .map(Seq);
            let parent = event
                .meta
                .get(meta::RELAYED_FROM)
                .and_then(Value::as_u64)
                .map(Seq)
                .into_iter()
                .chain(event.thread)
                .chain(cited);
            parent
                .filter_map(|seq| self.seqs.get(&(event.committee.clone(), seq)))
                .next()
                .cloned()
        };
        if let Some(conversation) = conversation {
            if let Some(message_ref) = event.mirror.as_ref().map(|m| &m.message_ref)
                && let Some(entry) = self
                    .conversations
                    .get_mut(&conversation)
                    .and_then(|c| c.entries.iter_mut().find(|e| &e.message_ref == message_ref))
            {
                entry.seq = Some(event.seq);
            }
            self.seqs.insert(key, conversation);
        }
    }

    /// The conversation a reply to `message_ref` belongs to, if any.
    pub(super) fn route_reply(&self, message_ref: &str) -> Option<SimRoute> {
        let r = self.refs.get(message_ref)?;
        let live = self.conversations.get(&r.conversation)?.live;
        Some(SimRoute {
            conversation: r.conversation.clone(),
            live,
            reply_to: Some(message_ref.to_owned()),
            thread_root: Some(r.thread_root.clone()),
            group_id: None,
        })
    }

    /// The conversation a new root in `group_id` with `operation_id`
    /// belongs to: one posted for an event descended from a simulated
    /// message.
    pub(super) fn route_root(
        &self,
        epoch: &str,
        group_id: &str,
        operation_id: &str,
    ) -> Option<SimRoute> {
        let conversation = self.seqs.iter().find_map(|((committee, seq), c)| {
            (senate_speaker::operation_id(epoch, committee, *seq, group_id) == operation_id)
                .then_some(c)
        })?;
        let live = self.conversations.get(conversation)?.live;
        Some(SimRoute {
            conversation: conversation.clone(),
            live,
            reply_to: None,
            thread_root: None,
            group_id: Some(group_id.to_owned()),
        })
    }

    /// A post the platform will never see: answered with a `sim:` ref.
    pub(super) fn post_offline(
        &mut self,
        route: &SimRoute,
        text: &str,
        card: bool,
        operation_id: &str,
    ) -> Posted {
        if let Some(posted) = self
            .conversations
            .get(&route.conversation)
            .and_then(|c| c.ops.get(operation_id))
        {
            return Posted {
                duplicate: true,
                ..posted.clone()
            };
        }
        let posted = Posted {
            message_ref: new_sim_ref(),
            thread_ref: route.thread_root.clone(),
            duplicate: false,
        };
        self.record_post(route, text, card, operation_id, &posted);
        posted
    }

    /// A post into the conversation, made (or not) on the platform.
    pub(super) fn record_post(
        &mut self,
        route: &SimRoute,
        text: &str,
        card: bool,
        operation_id: &str,
        posted: &Posted,
    ) {
        let root = posted
            .thread_ref
            .clone()
            .or_else(|| route.thread_root.clone())
            .unwrap_or_else(|| posted.message_ref.clone());
        self.add_ref(&posted.message_ref, &route.conversation, &root);
        let Some(conversation) = self.conversations.get_mut(&route.conversation) else {
            return;
        };
        if conversation.ops.contains_key(operation_id) {
            return;
        }
        conversation
            .ops
            .insert(operation_id.to_owned(), posted.clone());
        let group_id = route
            .group_id
            .clone()
            .filter(|g| *g != conversation.group_id);
        conversation.entries.push(SimEntry {
            ts: Utc::now(),
            from: Address::Speaker,
            simulated_by: None,
            text: text.to_owned(),
            message_ref: posted.message_ref.clone(),
            reply_to: route.reply_to.clone(),
            group_id,
            card,
            updates: None,
            seq: None,
        });
    }

    /// An update of a card posted into a conversation.
    pub(super) fn record_update(&mut self, message_ref: &str, text: &str) {
        let Some(r) = self.refs.get(message_ref) else {
            return;
        };
        let Some(conversation) = self.conversations.get_mut(&r.conversation) else {
            return;
        };
        conversation.entries.push(SimEntry {
            ts: Utc::now(),
            from: Address::Speaker,
            simulated_by: None,
            text: text.to_owned(),
            message_ref: message_ref.to_owned(),
            reply_to: None,
            group_id: None,
            card: true,
            updates: Some(message_ref.to_owned()),
            seq: None,
        });
    }

    pub(super) fn is_offline(&self, message_ref: &str) -> Option<bool> {
        let r = self.refs.get(message_ref)?;
        self.conversations.get(&r.conversation).map(|c| !c.live)
    }

    fn transcript(&self, committee: &CommitteeId, message_ref: &str) -> Option<SimTranscript> {
        let id = &self.refs.get(message_ref)?.conversation;
        let conversation = self.conversations.get(id)?;
        (&conversation.committee == committee).then(|| SimTranscript {
            conversation: id.clone(),
            committee: conversation.committee.clone(),
            group_id: conversation.group_id.clone(),
            live: conversation.live,
            entries: conversation.entries.clone(),
        })
    }
}

fn new_sim_ref() -> String {
    format!(
        "{SIM_REF_PREFIX}{}",
        EventId::new().to_string().to_lowercase()
    )
}

/// A card as the transcript shows it.
pub(super) fn card_text(card: &senate_core::Card) -> String {
    if card.markdown.is_empty() {
        format!("**{}**", card.title)
    } else {
        format!("**{}**\n{}", card.title, card.markdown)
    }
}

/// How a live simulated message shows in the group: marked, by whom, and
/// whom it is from.
fn live_text(by: &str, person: &str, text: &str) -> String {
    format!("_(simulated by {by})_ **{person}**: {text}")
}

fn invalid(message: impl Into<String>) -> SimError {
    SimError::Invalid(message.into())
}

impl From<GatewayError> for SimError {
    fn from(error: GatewayError) -> Self {
        Self::Unavailable(error.to_string())
    }
}

impl Inner {
    /// Where a post the Speaker makes goes, when a simulated conversation
    /// owns it.
    pub(super) async fn sim_route(
        &self,
        target: &PostTarget,
        operation_id: &str,
    ) -> Option<SimRoute> {
        match target {
            PostTarget::Reply { message_ref, .. } => {
                // Only a conversation's refs, which are never the
                // platform's own unless live.
                lock(&self.state).sims.route_reply(message_ref)
            }
            PostTarget::NewRoot { group_id } => {
                if lock(&self.state).sims.seqs.is_empty() {
                    return None;
                }
                let epoch = self.epoch().await?;
                lock(&self.state)
                    .sims
                    .route_root(&epoch, group_id, operation_id)
            }
        }
    }

    /// The log's epoch, as the Speaker's operation ids name it.
    async fn epoch(&self) -> Option<String> {
        if let Some(epoch) = lock(&self.epoch).clone() {
            return Some(epoch);
        }
        let hub = self.hub.borrow().clone()?;
        let epoch = hub.epoch().await.ok()?;
        *lock(&self.epoch) = Some(epoch.clone());
        Some(epoch)
    }
}

impl ChatGateway {
    /// Inject a simulated message ([`senate_core::chat_sim`]); the caller
    /// has checked who may.
    pub async fn simulate(&self, request: SimRequest) -> Result<SimulatedChat, SimError> {
        let hub = self.inner.hub().await?;
        self.inner.drain_fresh();
        let roster = hub.roster().await.map_err(GatewayError::from)?;
        let platform = self.inner.platform.clone();
        let sender = roster
            .principal(&request.from)
            .and_then(|p| p.account_ids(&platform).next().map(str::to_owned))
            .ok_or_else(|| {
                invalid(format!(
                    "{} has no {platform} account on the roster: a message from them would arrive as a guest",
                    request.from
                ))
            })?;
        if request.text.trim().is_empty() {
            return Err(invalid("the message is empty"));
        }
        // The conversation it continues, or the group it opens one in.
        let continued = match &request.reply_to {
            Some(reply_to) => {
                let state = lock(&self.inner.state);
                let r = state.sims.refs.get(reply_to).ok_or_else(|| {
                    SimError::NotFound(format!("{reply_to} is no simulated conversation's"))
                })?;
                let c = &state.sims.conversations[&r.conversation];
                if c.committee != request.committee {
                    return Err(SimError::NotFound(format!(
                        "{reply_to} is no simulated conversation of {}",
                        request.committee
                    )));
                }
                Some((
                    r.conversation.clone(),
                    r.thread_root.clone(),
                    c.group_id.clone(),
                    c.live,
                ))
            }
            None => None,
        };
        let (group_id, live) = match &continued {
            Some((_, _, group, live)) => (group.clone(), *live),
            None => (self.group_for(&request)?, request.live),
        };
        let thread_root = continued.as_ref().map(|(_, root, _, _)| root.clone());
        let (message_ref, thread_ref) = if live {
            let target = match &request.reply_to {
                Some(reply_to) => PostTarget::Reply {
                    message_ref: reply_to.clone(),
                    in_thread: true,
                },
                None => PostTarget::NewRoot {
                    group_id: group_id.clone(),
                },
            };
            let name = roster
                .principal(&request.from)
                .and_then(|p| p.names.first().cloned())
                .unwrap_or_else(|| request.from.to_string());
            let operation = format!("senate:sim:{}", EventId::new());
            let posted = self
                .inner
                .surface
                .post(
                    target,
                    &live_text(request.by.as_str(), &name, &request.text),
                    &operation,
                )
                .await
                .map_err(|e| SimError::Unavailable(format!("the platform refused it: {e}")))?;
            (
                posted.message_ref,
                posted.thread_ref.or(thread_root.clone()),
            )
        } else {
            (new_sim_ref(), thread_root.clone())
        };
        let conversation = continued
            .as_ref()
            .map_or_else(|| message_ref.clone(), |(c, _, _, _)| c.clone());
        {
            let mut state = lock(&self.inner.state);
            if continued.is_none() {
                state.sims.open(
                    &conversation,
                    Conversation {
                        committee: request.committee.clone(),
                        group_id: group_id.clone(),
                        live,
                        entries: Vec::new(),
                        ops: HashMap::new(),
                    },
                );
            }
            let root = thread_ref.clone().unwrap_or_else(|| message_ref.clone());
            state.sims.add_ref(&message_ref, &conversation, &root);
            if let Some(c) = state.sims.conversations.get_mut(&conversation) {
                c.entries.push(SimEntry {
                    ts: Utc::now(),
                    from: Address::senior(&request.from),
                    simulated_by: Some(request.by.clone()),
                    text: request.text.clone(),
                    message_ref: message_ref.clone(),
                    reply_to: request.reply_to.clone(),
                    group_id: None,
                    card: false,
                    updates: None,
                    seq: None,
                });
            }
        }
        let inbound = Inbound {
            group_id: group_id.clone(),
            group_name: None,
            sender,
            message_ref: message_ref.clone(),
            thread_ref: thread_ref.clone(),
            thread_root: thread_ref.clone(),
            reply_to: request.reply_to.clone(),
            addressing: if request.ambient {
                Addressing::Ambient
            } else {
                Addressing::Mention
            },
            mentions: Vec::new(),
            text: request.text,
        };
        let seq = match self.deliver_marked(inbound, Some(&request.by)).await? {
            Delivered::Recorded(event) => {
                lock(&self.inner.state).sims.observe(&event);
                Some(event.seq)
            }
            Delivered::Unbound | Delivered::Dropped => None,
        };
        Ok(SimulatedChat {
            conversation,
            message_ref,
            group_id,
            seq,
            live,
        })
    }

    /// The bound group a new conversation opens in: the one named, else
    /// the committee's only people group.
    fn group_for(&self, request: &SimRequest) -> Result<String, SimError> {
        let state = lock(&self.inner.state);
        let mut bound: Vec<&String> = state
            .table
            .groups
            .iter()
            .filter(|(g, c)| {
                **c == request.committee
                    && (request.group.is_some() || !state.table.agents.contains(*g))
            })
            .map(|(g, _)| g)
            .collect();
        bound.sort();
        match (&request.group, bound.as_slice()) {
            (Some(group), _) if bound.contains(&group) => Ok(group.clone()),
            (Some(group), _) => Err(invalid(format!(
                "{group} is not bound to {} on {}",
                request.committee, self.inner.platform
            ))),
            (None, [one]) => Ok((*one).clone()),
            (None, []) => Err(invalid(format!(
                "no group is bound to {} on {}",
                request.committee, self.inner.platform
            ))),
            (None, several) => Err(invalid(format!(
                "several groups are bound to {}; name one with --group: {}",
                request.committee,
                several
                    .iter()
                    .map(|g| g.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    /// A simulated conversation, by any of its refs.
    pub async fn sim_transcript(
        &self,
        committee: &CommitteeId,
        message_ref: &str,
    ) -> Result<SimTranscript, SimError> {
        self.inner.drain_fresh();
        lock(&self.inner.state)
            .sims
            .transcript(committee, message_ref)
            .ok_or_else(|| {
                SimError::NotFound(format!(
                    "no simulated conversation of {committee} has {message_ref} (they last until senated restarts)"
                ))
            })
    }
}

#[async_trait::async_trait]
impl senate_core::chat_sim::ChatSimulator for ChatGateway {
    async fn simulate(&self, request: SimRequest) -> Result<SimulatedChat, SimError> {
        ChatGateway::simulate(self, request).await
    }

    async fn transcript(
        &self,
        committee: &CommitteeId,
        message_ref: &str,
    ) -> Result<SimTranscript, SimError> {
        self.sim_transcript(committee, message_ref).await
    }
}
