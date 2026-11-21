//! In-memory conversations and ordered, confirmed agent plans.
use super::*;
use crate::brain::{AgentTurn, Conversation, Turn};

pub(super) type Replies = HashMap<(CommitteeId, Seq), Vec<(String, String)>>;

const SCOPE: &str = "speaker_agent_action";

#[derive(Clone)]
pub(super) struct Session {
    group: String,
    root: String,
    thread: Option<String>,
    replies: Vec<String>,
    log_root: Option<Seq>,
    speaker: Address,
    turns: Vec<Turn>,
    awaiting: bool,
    pub(super) expires: Instant,
}

#[derive(Clone, Default)]
pub(super) struct Progress {
    next: usize,
    actions: Vec<PendingAction>,
}

pub(super) fn decision(turn: Result<AgentTurn, BrainError>, l: Locale) -> Decision {
    match turn {
        Ok(turn) => Decision::Agent(turn),
        Err(error) => super::failed(error, l),
    }
}

fn scoped(chat: &Event, scope: impl ToString) -> Event {
    let mut chat = chat.clone();
    chat.meta.insert(SCOPE.into(), json!(scope.to_string()));
    chat
}

pub(super) fn event_key(chat: &Event) -> String {
    match chat.meta.get(SCOPE).and_then(Value::as_str) {
        Some(scope) => format!("{}:{scope}", chat.seq),
        None => chat.seq.to_string(),
    }
}

pub(super) fn reply_target(chat: &Event, group: &str) -> String {
    match chat.meta.get(SCOPE).and_then(Value::as_str) {
        Some(scope) => format!("{group}#agent-{scope}"),
        None => group.to_owned(),
    }
}

pub(super) fn remember_reply(engine: &mut Engine, chat: &Event, text: &str) {
    if let Some(scope) = chat.meta.get(SCOPE).and_then(Value::as_str) {
        let replies = engine
            .agent_replies
            .entry((chat.committee.clone(), chat.seq))
            .or_default();
        if !replies.iter().any(|(s, _)| s == scope) {
            replies.push((scope.to_owned(), text.to_owned()));
        }
    }
}

fn restatement(l: Locale, actions: &[PendingAction]) -> String {
    render::confirm_plan(
        l,
        &actions
            .iter()
            .map(|action| action.plan_line(l))
            .collect::<Vec<_>>(),
    )
}

/// Where the next step of a plan acts: `Some(committee)` (the chat's own, or
/// the one an earlier Bind binds to) or `None` after an Unbind; and whether
/// a Bind came before it (then read-only steps wait for a yes too, and run
/// on the new committee in order).
fn plan_target(origin: &CommitteeId, actions: &[PendingAction]) -> (Option<CommitteeId>, bool) {
    let mut target = Some(origin.clone());
    let mut rebound = false;
    for action in actions {
        match action {
            PendingAction::Bind { to, .. } => {
                target = Some(to.clone());
                rebound = true;
            }
            PendingAction::Unbind { .. } => target = None,
            _ => {}
        }
    }
    (target, rebound)
}

/// Successful steps survive a transient failure; only final outcomes are cached.
#[derive(Debug, Clone, Default)]
pub(super) struct Execution {
    completed: Vec<String>,
    result: Option<String>,
}

impl Engine {
    fn session_for_chat(&self, chat: &Event) -> Option<usize> {
        let source = chat.source.as_ref()?;
        let mirror = chat.mirror.as_ref()?;
        self.conversations.iter().position(|s| {
            s.group == source.group_id
                && s.expires > Instant::now()
                && (mirror
                    .thread_ref
                    .as_ref()
                    .is_some_and(|t| Some(t) == s.thread.as_ref())
                    || chat.meta.get(meta::THREAD_ROOT).and_then(Value::as_str)
                        == Some(s.root.as_str())
                    || chat.thread.is_some_and(|root| Some(root) == s.log_root)
                    || chat
                        .meta
                        .get(meta::REPLY_TO)
                        .and_then(Value::as_str)
                        .is_some_and(|r| s.replies.iter().any(|p| p == r))
                    || mirror.message_ref == s.root)
        })
    }

