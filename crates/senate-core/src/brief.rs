//! The committee brief: a short page of the committee's stable facts, given
//! to a session when it starts and readable at any time.
//!
//! Everything here is derived from the log by [`BriefBuilder`], so the brief
//! is rebuilt from any prefix of the log and never stored. The one part a
//! model writes, the "top three things" header, is itself a log event: the
//! latest `note` from the Speaker carrying [`meta::BRIEF_TOP`].
//!
//! A builder folds events in `seq` order; [`BriefBuilder::observe`] ignores
//! an event at or below the last one it saw, so a holder can feed it a page
//! that overlaps what it already read.
//!
//! A [`Brief`] is data, the same in every language. [`Brief::to_markdown`]
//! writes it out for a reader in one [`Locale`]: headings, connecting words
//! and ages are in that language; what people and agents wrote is quoted as
//! they wrote it; addresses are bracketed mentions and `seq` is `seq`.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::address::Address;
use crate::continuity::{
    AgentPresence, CheckpointPayload, CheckpointTrigger, PresenceBoard, PresenceState, lag_words,
    short_commit,
};
use crate::duty::DutyFold;
use crate::event::{BindingAction, Event, Kind, Refs, StatusState, meta};
use crate::ids::{CommitteeId, Seq};
use crate::locale::Locale;
use crate::locator::{Locator, intended};
use crate::memory::{Digest, MAX_BRIEF_POINTS, MemoryFold, MemoryPoint};
use crate::senator::SenatorName;
use crate::task::StatusPayload;

/// Most recent decisions kept (in seq order).
pub const MAX_DECISIONS: usize = 30;
/// Most recent open questions kept (in seq order).
pub const MAX_OPEN_QUESTIONS: usize = 10;
/// The Speaker's note that a question's seat is gone ([`meta::SEAT_REMOVED`]):
/// it closes the question.
pub fn is_seat_removed(event: &Event) -> bool {
    event.kind == Kind::Note
        && event.from == Address::Speaker
        && event.meta.contains_key(meta::SEAT_REMOVED)
}

/// The seat an in-thread message hands its thread to ([`meta::HANDED_TO`]).
pub fn handed_to(event: &Event) -> Option<SenatorName> {
    event
        .meta
        .get(meta::HANDED_TO)
        .and_then(serde_json::Value::as_str)
        .and_then(|s| s.strip_prefix("senator::").unwrap_or(s).parse().ok())
}