    pub(super) fn continuing_chat(&mut self, chat: &Event) -> bool {
        self.purge_expired();
        self.session_for_chat(chat).is_some_and(|i| {
            let s = &self.conversations[i];
            s.awaiting && s.speaker == chat.from
        })
    }

    pub(super) fn start_conversation(&mut self, chat: &Event, utterance: Utterance) {
        let index = match self.session_for_chat(chat) {
            Some(i) => i,
            None => {
                self.conversations.push(Session {
                    group: utterance.group.group_id.clone(),
                    root: chat
                        .mirror
                        .as_ref()
                        .map(|m| m.message_ref.clone())
                        .unwrap_or_default(),
                    thread: chat.mirror.as_ref().and_then(|m| m.thread_ref.clone()),
                    replies: Vec::new(),
                    log_root: Some(chat.thread.unwrap_or(chat.seq)),
                    speaker: chat.from.clone(),
                    turns: Vec::new(),
                    awaiting: false,
                    expires: Instant::now() + self.confirmation_ttl,
                });
                self.conversations.len() - 1
            }
        };
        let s = &mut self.conversations[index];
        let conversation = Conversation {
            utterance,
            turns: s.turns.clone(),
        };
        s.turns.push(Turn {
            speaker: chat.from.clone(),
            text: chat.body.clone(),
        });
        s.speaker = chat.from.clone();
        s.awaiting = false;
        s.expires = Instant::now() + self.confirmation_ttl;
        let brain = Arc::clone(&self.brain);
        let (committee, seq) = (chat.committee.clone(), chat.seq);
        self.think(
            async move { brain.converse(conversation).await },
            move |turn| ThoughtKind::Conversed {
                committee,
                seq,
                turn,
            },
        );
    }

    pub(super) async fn run_agent(&mut self, chat: &Event, turn: AgentTurn) -> Result<Flow, Stall> {
        let l = self.locale(&chat.committee);
        if turn.actions.len() > self.agent_max_actions {
            let text = render::too_many_actions(l, self.agent_max_actions);
            return self.say(chat, &text).await;
        }
        let Some(source) = chat.source.as_ref() else {
            return Ok(Flow::Done);
        };
        if !turn.reply.trim().is_empty() {
            self.say(&scoped(chat, "reply"), turn.reply.trim()).await?;
        }
        let key = (chat.committee.clone(), chat.seq);
        let mut progress = self.agent_progress.get(&key).cloned().unwrap_or_default();
        while progress.next < turn.actions.len() {
            let action = turn.actions[progress.next].clone();
            let item = scoped(chat, progress.next);
            let (target, rebound) = plan_target(&chat.committee, &progress.actions);
            let result = match target {
                Some(target) => {
                    self.preparing = Some(progress.actions.clone());
                    self.acting = (target != chat.committee).then_some(target);
                    self.deferring = rebound;
                    let result = self
                        .run_bound(
                            &item,
                            Decision::Run {
                                intent: action,
                                confirm: true,
                            },
                        )
                        .await;
                    self.acting = None;
                    self.deferring = false;
                    progress.actions = self.preparing.take().unwrap_or_default();
                    result
                }
                None => {
                    self.after_unbind(&item, action, &mut progress.actions)
                        .await
                }
            };
            match result? {
                Flow::Await => return Ok(Flow::Await),
                Flow::Done => {
                    progress.next += 1;
                    self.agent_progress.insert(key.clone(), progress.clone());
                    if let Some(state) = self.committees.get_mut(&chat.committee) {
                        state.triage.remove(&chat.seq);
                    }
                }
            }
        }
        if !progress.actions.is_empty() {
            let posted = self
                .reply_to_chat(&scoped(chat, "plan"), &restatement(l, &progress.actions))
                .await?;
            if let Some(posted) = posted {
                self.pending.retain(|p| p.confirm_ref != posted.message_ref);
                self.pending.push(Pending {
                    confirm_ref: posted.message_ref,
                    thread_ref: posted.thread_ref,
                    origin_ref: chat
                        .mirror
                        .as_ref()
                        .map(|m| m.message_ref.clone())
                        .unwrap_or_default(),
                    origin: Origin::Bound {
                        committee: chat.committee.clone(),
                        seq: chat.seq,
                    },
                    group: group_ref(source),
                    actions: progress.actions,
                    agent_plan: true,
                    execution: None,
                    requester: chat.from.clone(),
                    expires: Instant::now() + self.confirmation_ttl,
                });
            }
        }
        // Only the final marker suppresses the whole turn after a restart;
        // partial replies must not skip the rest of a plan after a stall.
        let platform = self.surface.platform();
        let copies = self
            .hub
            .mirrors_of_seq(&chat.committee, chat.seq, platform)
            .await?;
        let copy = copies.iter().find(|m| m.target.contains("#agent-"));
        let replies = self.agent_replies.remove(&key).unwrap_or_default();
        if let Some(i) = self.session_for_chat(chat) {
            let s = &mut self.conversations[i];
            if let Some(copy) = copy {
                s.thread = copy.thread_ref.clone();
            }
            s.replies.extend(
                copies
                    .iter()
                    .filter(|m| m.target.contains("#agent-"))
                    .map(|m| m.message_ref.clone()),
            );
            s.awaiting = turn.awaiting_reply;
            s.expires = Instant::now() + self.confirmation_ttl;
            s.turns.extend(replies.into_iter().map(|(_, text)| Turn {
                speaker: Address::Speaker,
                text,
            }));
        }
        if let Some(copy) = copy {
            let mut marker = copy.clone();
            marker.target = source.group_id.clone();
            self.hub.put_mirror(marker).await?;
        }
        Ok(Flow::Done)
    }

    /// A step after an Unbind of the same plan: the group is then bound to
    /// nothing, so, as in an unbound group, only a Bind or the adoption of a
    /// named committee's Agent group can follow; the rest is left out.
    async fn after_unbind(
        &mut self,
        item: &Event,
        intent: Intent,
        actions: &mut Vec<PendingAction>,
    ) -> Result<Flow, Stall> {
        let l = self.locale(&item.committee);
        if let Some(refusal) = self.refusal_for(&item.from, &intent, l) {
            return self.say(item, &refusal).await;
        }
        let planned = match intent {
            Intent::Bind { committee: raw } => CommitteeId::new(raw.trim())
                .map(|to| PendingAction::Bind { from: None, to })
                .map_err(|_| render::invalid_committee(l, &raw)),
            Intent::AdoptAgentGroup {
                committee: Some(raw),
            } if !raw.trim().is_empty() => CommitteeId::new(raw.trim())
                .map(|committee| PendingAction::AdoptAgents {
                    committee,
                    from_people: false,
                })
                .map_err(|_| render::invalid_committee(l, &raw)),
            Intent::None { reply } => Err(reply),
            _ => Err(render::step_after_unbind(l)),
        };
        match planned {
            Ok(action) => {
                actions.push(action);
                Ok(Flow::Done)
            }
            Err(text) if text.trim().is_empty() => Ok(Flow::Done),
            Err(text) => self.say(item, &text).await,
        }
    }

    pub(super) fn remember_confirmation(&mut self, chat: &Event, text: &str) {
        if let Some(i) = self.session_for_chat(chat) {
            let s = &mut self.conversations[i];
            s.turns.push(Turn {
                speaker: chat.from.clone(),
                text: chat.body.clone(),
            });
            if !text.is_empty() {
                s.turns.push(Turn {
                    speaker: Address::Speaker,
                    text: text.to_owned(),
                });
            }
            s.expires = Instant::now() + self.confirmation_ttl;
            s.awaiting = false;
        }
    }

    pub(super) async fn append_action(
        &self,
        event: NewEvent,
        strict: bool,
    ) -> Result<Option<Event>, Stall> {
        if strict {
            return Ok(Some(self.hub.append(self.stamped(event)).await?.event));
        }
        self.append(event).await
    }