/// Characters of an open question's body kept in the brief.
pub const QUESTION_CHARS: usize = 200;
/// Agents whose DECISIONS notes the brief shows while no decision is
/// recorded: those with the latest checkpoints.
pub const MAX_DECISION_NOTES: usize = 3;
/// Characters of each agent's DECISIONS notes kept in the brief.
pub const DECISION_NOTE_CHARS: usize = 800;
/// Default bound of [`Brief::to_markdown`].
pub const DEFAULT_MARKDOWN_CHARS: usize = 8_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Brief {
    pub committee: CommitteeId,
    /// The last seq folded in. `Seq::ZERO` for an empty log.
    pub head: Seq,
    /// The Speaker's "top three things right now", if it has written one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top: Option<BriefTop>,
    /// Decisions in force (not superseded), oldest first, the latest
    /// [`MAX_DECISIONS`].
    #[serde(default)]
    pub decisions: Vec<BriefDecision>,
    /// While no decision is recorded: what agents wrote down as decided in
    /// their latest checkpoints (their `DECISIONS.md`), newest first, at
    /// most [`MAX_DECISION_NOTES`]. An agent's own record, confirmed by no
    /// one; empty once the log holds a decision.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decision_notes: Vec<BriefDecisionNotes>,
    /// Question roots with no answer yet, oldest first, the latest
    /// [`MAX_OPEN_QUESTIONS`]; bodies cut to [`QUESTION_CHARS`].
    #[serde(default)]
    pub open_questions: Vec<BriefQuestion>,
    /// Groups bound to the committee, in the order they were bound.
    #[serde(default)]
    pub groups: Vec<BriefGroup>,
    /// The latest checkpoint of each agent, oldest first. One person's
    /// agents each have their own.
    #[serde(default)]
    pub checkpoints: Vec<BriefCheckpoint>,
    /// Each agent's presence, by path. Folded from the server's `presence`
    /// events; the server's own brief replaces `lag` with how far behind
    /// each bridge is now.
    #[serde(default)]
    pub presence: Vec<AgentPresence>,
    /// Senators and guests who have written to the log, most recent first.
    #[serde(default)]
    pub participants: Vec<BriefParticipant>,
    /// What the committee knows: the latest [`MAX_BRIEF_POINTS`] memory
    /// points in force, oldest first ([`crate::memory`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memory: Vec<MemoryPoint>,
    /// The Speaker's latest digest of the discussion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<Digest>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BriefTop {
    pub seq: Seq,
    pub ts: DateTime<Utc>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BriefDecision {
    pub seq: Seq,
    pub ts: DateTime<Utc>,
    /// Who decided: `on_behalf_of` when the Speaker recorded it for someone,
    /// else `from`.
    pub by: Address,
    pub body: String,
}

/// An agent's `DECISIONS.md` as its latest checkpoint carried it, cut to
/// [`DECISION_NOTE_CHARS`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BriefDecisionNotes {
    /// The checkpoint it is from.
    pub seq: Seq,
    pub ts: DateTime<Utc>,
    pub agent: SenatorName,
    pub notes: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BriefQuestion {
    pub seq: Seq,
    pub ts: DateTime<Utc>,
    /// The asker: `on_behalf_of` when relayed, else `from`.
    pub from: Address,
    #[serde(default)]
    pub to: Vec<Address>,
    /// The seat it was meant for when that is outside this committee's own
    /// agents ([`meta::INTENDED`]): a seat a child Senate declares.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intended: Option<Locator>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BriefGroup {
    pub platform: String,
    pub group_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BriefCheckpoint {
    pub seq: Seq,
    pub ts: DateTime<Utc>,
    /// The agent whose checkpoint it is.
    pub agent: SenatorName,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refs: Option<Refs>,
    /// From the [`CheckpointPayload`], when the event carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<CheckpointTrigger>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BriefParticipant {
    pub address: Address,
    pub last_seq: Seq,
    pub last_ts: DateTime<Utc>,
}

/// Folds a committee's log, in seq order, into a [`Brief`].
#[derive(Debug, Clone)]
pub struct BriefBuilder {
    committee: CommitteeId,
    head: Seq,
    /// Every decision seen, by seq; superseded ones are removed.
    decisions: BTreeMap<Seq, BriefDecision>,
    /// Supersessions that named a decision not yet seen (or never stored).
    superseded: Vec<Seq>,
    /// Question roots not yet answered, by seq.
    open: BTreeMap<Seq, BriefQuestion>,
    /// Question roots whose thread was archived unanswered, by seq.
    archived: BTreeMap<Seq, BriefQuestion>,
    groups: Vec<BriefGroup>,
    checkpoints: HashMap<SenatorName, BriefCheckpoint>,
    /// The DECISIONS notes of each agent's latest checkpoint, when it has
    /// any.
    decision_notes: HashMap<SenatorName, BriefDecisionNotes>,
    presence: PresenceBoard,
    duties: DutyFold,
    participants: HashMap<Address, (Seq, DateTime<Utc>)>,
    memory: MemoryFold,
}

impl BriefBuilder {
    pub fn new(committee: CommitteeId) -> Self {
        Self {
            committee,
            head: Seq::ZERO,
            decisions: BTreeMap::new(),
            superseded: Vec::new(),
            open: BTreeMap::new(),
            archived: BTreeMap::new(),
            groups: Vec::new(),
            checkpoints: HashMap::new(),
            decision_notes: HashMap::new(),
            presence: PresenceBoard::new(),
            duties: DutyFold::new(),
            participants: HashMap::new(),
            memory: MemoryFold::new(),
        }
    }

    pub fn committee(&self) -> &CommitteeId {
        &self.committee
    }

    /// The last seq folded in.
    pub fn head(&self) -> Seq {
        self.head
    }

    /// Fold one event. Events of another committee, and events at or below
    /// [`Self::head`], are ignored.
    pub fn observe(&mut self, event: &Event) {
        if event.committee != self.committee || event.seq <= self.head {
            return;
        }
        self.head = event.seq;
        self.presence.observe(event);
        self.duties.observe(event);
        // Simulated traffic (`senate chat send`) is a test of the Speaker,
        // never work: no decision, question or participant of it is kept.
        // A group it bound is bound all the same.
        if event.is_simulated() && event.kind != Kind::Binding {
            return;
        }
        self.memory.observe(event);

        if matches!(event.from, Address::Senator(_) | Address::Guest(_)) {
            self.participants
                .insert(event.from.clone(), (event.seq, event.ts));
        }
        if let Some(root) = event.thread
            && (event.kind == Kind::Answer || is_seat_removed(event))
        {
            self.open.remove(&root);
            self.archived.remove(&root);
        }
        // An archived thread waits on no one, as on the task board: a
        // notice whose prompt was settled, a question its asker closed. A
        // later status that is not archived opens it again.
        if let Some(root) = event.thread
            && event.kind == Kind::Status
            && let Some(status) = StatusPayload::of(event)
        {
            if status.state == StatusState::Archived {
                if let Some(question) = self.open.remove(&root) {
                    self.archived.insert(root, question);
                }
            } else if let Some(question) = self.archived.remove(&root) {
                self.open.insert(root, question);
            }
        }
        // Handed to the seat that replaced the one it was put to: that seat
        // answers it now.
        if let Some(root) = event.thread
            && let Some(seat) = handed_to(event)
            && let Some(question) = self.open.get_mut(&root)
        {
            let seat = Address::senator(&seat);
            if !question.to.contains(&seat) {
                question.to.push(seat);
            }
        }

        match &event.kind {
            Kind::Decision => {
                for old in supersedes(event) {
                    if self.decisions.remove(&old).is_none() {
                        self.superseded.push(old);
                    }
                }
                if !self.superseded.contains(&event.seq) {
                    self.decisions.insert(
                        event.seq,
                        BriefDecision {
                            seq: event.seq,
                            ts: event.ts,
                            by: principal(event),
                            body: event.body.clone(),
                        },
                    );
                }
            }
            Kind::Question if event.thread.is_none() => {
                self.open.insert(
                    event.seq,
                    BriefQuestion {
                        seq: event.seq,
                        ts: event.ts,
                        from: principal(event),
                        to: event.to.clone(),
                        intended: intended(event),
                        body: cut(&event.body, QUESTION_CHARS),
                    },
                );
            }
            // A checkpoint is an agent's (the server takes it from no one
            // else); one that is not from a senator is not kept.
            Kind::Checkpoint => {
                if let Some(agent) = event.from.as_senator() {
                    let payload = CheckpointPayload::of(event);
                    // Its latest checkpoint is what counts: one without
                    // the notes takes the earlier ones away.
                    let notes = payload
                        .as_ref()
                        .and_then(|p| p.files.decisions.as_deref())
                        .map(str::trim)
                        .filter(|notes| !notes.is_empty());
                    match notes {
                        Some(notes) => self.decision_notes.insert(
                            agent.clone(),
                            BriefDecisionNotes {
                                seq: event.seq,
                                ts: event.ts,
                                agent: agent.clone(),
                                notes: cut(notes, DECISION_NOTE_CHARS),
                            },
                        ),
                        None => self.decision_notes.remove(agent),
                    };
                    self.checkpoints.insert(
                        agent.clone(),
                        BriefCheckpoint {
                            seq: event.seq,
                            ts: event.ts,
                            agent: agent.clone(),
                            body: event.body.clone(),
                            refs: event.refs.clone(),
                            branch: payload.as_ref().map(|p| p.branch.clone()),
                            commit: payload.as_ref().map(|p| p.commit.clone()),
                            pushed: payload.as_ref().map(|p| p.pushed),
                            trigger: payload.as_ref().map(|p| p.trigger),
                        },
                    );
                }
            }
            Kind::Binding if event.from == Address::Speaker => {
                if let Some(b) = event.binding() {
                    self.groups
                        .retain(|g| !(g.platform == b.platform && g.group_id == b.group_id));
                    if b.action == BindingAction::Bind {
                        self.groups.push(BriefGroup {
                            platform: b.platform,
                            group_id: b.group_id,
                            group_name: b.group_name,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    /// Every question root still open, oldest first (the brief keeps only
    /// the latest [`MAX_OPEN_QUESTIONS`]).
    pub fn open_questions(&self) -> impl Iterator<Item = &BriefQuestion> {
        self.open.values()
    }

    /// Whether the log unseated `agent` from this committee (and has not
    /// seated it again since).
    pub fn is_unseated(&self, agent: &SenatorName) -> bool {
        self.presence.seats().is_unseated(agent)
    }

    /// The committee's memory, folded with the rest.
    pub fn memory(&self) -> &MemoryFold {
        &self.memory
    }

    pub fn brief(&self) -> Brief {
        let decisions = last_n(self.decisions.values(), MAX_DECISIONS);
        let open_questions = last_n(self.open.values(), MAX_OPEN_QUESTIONS);
        let mut checkpoints: Vec<BriefCheckpoint> = self.checkpoints.values().cloned().collect();
        checkpoints.sort_by_key(|c| c.seq);
        let mut participants: Vec<BriefParticipant> = self
            .participants
            .iter()
            .map(|(address, (seq, ts))| BriefParticipant {
                address: address.clone(),
                last_seq: *seq,
                last_ts: *ts,
            })
            .collect();
        participants.sort_by_key(|p| std::cmp::Reverse(p.last_seq));
        // A recorded decision is the committee's; until there is one, what
        // its agents noted as decided is the nearest thing to read.
        let mut decision_notes: Vec<BriefDecisionNotes> = if decisions.is_empty() {
            self.decision_notes.values().cloned().collect()
        } else {
            Vec::new()
        };
        decision_notes.sort_by_key(|n| std::cmp::Reverse(n.seq));
        decision_notes.truncate(MAX_DECISION_NOTES);
        Brief {
            committee: self.committee.clone(),
            head: self.head,
            decision_notes,
            top: self.memory.focus().map(|p| BriefTop {
                seq: p.seq,
                ts: p.ts,
                body: p.text.clone(),
            }),
            decisions,
            open_questions,
            groups: self.groups.clone(),
            checkpoints,
            presence: self
                .presence
                .agents()
                .into_iter()
                .map(|mut p| {
                    p.duty = self.duties.duty(&p.agent).map(str::to_owned);
                    p
                })
                .collect(),
            participants,
            memory: last_n(self.memory.facts(), MAX_BRIEF_POINTS),
            digest: self.memory.latest_digest().cloned(),
        }
    }
}

impl Brief {
    /// Build from a whole log (or any prefix of it) in one go.
    pub fn from_events<'a>(
        committee: CommitteeId,
        events: impl IntoIterator<Item = &'a Event>,
    ) -> Self {
        let mut builder = BriefBuilder::new(committee);
        for event in events {
            builder.observe(event);
        }
        builder.brief()
    }

    /// The brief as Markdown in `locale`, at most `max_chars` characters.
    /// Sections are dropped from the end (participants, then presence, then
    /// checkpoints, then open questions) before decisions are cut, and the
    /// oldest decisions go first. Ages are relative to now.
    pub fn to_markdown(&self, locale: Locale, max_chars: usize) -> String {
        self.to_markdown_at(locale, max_chars, Utc::now())
    }

    /// [`Brief::to_markdown`] with ages relative to `now`.
    pub fn to_markdown_at(&self, locale: Locale, max_chars: usize, now: DateTime<Utc>) -> String {
        let header = self.header_markdown(locale);
        let mut decisions = self.decisions.as_slice();
        let tails = [
            self.decision_notes_markdown(locale, now),
            self.digest_markdown(locale),
            self.memory_markdown(locale),
            self.open_questions_markdown(locale),
            self.checkpoints_markdown(locale, now),
            self.presence_markdown(locale),
            self.participants_markdown(locale),
        ];
        loop {
            let mut out = header.clone();
            out.push_str(&decisions_markdown(decisions, self.decisions.len(), locale));
            for tail in &tails {
                if out.chars().count() + tail.chars().count() > max_chars {
                    break;
                }
                out.push_str(tail);
            }
            if out.chars().count() <= max_chars || decisions.is_empty() {
                return cut(&out, max_chars);
            }
            decisions = &decisions[1..];
        }
    }

    fn header_markdown(&self, locale: Locale) -> String {
        let (committee, head) = (&self.committee, self.head);
        let mut out = match locale {
            Locale::Zh => format!("# 简报：{committee}（截至 seq {head}）\n"),
            Locale::En => format!("# Brief: {committee} (through seq {head})\n"),
        };
        if let Some(top) = &self.top {
            let heading = match locale {
                Locale::Zh => format!("当前最关键的三件事（seq {}）", top.seq),
                Locale::En => format!("Top of mind (seq {})", top.seq),
            };
            let _ = write!(out, "\n## {heading}\n\n{}\n", top.body.trim());
        }
        if !self.groups.is_empty() {
            let _ = write!(
                out,
                "\n## {}\n\n",
                locale.pick("已绑定的群", "Bound groups")
            );
            for g in &self.groups {
                let name = g.group_name.as_deref().unwrap_or(&g.group_id);
                let _ = match locale {
                    Locale::Zh => writeln!(out, "- {name}（{}:{}）", g.platform, g.group_id),
                    Locale::En => writeln!(out, "- {name} ({}:{})", g.platform, g.group_id),
                };
            }
        }
        out
    }

    /// What follows "None recorded yet." under Decisions: the agents' own
    /// DECISIONS notes, said to be exactly that. Like every tail it is
    /// left out when the brief has no room for it.
    fn decision_notes_markdown(&self, locale: Locale, now: DateTime<Utc>) -> String {
        if self.decision_notes.is_empty() {
            return String::new();
        }
        let mut out = String::from(locale.pick(
            "\n各 agent 在最近的检查点里记下的决定（它们自己的 DECISIONS 笔记，未经任何人确认）：\n\n",
            "\nWhat agents noted as decided in their latest checkpoints (their own DECISIONS \
             notes, confirmed by no one):\n\n",
        ));
        for n in &self.decision_notes {
            let _ = writeln!(
                out,
                "- {} · seq {} · {}{}",
                Address::senator(&n.agent).in_text(),
                n.seq,
                age(now, n.ts, locale),
                colon(locale).trim_end()
            );
            for line in n.notes.lines().filter(|line| !line.trim().is_empty()) {
                let _ = writeln!(out, "  {}", line.trim_end());
            }
        }
        out
    }

    fn digest_markdown(&self, locale: Locale) -> String {
        let Some(digest) = &self.digest else {
            return String::new();
        };
        let mut out = match locale {
            Locale::Zh => format!(
                "\n## 近况（seq {}，截至 seq {}）\n\n",
                digest.seq, digest.payload.through
            ),
            Locale::En => format!(
                "\n## Latest discussion (seq {}, through seq {})\n\n",
                digest.seq, digest.payload.through
            ),
        };
        for point in &digest.payload.points {
            let _ = writeln!(
                out,
                "- {}{}",
                one_line(&point.text),
                refs_words(&point.refs, locale)
            );
        }
        out
    }

    fn memory_markdown(&self, locale: Locale) -> String {
        if self.memory.is_empty() {
            return String::new();
        }
        let mut out = format!("\n## {}\n\n", locale.pick("我们知道的", "What we know"));
        for point in &self.memory {
            let _ = writeln!(
                out,
                "- seq {} · {}{}",
                point.seq,
                one_line(&point.text),
                refs_words(&point.refs, locale)
            );
        }
        out
    }

    fn open_questions_markdown(&self, locale: Locale) -> String {
        if self.open_questions.is_empty() {
            return String::new();
        }
        let mut out = format!("\n## {}\n\n", locale.pick("待回答的问题", "Open questions"));
        for q in &self.open_questions {
            let to =
                q.to.iter()
                    .map(Address::in_text)
                    .collect::<Vec<_>>()
                    .join(locale.pick("、", ", "));
            let _ = writeln!(
                out,
                "- seq {} · {} → {to}{}{}",
                q.seq,
                q.from.in_text(),
                colon(locale),
                one_line(&q.body)
            );
        }
        out
    }

    fn checkpoints_markdown(&self, locale: Locale, now: DateTime<Utc>) -> String {
        if self.checkpoints.is_empty() {
            return String::new();
        }
        let mut out = format!(
            "\n## {}\n\n",
            locale.pick("最近检查点", "Latest checkpoints")
        );
        for c in &self.checkpoints {
            let at = match (&c.branch, &c.commit) {
                (Some(branch), Some(commit)) => {
                    let pushed = match c.pushed {
                        Some(true) => locale.pick("，已推送", ", pushed"),
                        Some(false) => locale.pick("，未推送", ", not pushed"),
                        None => "",
                    };
                    format!(" · {branch}@{}{pushed}", short_commit(commit))
                }
                _ => c
                    .refs
                    .as_ref()
                    .and_then(|r| r.branch.as_deref().or(r.commit.as_deref()))
                    .map(|r| format!(" · {r}"))
                    .unwrap_or_default(),
            };
            let _ = writeln!(
                out,
                "- {} {}{at} · {} · seq {}{}{}",
                Address::senator(&c.agent).in_text(),
                locale.pick("最近检查点", "latest checkpoint"),
                age(now, c.ts, locale),
                c.seq,
                colon(locale),
                one_line(&c.body)
            );
        }
        out
    }

    fn presence_markdown(&self, locale: Locale) -> String {
        if self.presence.is_empty() {
            return String::new();
        }
        let mut out = format!("\n## {}\n\n", locale.pick("在场", "Presence"));
        for p in &self.presence {
            let _ = write!(
                out,
                "- {} {}",
                Address::senator(&p.agent).in_text(),
                crate::continuity::presence_label(p.state, p.paused, locale)
            );
            if let Some(owner) = &p.new_owner {
                let _ = write!(out, " → {}", Address::senator(owner).in_text());
            }
            if p.state != PresenceState::Online {
                let since = p.since.format("%Y-%m-%d %H:%M UTC");
                let _ = match locale {
                    Locale::Zh => write!(out, "（自 {since} 起）"),
                    Locale::En => write!(out, " since {since}"),
                };
            }
            if let Some(lag) = p.lag.filter(|l| *l > 0) {
                let behind = lag_words(lag, locale);
                let _ = match locale {
                    Locale::Zh => write!(out, " · 在 {} 上{behind}", self.committee),
                    Locale::En => write!(out, " · {behind} on {}", self.committee),
                };
            }
            if let Some(duty) = &p.duty {
                let _ = match locale {
                    Locale::Zh => write!(out, " · 职责：{duty}"),
                    Locale::En => write!(out, " · duty: {duty}"),
                };
            }
            out.push('\n');
        }
        out
    }

    fn participants_markdown(&self, locale: Locale) -> String {
        if self.participants.is_empty() {
            return String::new();
        }
        let list = self
            .participants
            .iter()
            .map(|p| match locale {
                Locale::Zh => format!("{}（seq {}）", p.address.in_text(), p.last_seq),
                Locale::En => format!("{} (seq {})", p.address.in_text(), p.last_seq),
            })
            .collect::<Vec<_>>()
            .join(locale.pick("、", ", "));
        format!(
            "\n## {}\n\n{list}\n",
            locale.pick("最近活跃", "Recently active")
        )
    }
}

fn decisions_markdown(shown: &[BriefDecision], total: usize, locale: Locale) -> String {
    let mut out = format!("\n## {}\n\n", locale.pick("决定", "Decisions"));
    if total == 0 {
        out.push_str(locale.pick("尚无记录。\n", "None recorded yet.\n"));
        return out;
    }
    if shown.len() < total {
        let omitted = total - shown.len();
        let _ = match locale {
            Locale::Zh => writeln!(
                out,
                "（更早的 {omitted} 条决定已省略，可用 search 查找。）\n"
            ),
            Locale::En if omitted == 1 => {
                writeln!(out, "(1 older decision omitted; use search.)\n")
            }
            Locale::En => writeln!(out, "({omitted} older decisions omitted; use search.)\n"),
        };
    }
    for d in shown {
        let _ = writeln!(
            out,
            "- seq {} · {} · {}{}{}",
            d.seq,
            d.ts.format("%Y-%m-%d"),
            d.by.in_text(),
            colon(locale),
            d.body.trim()
        );
    }
    out
}

/// ` (from seq 3, 7)` after a point, or nothing without refs.
fn refs_words(refs: &[Seq], locale: Locale) -> String {
    if refs.is_empty() {
        return String::new();
    }
    let list = refs
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    match locale {
        Locale::Zh => format!("（出自 seq {list}）"),
        Locale::En => format!(" (from seq {list})"),
    }
}

/// What separates who from what they said.
fn colon(locale: Locale) -> &'static str {
    locale.pick("：", ": ")
}

/// Whose event it is: the `on_behalf_of` when set, else `from`.
fn principal(event: &Event) -> Address {
    event
        .on_behalf_of
        .clone()
        .unwrap_or_else(|| event.from.clone())
}

/// `meta.supersedes` of a decision: one seq or a list of seqs.
fn supersedes(event: &Event) -> Vec<Seq> {
    match event.meta.get(meta::SUPERSEDES) {
        Some(serde_json::Value::Number(n)) => n.as_u64().map(Seq).into_iter().collect(),
        Some(serde_json::Value::Array(items)) => {
            items.iter().filter_map(|v| v.as_u64().map(Seq)).collect()
        }
        _ => Vec::new(),
    }
}

fn last_n<'a, T: Clone + 'a>(items: impl DoubleEndedIterator<Item = &'a T>, n: usize) -> Vec<T> {
    let mut out: Vec<T> = items.rev().take(n).cloned().collect();
    out.reverse();
    out
}

fn cut(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        None => s.to_string(),
        Some((i, _)) => format!("{}…", &s[..i]),
    }
}

/// `now - ts` in the largest whole unit: `3 min ago`, `2 h ago`, `4 d ago`;
/// `3 分钟前`, `2 小时前`, `4 天前`.
fn age(now: DateTime<Utc>, ts: DateTime<Utc>, locale: Locale) -> String {
    let secs = (now - ts).num_seconds().max(0);
    match (secs, locale) {
        (s, Locale::Zh) if s < 60 => "刚刚".to_string(),
        (s, Locale::En) if s < 60 => "just now".to_string(),
        (s, Locale::Zh) if s < 3_600 => format!("{} 分钟前", s / 60),
        (s, Locale::En) if s < 3_600 => format!("{} min ago", s / 60),
        (s, Locale::Zh) if s < 86_400 => format!("{} 小时前", s / 3_600),
        (s, Locale::En) if s < 86_400 => format!("{} h ago", s / 3_600),
        (s, Locale::Zh) => format!("{} 天前", s / 86_400),
        (s, Locale::En) => format!("{} d ago", s / 86_400),
    }
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use serde_json::json;

    use super::*;
    use crate::continuity::{CheckpointFiles, PresencePayload, presence_event};
    use crate::event::{BindingPayload, GroupRole, NewEvent};
    use crate::testing::assert_senate_vocabulary;

    fn c() -> CommitteeId {
        CommitteeId::new("refund-v2").unwrap()
    }

    fn addr(s: &str) -> Address {
        s.parse().unwrap()
    }

    struct Log(Vec<Event>);

    impl Log {
        fn push(&mut self, e: NewEvent) -> Seq {
            self.push_at(e, Utc::now())
        }
        fn push_at(&mut self, e: NewEvent, ts: DateTime<Utc>) -> Seq {
            let seq = Seq(self.0.len() as u64 + 1);
            self.0.push(e.into_event(seq, ts));
            seq
        }
    }

    fn ev(kind: Kind, from: &str, body: &str) -> NewEvent {
        NewEvent::new(c(), kind, addr(from), body)
    }

    fn bind(action: BindingAction, group: &str) -> NewEvent {
        ev(Kind::Binding, "speaker", "").with_binding(&BindingPayload {
            action,
            platform: "chat".into(),
            group_id: group.into(),
            group_name: Some(format!("{group} 群")),
            role: GroupRole::People,
        })
    }

    /// Thread 797 item 7: a question to a seat that is gone closes when
    /// the Speaker says so in its thread; nobody else's note closes it.
    #[test]
    fn a_seat_removed_note_from_the_speaker_closes_the_question() {
        let mut log = Log(Vec::new());
        let gone = log.push(
            ev(Kind::Question, "speaker", "退款服务怎么部署？")
                .to(Address::Child {
                    senate: crate::Name::new("bob-desk").unwrap(),
                    committee: c(),
                })
                .on_behalf_of(addr("senator::wendell"))
                .with_meta(
                    meta::INTENDED,
                    json!("/vm/bob-desk::refund-v2::senator::codex"),
                ),
        );
        let kept = log
            .push(ev(Kind::Question, "senator::claude", "重试几次？").to(addr("senator::copilot")));
        log.push(
            ev(Kind::Note, "senator::claude", "fake")
                .in_thread(kept)
                .with_meta(meta::SEAT_REMOVED, json!("senator::copilot")),
        );
        let mut builder = BriefBuilder::new(c());
        for e in &log.0 {
            builder.observe(e);
        }
        let open: Vec<_> = builder.open_questions().map(|q| q.seq).collect();
        assert_eq!(open, [gone, kept]);
        assert_eq!(
            builder.brief().open_questions[0]
                .intended
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("/vm/bob-desk::refund-v2::senator::codex")
        );
        log.push(
            ev(Kind::Note, "speaker", "该席位已不在本委员会。")
                .in_thread(gone)
                .to(addr("senator::wendell"))
                .with_meta(
                    meta::SEAT_REMOVED,
                    json!("/vm/bob-desk::refund-v2::senator::codex"),
                ),
        );
        let brief = Brief::from_events(c(), &log.0);
        let open: Vec<_> = brief.open_questions.iter().map(|q| q.seq).collect();
        assert_eq!(open, [kept]);
    }

    /// A permission-prompt notice is a question to the chair that nobody
    /// answers: it is closed by an archived status once the prompt is
    /// settled, and stayed in the brief for good.
    #[test]
    fn an_archived_thread_is_no_longer_an_open_question() {
        let status = |root: Seq, state: StatusState| {
            ev(Kind::Status, "senator::codex", "")
                .in_thread(root)
                .with_meta(meta::STATUS, StatusPayload::new(state).value())
        };
        let mut log = Log(Vec::new());
        let notice = log.push(
            ev(
                Kind::Question,
                "senator::codex",
                "has a permission prompt waiting",
            )
            .to(addr("chair")),
        );
        let kept = log
            .push(ev(Kind::Question, "senator::claude", "重试几次？").to(addr("senator::copilot")));
        // Progress on a thread leaves it open.
        log.push(status(kept, StatusState::Processing));
        log.push(status(notice, StatusState::Archived));
        let open = |log: &Log| -> Vec<Seq> {
            Brief::from_events(c(), &log.0)
                .open_questions
                .iter()
                .map(|q| q.seq)
                .collect()
        };
        assert_eq!(open(&log), [kept]);

        // Taken up again, it is open again; answered, it is closed for good.
        log.push(status(notice, StatusState::Processing));
        assert_eq!(open(&log), [notice, kept]);
        log.push(status(notice, StatusState::Archived));
        log.push(ev(Kind::Answer, "senator::claude", "allowed").in_thread(notice));
        log.push(status(notice, StatusState::Active));
        assert_eq!(open(&log), [kept]);
    }

    #[test]
    fn a_question_handed_to_a_new_seat_counts_it_among_its_recipients() {
        let mut log = Log(Vec::new());
        let q = log
            .push(ev(Kind::Question, "senator::claude", "重试几次？").to(addr("senator::copilot")));
        log.push(
            ev(Kind::Question, "senator::wendell", "handed over")
                .in_thread(q)
                .to(addr("senator::codex"))
                .with_meta(meta::HANDED_TO, json!("codex")),
        );
        // Handed twice to the same seat: listed once.
        log.push(
            ev(Kind::Note, "senator::wendell", "again")
                .in_thread(q)
                .with_meta(meta::HANDED_TO, json!("senator::codex")),
        );
        let brief = Brief::from_events(c(), &log.0);
        assert_eq!(brief.open_questions.len(), 1);
        assert_eq!(
            brief.open_questions[0].to,
            [addr("senator::copilot"), addr("senator::codex")]
        );
    }

    #[test]
    fn decisions_questions_groups_and_top() {
        let mut log = Log(Vec::new());
        log.push(bind(BindingAction::Bind, "backend"));
        log.push(bind(BindingAction::Bind, "product"));
        let d1 = log.push(
            ev(Kind::Decision, "speaker", "退款上限 14 天").on_behalf_of(addr("senator::pm")),
        );
        let q = log.push(
            ev(Kind::Question, "senator::claude", "用哪个 header？").to(addr("senator::copilot")),
        );
        let q2 = log
            .push(ev(Kind::Question, "senator::copilot", "重试几次？").to(addr("senator::claude")));
        log.push(ev(Kind::Answer, "senator::copilot", "Idempotency-Key").in_thread(q));
        let d2 = log.push(
            ev(Kind::Decision, "senator::pm", "退款上限 30 天")
                .with_meta(meta::SUPERSEDES, json!(d1.0)),
        );
        log.push(bind(BindingAction::Unbind, "backend"));
        let top = log.push(
            ev(Kind::Note, "speaker", "1. 上限 30 天").with_meta(meta::BRIEF_TOP, json!(true)),
        );
        // A note from anyone else with the flag is not the top.
        log.push(
            ev(Kind::Note, "senator::copilot", "fake").with_meta(meta::BRIEF_TOP, json!(true)),
        );

        let brief = Brief::from_events(c(), &log.0);
        assert_eq!(brief.head, Seq(10));
        assert_eq!(brief.decisions.len(), 1);
        assert_eq!(brief.decisions[0].seq, d2);
        assert_eq!(brief.decisions[0].by, addr("senator::pm"));
        assert_eq!(
            brief
                .open_questions
                .iter()
                .map(|q| q.seq)
                .collect::<Vec<_>>(),
            [q2]
        );
        assert_eq!(brief.groups.len(), 1);
        assert_eq!(brief.groups[0].group_id, "product");
        assert_eq!(brief.top.as_ref().map(|t| t.seq), Some(top));
        assert_eq!(brief.participants[0].address, addr("senator::copilot"));

        for locale in Locale::ALL {
            let md = brief.to_markdown(locale, DEFAULT_MARKDOWN_CHARS);
            assert!(md.contains("退款上限 30 天"));
            assert!(!md.contains("退款上限 14 天"));
            assert!(md.contains("1. 上限 30 天"));
            assert!(md.contains("重试几次"));
        }
    }

    #[test]
    fn a_supersession_that_arrives_first_still_applies() {
        let mut log = Log(Vec::new());
        log.push(ev(Kind::Decision, "senator::pm", "A").with_meta(meta::SUPERSEDES, json!([2])));
        log.push(ev(Kind::Decision, "senator::pm", "B"));
        let brief = Brief::from_events(c(), &log.0);
        assert_eq!(
            brief
                .decisions
                .iter()
                .map(|d| d.body.as_str())
                .collect::<Vec<_>>(),
            ["A"]
        );
    }

    #[test]
    fn incremental_equals_whole_and_overlap_is_ignored() {
        let mut log = Log(Vec::new());
        for i in 0..40 {
            log.push(ev(Kind::Decision, "senator::pm", &format!("d{i}")));
        }
        let mut b = BriefBuilder::new(c());
        for e in &log.0[..25] {
            b.observe(e);
        }
        for e in &log.0[10..] {
            b.observe(e);
        }
        let whole = Brief::from_events(c(), &log.0);
        assert_eq!(b.brief(), whole);
        assert_eq!(whole.decisions.len(), MAX_DECISIONS);
        assert_eq!(whole.decisions[0].body, "d10");
    }

    #[test]
    fn markdown_respects_its_bound_and_keeps_newest_decisions() {
        let mut log = Log(Vec::new());
        for i in 0..30 {
            log.push(ev(
                Kind::Decision,
                "senator::pm",
                &format!("decision {i} {}", "x".repeat(80)),
            ));
        }
        let brief = Brief::from_events(c(), &log.0);
        for (locale, omitted) in [
            (Locale::En, "older decisions omitted"),
            (Locale::Zh, "条决定已省略"),
        ] {
            let md = brief.to_markdown(locale, 1_000);
            assert!(md.chars().count() <= 1_001);
            assert!(md.contains("decision 29"));
            assert!(!md.contains("decision 0 "));
            assert!(md.contains(omitted), "{md}");
        }
    }

    fn checkpoint_payload() -> CheckpointPayload {
        CheckpointPayload {
            branch: "senate/refund-v2/alice".into(),
            commit: "a41f9c2deadbeef".into(),
            pushed: false,
            remote_url: None,
            files: CheckpointFiles::default(),
            session_transcript: false,
            trigger: CheckpointTrigger::Handoff,
        }
    }

    /// A log that fills every section of the brief.
    fn full_log(now: DateTime<Utc>) -> Log {
        let mut log = Log(Vec::new());
        log.push_at(
            ev(Kind::Checkpoint, "senator::claude", "幂等键做完了")
                .with_meta(meta::CHECKPOINT, checkpoint_payload().value()),
            now - Duration::minutes(3),
        );
        let since = "2026-11-16T09:00:00Z".parse().unwrap();
        log.push(presence_event(
            c(),
            &PresencePayload {
                paused: false,
                agent: "copilot".parse().unwrap(),
                state: PresenceState::Disconnected,
                since,
                since_seq: Seq(1),
                previous: Some(PresenceState::Online),
                last_seen: None,
                cursor: Some(Seq(0)),
                lag: Some(14),
                new_owner: None,
            },
        ));
        log.push(bind(BindingAction::Bind, "backend"));
        log.push_at(
            ev(Kind::Decision, "speaker", "cap is 30 days").on_behalf_of(addr("senator::pm")),
            "2026-11-15T08:00:00Z".parse().unwrap(),
        );
        log.push(
            ev(Kind::Question, "senator::copilot", "how many\nretries?")
                .to(addr("senator::claude"))
                .to(addr("senator::alice")),
        );
        log.push(ev(Kind::Chat, "guest::chat/u_8f3a", "when does it ship?"));
        log.push(ev(Kind::Note, "speaker", "1. ship it").with_meta(meta::BRIEF_TOP, json!(true)));
        log
    }

    #[test]
    fn checkpoints_and_presence_show_in_the_brief() {
        let now = Utc::now();
        let brief = Brief::from_events(c(), &full_log(now).0);
        assert_eq!(brief.checkpoints[0].seq, Seq(1));
        assert_eq!(
            brief.checkpoints[0].agent,
            "claude".parse::<SenatorName>().unwrap()
        );
        assert_eq!(brief.checkpoints[0].pushed, Some(false));
        assert_eq!(brief.presence.len(), 1);
        assert!(
            brief
                .participants
                .iter()
                .all(|p| p.address != Address::Committee && p.address != Address::Speaker),
            "the server's presence and the Speaker are not participants"
        );
        assert!(
            brief
                .participants
                .iter()
                .any(|p| p.address == addr("guest::chat/u_8f3a")),
            "a guest who spoke is"
        );
    }

    /// No committee had a decision event, so every brief said "None
    /// recorded yet." while its agents' checkpoints held what was decided.
    #[test]
    fn with_no_decision_recorded_the_brief_shows_what_agents_noted_as_decided() {
        let now = Utc::now();
        let cp = |from: &str, decisions: Option<&str>| {
            let mut payload = checkpoint_payload();
            payload.files.decisions = decisions.map(str::to_owned);
            ev(Kind::Checkpoint, from, "progress").with_meta(meta::CHECKPOINT, payload.value())
        };
        let mut log = Log(Vec::new());
        let ago = |minutes| now - Duration::minutes(minutes);
        log.push_at(
            cp("senator::main", Some("- merge order: perf first")),
            ago(50),
        );
        log.push_at(cp("senator::docs", Some("- old note")), ago(40));
        log.push_at(cp("senator::perf", Some(&"x".repeat(2_000))), ago(30));
        log.push_at(
            cp(
                "senator::app",
                Some("- ship the town behind a flag\n\n- no push"),
            ),
            ago(20),
        );
        log.push_at(cp("senator::quiet", None), ago(10));
        // Its latest checkpoint has no notes: the earlier ones are not shown.
        log.push_at(cp("senator::docs", Some("  \n")), ago(5));
        let third = log.push_at(
            cp("senator::main", Some("- release waits for Wendell")),
            ago(3),
        );

        let brief = Brief::from_events(c(), &log.0);
        let agents: Vec<String> = brief
            .decision_notes
            .iter()
            .map(|n| n.agent.to_string())
            .collect();
        assert_eq!(
            agents,
            ["main", "app", "perf"],
            "newest first, three at most"
        );
        assert_eq!(brief.decision_notes[0].seq, third);
        assert_eq!(
            brief.decision_notes[2].notes.chars().count(),
            DECISION_NOTE_CHARS + 1,
            "cut, with its mark"
        );
        let en = brief.to_markdown_at(Locale::En, DEFAULT_MARKDOWN_CHARS, now);
        assert!(
            en.contains(
                "\n## Decisions\n\nNone recorded yet.\n\
                 \nWhat agents noted as decided in their latest checkpoints (their own DECISIONS \
                 notes, confirmed by no one):\n\
                 \n- [senator::main] · seq 7 · 3 min ago:\n  - release waits for Wendell\n\
                 - [senator::app] · seq 4 · 20 min ago:\n  - ship the town behind a flag\n  - no push\n"
            ),
            "{en}"
        );
        assert!(
            !en.contains("merge order"),
            "an agent's latest notes only: {en}"
        );
        assert!(!en.contains("old note"), "{en}");
        let zh = brief.to_markdown_at(Locale::Zh, DEFAULT_MARKDOWN_CHARS, now);
        assert!(zh.contains("未经任何人确认"), "{zh}");
        assert_senate_vocabulary(&en);
        assert_senate_vocabulary(&zh);
        // With no room they go before anything else does.
        let tight = brief.to_markdown_at(Locale::En, 120, now);
        assert!(tight.contains("None recorded yet."), "{tight}");
        assert!(!tight.contains("noted as decided"), "{tight}");

        // A decision in the log is the committee's own: the notes step back.
        log.push(ev(Kind::Decision, "senator::pm", "cap is 30 days"));
        let brief = Brief::from_events(c(), &log.0);
        assert!(brief.decision_notes.is_empty());
        let en = brief.to_markdown_at(Locale::En, DEFAULT_MARKDOWN_CHARS, now);
        assert!(!en.contains("noted as decided"), "{en}");
        assert!(en.contains("cap is 30 days"), "{en}");
    }

    #[test]
    fn each_agent_has_its_own_checkpoint_and_the_speaker_none() {
        let mut log = Log(Vec::new());
        let cp = |from: &str, body: &str| {
            ev(Kind::Checkpoint, from, body)
                .with_meta(meta::CHECKPOINT, checkpoint_payload().value())
        };
        log.push(cp("senator::claude", "one"));
        log.push(cp("senator::codex", "two"));
        log.push(cp("senator::claude", "three"));
        // A person's would be refused by the server; the Speaker's is no
        // agent's.
        log.push(cp("speaker", "not an agent"));
        let brief = Brief::from_events(c(), &log.0);
        assert_eq!(
            brief
                .checkpoints
                .iter()
                .map(|c| (c.agent.to_string(), c.body.as_str()))
                .collect::<Vec<_>>(),
            [
                ("codex".to_string(), "two"),
                ("claude".to_string(), "three")
            ]
        );
    }

    #[test]
    fn the_brief_is_written_in_chinese() {
        let now = Utc::now();
        let brief = Brief::from_events(c(), &full_log(now).0);
        let md = brief.to_markdown_at(Locale::Zh, DEFAULT_MARKDOWN_CHARS, now);
        assert_eq!(
            md,
            "# 简报：refund-v2（截至 seq 7）\n\
             \n## 当前最关键的三件事（seq 7）\n\n1. ship it\n\
             \n## 已绑定的群\n\n- backend 群（chat:backend）\n\
             \n## 决定\n\n- seq 4 · 2026-11-15 · [senator::pm]：cap is 30 days\n\
             \n## 待回答的问题\n\n- seq 5 · [senator::copilot] → [senator::claude]、[senator::alice]：how many retries?\n\
             \n## 最近检查点\n\n- [senator::claude] 最近检查点 · senate/refund-v2/alice@a41f9c2，未推送 · 3 分钟前 · seq 1：幂等键做完了\n\
             \n## 在场\n\n- [senator::copilot] 失联（自 2026-11-16 09:00 UTC 起） · 在 refund-v2 上落后 14 条事件\n\
             \n## 最近活跃\n\n[guest::chat/u_8f3a]（seq 6）、[senator::copilot]（seq 5）、[senator::claude]（seq 1）\n"
        );
        assert_senate_vocabulary(&md);
    }

    #[test]
    fn what_we_know_and_the_latest_digest_show_in_the_brief() {
        use crate::memory::{DigestPayload, DigestPoint, DigestTrigger, MemoryPayload};
        let mut log = Log(Vec::new());
        log.push(ev(Kind::Chat, "senator::ana", "let's ship on Monday"));
        log.push(ev(Kind::Chat, "senator::ben", "agreed, Monday"));
        let point = |body: &str, refs: Vec<Seq>, supersedes: Vec<Seq>| {
            ev(Kind::Note, "speaker", body).with_meta(
                meta::MEMORY,
                MemoryPayload::Point {
                    refs,
                    supersedes,
                    digest: None,
                }
                .to_value(),
            )
        };
        let old = log.push(point("Release on Friday", vec![Seq(1)], vec![]));
        log.push(point("Release on Monday", vec![Seq(1), Seq(2)], vec![old]));
        log.push(
            ev(Kind::Note, "speaker", "- Release moved to Monday").with_meta(
                meta::DIGEST,
                DigestPayload {
                    trigger: DigestTrigger::Quiet,
                    from: Seq(1),
                    through: Seq(2),
                    points: vec![DigestPoint {
                        text: "Release moved to Monday".into(),
                        refs: vec![Seq(1), Seq(2)],
                    }],
                }
                .to_value(),
            ),
        );
        let brief = Brief::from_events(c(), &log.0);
        assert_eq!(brief.memory.len(), 1, "the superseded point is gone");
        let en = brief.to_markdown(Locale::En, DEFAULT_MARKDOWN_CHARS);
        assert!(en.contains(
            "\n## Latest discussion (seq 5, through seq 2)\n\n- Release moved to Monday (from seq 1, 2)\n"
        ));
        assert!(en.contains("\n## What we know\n\n- seq 4 · Release on Monday (from seq 1, 2)\n"));
        let zh = brief.to_markdown(Locale::Zh, DEFAULT_MARKDOWN_CHARS);
        assert!(zh.contains(
            "\n## 近况（seq 5，截至 seq 2）\n\n- Release moved to Monday（出自 seq 1, 2）\n"
        ));
        assert!(zh.contains("\n## 我们知道的\n\n- seq 4 · Release on Monday（出自 seq 1, 2）\n"));
        assert_senate_vocabulary(&en);
        assert_senate_vocabulary(&zh);
    }

    #[test]
    fn the_brief_is_written_in_english() {
        let now = Utc::now();
        let brief = Brief::from_events(c(), &full_log(now).0);
        let md = brief.to_markdown_at(Locale::En, DEFAULT_MARKDOWN_CHARS, now);
        assert_eq!(
            md,
            "# Brief: refund-v2 (through seq 7)\n\
             \n## Top of mind (seq 7)\n\n1. ship it\n\
             \n## Bound groups\n\n- backend 群 (chat:backend)\n\
             \n## Decisions\n\n- seq 4 · 2026-11-15 · [senator::pm]: cap is 30 days\n\
             \n## Open questions\n\n- seq 5 · [senator::copilot] → [senator::claude], [senator::alice]: how many retries?\n\
             \n## Latest checkpoints\n\n- [senator::claude] latest checkpoint · senate/refund-v2/alice@a41f9c2, not pushed · 3 min ago · seq 1: 幂等键做完了\n\
             \n## Presence\n\n- [senator::copilot] disconnected since 2026-11-16 09:00 UTC · 14 events behind on refund-v2\n\
             \n## Recently active\n\n[guest::chat/u_8f3a] (seq 6), [senator::copilot] (seq 5), [senator::claude] (seq 1)\n"
        );
        assert_senate_vocabulary(&md);
    }

    #[test]
    fn an_empty_brief_says_so_in_each_language() {
        let brief = Brief::from_events(c(), &[]);
        assert_eq!(
            brief.to_markdown(Locale::Zh, DEFAULT_MARKDOWN_CHARS),
            "# 简报：refund-v2（截至 seq 0）\n\n## 决定\n\n尚无记录。\n"
        );
        assert_eq!(
            brief.to_markdown(Locale::En, DEFAULT_MARKDOWN_CHARS),
            "# Brief: refund-v2 (through seq 0)\n\n## Decisions\n\nNone recorded yet.\n"
        );
    }

    #[test]
    fn ages_are_written_in_each_language() {
        let now = Utc::now();
        let ago = |secs: i64, locale| age(now, now - Duration::seconds(secs), locale);
        assert_eq!(ago(5, Locale::Zh), "刚刚");
        assert_eq!(ago(5, Locale::En), "just now");
        assert_eq!(ago(7_300, Locale::Zh), "2 小时前");
        assert_eq!(ago(7_300, Locale::En), "2 h ago");
        assert_eq!(ago(4 * 86_400, Locale::Zh), "4 天前");
        assert_eq!(ago(4 * 86_400, Locale::En), "4 d ago");
    }

    #[test]
    fn round_trips_as_json_and_holds_no_words() {
        let now = Utc::now();
        let brief = Brief::from_events(c(), &full_log(now).0);
        let value = serde_json::to_value(&brief).unwrap();
        let back: Brief = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(back, brief);
        // The data names states and addresses; the words come at rendering.
        assert_eq!(value["presence"][0]["state"], "disconnected");
        assert_eq!(value["presence"][0]["agent"], "copilot");
        assert_eq!(value["checkpoints"][0]["agent"], "claude");
        assert_eq!(value["committee"], "refund-v2");
    }
}