    pub(super) async fn execute_pending(
        &mut self,
        index: usize,
        pending: &Pending,
        trigger: &str,
    ) -> Result<String, Stall> {
        let l = match &pending.origin {
            Origin::Bound { committee, .. } => self.locale(committee),
            Origin::Unbound => self.config.locale,
        };
        if !pending.agent_plan {
            return self
                .execute_now(
                    &pending.actions[0],
                    &pending.group,
                    &pending.requester,
                    trigger,
                )
                .await;
        }
        let Some(current) = self.pending.get_mut(index) else {
            return Ok(String::new());
        };
        let execution = current.execution.get_or_insert_with(Execution::default);
        if let Some(result) = &execution.result {
            return Ok(result.clone());
        }
        let mut lines = execution.completed.clone();
        for (i, action) in pending.actions.iter().enumerate().skip(lines.len()) {
            match self
                .execute(
                    action,
                    &pending.group,
                    &pending.requester,
                    &format!("{trigger}:{i}"),
                    true,
                )
                .await
            {
                Ok(text) => {
                    lines.push(render::action_completed(l, i, &action.plan_line(l), &text));
                    if let Some(execution) = self
                        .pending
                        .get_mut(index)
                        .and_then(|p| p.execution.as_mut())
                    {
                        execution.completed = lines.clone();
                    }
                }
                Err(ActionError::Retry(stall)) => return Err(stall),
                Err(ActionError::Refused) => {
                    lines.push(render::action_refused(
                        l,
                        i,
                        &action.plan_line(l),
                        pending.actions.len() - i - 1,
                    ));
                    break;
                }
            }
        }
        let result = lines.join("\n");
        if let Some(execution) = self
            .pending
            .get_mut(index)
            .and_then(|p| p.execution.as_mut())
        {
            execution.result = Some(result.clone());
        }
        Ok(result)
    }

    fn session_for_unbound(&self, mention: &UnboundMention) -> Option<usize> {
        self.conversations.iter().position(|s| {
            s.group == mention.group_id
                && s.expires > Instant::now()
                && (mention
                    .thread_ref
                    .as_ref()
                    .is_some_and(|t| Some(t) == s.thread.as_ref())
                    || mention.thread_root_message_ref.as_deref() == Some(s.root.as_str())
                    || mention
                        .reply_to_message_ref
                        .as_ref()
                        .is_some_and(|r| s.replies.contains(r))
                    || mention.message_ref == s.root)
        })
    }

    pub(super) fn start_unbound_conversation(
        &mut self,
        mention: UnboundMention,
        utterance: Utterance,
    ) {
        let i = self.session_for_unbound(&mention).unwrap_or_else(|| {
            self.conversations.push(Session {
                group: mention.group_id.clone(),
                root: mention.message_ref.clone(),
                thread: mention.thread_ref.clone(),
                replies: Vec::new(),
                log_root: None,
                speaker: mention.sender.clone(),
                turns: Vec::new(),
                awaiting: false,
                expires: Instant::now() + self.confirmation_ttl,
            });
            self.conversations.len() - 1
        });
        let s = &mut self.conversations[i];
        let conversation = Conversation {
            utterance,
            turns: s.turns.clone(),
        };
        s.turns.push(Turn {
            speaker: mention.sender.clone(),
            text: mention.text.clone(),
        });
        s.speaker = mention.sender.clone();
        s.awaiting = false;
        s.expires = Instant::now() + self.confirmation_ttl;
        let brain = Arc::clone(&self.brain);
        let mention = Box::new(mention);
        self.think(
            async move { brain.converse(conversation).await },
            move |turn| ThoughtKind::UnboundConversed { mention, turn },
        );
    }

    pub(super) fn ignores_unbound_reply(&self, mention: &UnboundMention) -> bool {
        mention.addressing != "mention"
            && self.session_for_unbound(mention).is_some_and(|i| {
                let s = &self.conversations[i];
                !s.awaiting || s.speaker != mention.sender
            })
    }

    pub(super) fn continuing_unbound(&self, mention: &UnboundMention) -> bool {
        self.session_for_unbound(mention).is_some_and(|i| {
            self.conversations[i].awaiting && self.conversations[i].speaker == mention.sender
        })
    }

    async fn unbound_part(
        &mut self,
        mention: &UnboundMention,
        scope: &str,
        text: &str,
    ) -> Result<Option<Posted>, Stall> {
        if text.trim().is_empty() {
            return Ok(None);
        }
        let operation = format!(
            "{}:{scope}",
            unbound_operation_id(&mention.group_id, &mention.message_ref)
        );
        let posted = self
            .post(
                PostTarget::Reply {
                    message_ref: mention.message_ref.clone(),
                    in_thread: true,
                },
                text,
                &operation,
            )
            .await?;
        if let Some(posted) = &posted
            && let Some(i) = self.session_for_unbound(mention)
        {
            let s = &mut self.conversations[i];
            s.thread = posted.thread_ref.clone();
            s.replies.push(posted.message_ref.clone());
            s.turns.push(Turn {
                speaker: Address::Speaker,
                text: text.to_owned(),
            });
            s.expires = Instant::now() + self.confirmation_ttl;
        }
        Ok(posted)
    }

    pub(super) async fn run_unbound_agent(
        &mut self,
        mention: &UnboundMention,
        decision: Decision,
    ) -> Result<(), Stall> {
        let Decision::Agent(turn) = decision else {
            return self.run_unbound(mention, decision).await;
        };
        let l = self.config.locale;
        if turn.actions.len() > self.agent_max_actions {
            let text = render::too_many_actions(l, self.agent_max_actions);
            self.unbound_part(mention, "limit", &text).await?;
            return Ok(());
        }
        self.unbound_part(mention, "reply", &turn.reply).await?;
        let mut actions = Vec::new();
        for (i, intent) in turn.actions.into_iter().enumerate() {
            let action = if let Some(refusal) = self.refusal_for(&mention.sender, &intent, l) {
                Err(refusal)
            } else {
                match intent {
                    Intent::Bind { committee: raw } => CommitteeId::new(raw.trim())
                        .map(|to| PendingAction::Bind { from: None, to })
                        .map_err(|_| render::invalid_committee(l, &raw)),
                    Intent::AdoptAgentGroup { committee: raw } => {
                        match raw.as_deref().map(str::trim) {
                            None | Some("") => Err(render::adopt_needs_committee(l)),
                            Some(raw) => CommitteeId::new(raw)
                                .map(|committee| PendingAction::AdoptAgents {
                                    committee,
                                    from_people: false,
                                })
                                .map_err(|_| render::invalid_committee(l, raw)),
                        }
                    }
                    Intent::Unbind => Err(render::nothing_to_unbind(l)),
                    Intent::Status => Err(render::unbound_status(l)),
                    Intent::Relay { .. } => Err(render::relay_needs_binding(l)),
                    Intent::Decide { .. } => Err(render::decide_needs_binding(l)),
                    Intent::Recall => Err(render::recall_needs_binding(l)),
                    Intent::InviteToAgentGroup { .. } | Intent::SetOutputLevel { .. } => {
                        Err(render::agents_need_binding(l))
                    }
                    Intent::Work { .. } | Intent::Reassign { .. } => {
                        Err(render::work_needs_binding(l))
                    }
                    Intent::None { reply } => Err(reply),
                }
            };
            match action {
                Ok(action) => actions.push(action),
                Err(text) => {
                    self.unbound_part(mention, &i.to_string(), &text).await?;
                }
            }
        }
        if !actions.is_empty()
            && let Some(posted) = self
                .unbound_part(mention, "plan", &restatement(l, &actions))
                .await?
        {
            self.pending.push(Pending {
                confirm_ref: posted.message_ref,
                thread_ref: posted.thread_ref,
                origin_ref: mention.message_ref.clone(),
                origin: Origin::Unbound,
                group: mention_group(mention),
                actions,
                agent_plan: true,
                execution: None,
                requester: mention.sender.clone(),
                expires: Instant::now() + self.confirmation_ttl,
            });
        }
        if let Some(i) = self.session_for_unbound(mention) {
            self.conversations[i].awaiting = turn.awaiting_reply;
        }
        Ok(())
    }
}
