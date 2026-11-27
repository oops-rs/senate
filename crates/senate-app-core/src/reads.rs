//! Reads the app makes on demand and shows as they come back, outside the
//! [`crate::state::AppState`] snapshot: one thread's events. Like the
//! snapshot, every sentence is written in the app's language and every id
//! as a bracketed mention.

use std::time::SystemTime;

use senate_bridge::ipc::{DeskItemDetail, DeskOutcome, ThreadReply};
use senate_core::{Address, Event, Locale, Name};

use crate::text;

/// One thread, root first.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct ThreadView {
    pub committee: String,
    /// The root's seq.
    pub root: u64,
    /// Its title, as its task board folds it (else the root's first line):
    /// what the page heads with when the thread is not in the inbox.
    pub title: String,
    /// Its state (`processing`, `suspended`, `active`, `archived`) and
    /// that in words, when it is a task thread.
    pub state: Option<String>,
    pub state_label: Option<String>,
    /// What kind its root is, in words.
    pub kind_label: String,
    pub events: Vec<ThreadEvent>,
}

/// One event in a thread.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct ThreadEvent {
    pub seq: u64,
    /// The wire spelling (`question`, `answer`, `status`).
    pub kind: String,
    pub kind_label: String,
    /// Who wrote it, as shown to people (`[speaker::claude] takeover`).
    /// For words the Speaker's office carried (a request it forwarded from
    /// another committee, an answer it relayed back), whose words they
    /// are: the asker there, or who answered; `via` says how they came.
    pub by: String,
    /// How the office carried them and who acted for it, when `by` is not
    /// the office: `forwarded from commons #109 by the Speaker's desk
    /// ([senator::wendell])`. The stored event is unchanged: it is still
    /// `from` the Speaker.
    pub via: Option<String>,
    /// Who it was addressed to.
    pub to: Vec<String>,
    /// What was said, as written.
    pub body: String,
    pub at: SystemTime,
    /// The person wrote it (`senator::<person>`): the app's own side.
    pub mine: bool,
    /// A task status (`kind` `status`): what it says, as one activity line.
    pub status: Option<StatusLine>,
}

/// A task status in a thread, as a compact activity line: the state in
/// words with the task's title ("working on it — Wait for startup seat
/// reports"), the checklist when it gives one, its links.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct StatusLine {
    /// `processing`, `active`, `suspended`, `archived`.
    pub state: String,
    /// The line: the state in words, then the title. One line, whatever
    /// the status said at length (that is `reason`).
    pub line: String,
    /// Why, in full and as written (Markdown, newlines kept): an
    /// escalation's or a suspension's reason, shown under the line and
    /// rendered like any message body.
    pub reason: Option<String>,
    pub steps: Vec<StatusStep>,
    pub links: Vec<StatusLink>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct StatusStep {
    pub label: String,
    pub done: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct StatusLink {
    pub label: String,
    pub url: String,
}

/// What the person may do with a desk item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum DeskAction {
    Forward,
    Refuse,
    Answer,
    Close,
}

impl From<senate_bridge::ipc::DeskAction> for DeskAction {
    fn from(action: senate_bridge::ipc::DeskAction) -> Self {
        use senate_bridge::ipc::DeskAction as A;
        match action {
            A::Forward => Self::Forward,
            A::Refuse => Self::Refuse,
            A::Answer => Self::Answer,
            A::Close => Self::Close,
        }
    }
}

/// A decision on a desk item ([`crate::ffi::AppCore::decide_desk_item`]).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum DeskChoice {
    Forward,
    Refuse {
        reason: String,
    },
    /// Answer as the Speaker (Markdown).
    Answer {
        body: String,
    },
    /// Close it with no answer, saying why when given.
    Close {
        reason: Option<String>,
    },
}

impl From<DeskChoice> for senate_bridge::ipc::DeskDecision {
    fn from(choice: DeskChoice) -> Self {
        match choice {
            DeskChoice::Forward => Self::Forward,
            DeskChoice::Refuse { reason } => Self::Refuse { reason },
            DeskChoice::Answer { body } => Self::Answer { body },
            DeskChoice::Close { reason } => Self::Close { reason },
        }
    }
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct DeskItemView {
    pub event_id: String,
    pub item: ThreadEvent,
    pub thread: ThreadView,
    pub origin: Option<String>,
    pub destination: Option<String>,
    pub destination_label: Option<String>,
    pub blocked: Option<String>,
    /// Exactly what this person may do with it now.
    pub actions: Vec<DeskAction>,
    /// When nobody here can act: on whom or what it waits.
    pub waits_on: Option<String>,
}

/// Which decision a [`DeskOutcomeView`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum DeskOutcomeKind {
    Delivered,
    Refused,
    Answered,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DeskOutcomeView {
    pub kind: DeskOutcomeKind,
    /// Kept for older callers: `kind == Delivered`.
    pub delivered: bool,
    /// Where it went (delivered or refused); empty for an answer or close.
    pub destination: String,
    pub destination_label: String,
    pub committee: String,
    pub seq: u64,
    /// Why it was closed, when said.
    pub reason: Option<String>,
}

impl DeskOutcomeView {
    pub(crate) fn new(value: DeskOutcome, locale: Locale) -> Self {
        let (kind, destination, committee, seq, reason) = match value {
            DeskOutcome::Delivered {
                destination,
                committee,
                seq,
            } => (
                DeskOutcomeKind::Delivered,
                destination,
                committee,
                seq,
                None,
            ),
            DeskOutcome::Refused {
                destination,
                committee,
                seq,
            } => (DeskOutcomeKind::Refused, destination, committee, seq, None),
            DeskOutcome::Answered { committee, seq } => (
                DeskOutcomeKind::Answered,
                String::new(),
                committee,
                seq,
                None,
            ),
            DeskOutcome::Closed {
                committee,
                seq,
                reason,
            } => (
                DeskOutcomeKind::Closed,
                String::new(),
                committee,
                seq,
                reason,
            ),
        };
        Self {
            destination_label: if destination.is_empty() {
                String::new()
            } else {
                crate::names::current().desk_destination(&destination, locale)
            },
            kind,
            delivered: kind == DeskOutcomeKind::Delivered,
            destination,
            committee: committee.to_string(),
            seq: seq.0,
            reason,
        }
    }
}

pub(crate) fn desk_item_view(
    detail: DeskItemDetail,
    person: Option<&Name>,
    locale: Locale,
) -> DeskItemView {
    let me = person.map(Address::senior);
    DeskItemView {
        event_id: detail.item.id.to_string(),
        item: thread_event(&detail.item, me.as_ref(), locale),
        thread: thread_view(
            ThreadReply {
                committee: detail.item.committee,
                root: detail.item.thread.unwrap_or(detail.item.seq),
                events: detail.events,
            },
            person,
            locale,
        ),
        origin: detail.origin,
        destination_label: detail
            .destination
            .as_deref()
            .map(|raw| crate::names::current().desk_destination(raw, locale)),
        destination: detail.destination,
        blocked: detail.blocked,
        actions: detail.actions.into_iter().map(DeskAction::from).collect(),
        waits_on: detail.waits_on,
    }
}

pub(crate) fn thread_view(reply: ThreadReply, person: Option<&Name>, locale: Locale) -> ThreadView {
    let me = person.map(Address::senior);
    let mut board = senate_core::TaskBoard::new(crate::names::current().people.clone());
    for e in &reply.events {
        board.observe(e);
    }
    let summary = board.get(reply.root);
    let root = reply.events.iter().find(|e| e.seq == reply.root);
    ThreadView {
        title: summary.map_or_else(
            || {
                root.map(|e| e.body.lines().next().unwrap_or_default().trim().to_string())
                    .unwrap_or_default()
            },
            |t| t.title.clone(),
        ),
        state: summary.map(|t| crate::state::wire(&t.state)),
        state_label: summary.map(|t| text::state(locale, t.state).to_string()),
        kind_label: root.map_or_else(String::new, |e| text::kind(locale, &e.kind)),
        committee: reply.committee.to_string(),
        root: reply.root.0,
        events: thread_events(&reply.events, me.as_ref(), locale),
    }
}

/// The thread's events as the page shows them: a status that changes
/// nothing visible (the same state, title, checklist, links, escalation
/// and takeover as the status before it) is left out.
fn thread_events(events: &[Event], me: Option<&Address>, locale: Locale) -> Vec<ThreadEvent> {
    let mut shown = Vec::with_capacity(events.len());
    let mut task = Task::default();
    for e in events {
        if e.kind == senate_core::Kind::Status {
            let Some(payload) = senate_core::StatusPayload::of(e) else {
                // No status to read: nothing to show (never a bare "status").
                continue;
            };
            let next = task.after(&payload);
            if next == task {
                continue;
            }
            let mut event = thread_event(e, me, locale);
            let mut line = next.line(&payload, locale);
            if payload.state == senate_core::StatusState::Archived
                && e.meta.contains_key(senate_core::meta::HOSTED_APPROVAL)
                && let Some(reason) = payload.reason.as_deref()
            {
                line.line = reason.to_owned();
            }
            event.status = Some(line);
            task = next;
            shown.push(event);
        } else {
            shown.push(thread_event(e, me, locale));
        }
    }
    shown
}

/// What a thread's statuses have said so far: each field as the last
/// status that set it left it (a status's missing field is no change).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Task {
    state: Option<senate_core::StatusState>,
    title: Option<String>,
    steps: Vec<(String, bool)>,
    links: Vec<(String, String)>,
    escalated_to: Option<String>,
    takeover: Option<String>,
}

impl Task {
    fn after(&self, s: &senate_core::StatusPayload) -> Self {
        Self {
            state: Some(s.state),
            title: s.title.clone().or_else(|| self.title.clone()),
            steps: s.steps.as_ref().map_or_else(
                || self.steps.clone(),
                |steps| steps.iter().map(|x| (x.label.clone(), x.done)).collect(),
            ),
            links: s.links.as_ref().map_or_else(
                || self.links.clone(),
                |links| {
                    links
                        .iter()
                        .map(|x| (x.label.clone(), x.url.clone()))
                        .collect()
                },
            ),
            escalated_to: if s.withdraw {
                None
            } else {
                s.escalated_to
                    .as_ref()
                    .map(ToString::to_string)
                    .or_else(|| self.escalated_to.clone())
            },
            takeover: s
                .takeover
                .as_ref()
                .map(ToString::to_string)
                .or_else(|| self.takeover.clone()),
        }
    }

    /// This status, as its line: the state in words, then the title; the
    /// checklist and links as they now stand.
    fn line(&self, s: &senate_core::StatusPayload, locale: Locale) -> StatusLine {
        use senate_core::StatusState as S;
        let words = match (s.state, s.takeover.as_ref(), s.escalated_to.as_ref()) {
            (_, Some(who), _) => {
                locale.pick(format!("由 {who} 接手"), format!("taken over by {who}"))
            }
            (S::Suspended, None, Some(who)) => {
                locale.pick(format!("等待 {who}"), format!("waiting on {who}"))
            }
            (S::Suspended, None, None) => locale.pick("已暂停", "suspended").to_string(),
            (S::Processing, ..) => locale.pick("处理中", "working on it").to_string(),
            (S::Active, ..) => locale.pick("进行中", "active").to_string(),
            (S::Archived, ..) => locale.pick("已归档", "archived").to_string(),
        };
        let line = match self.title.as_deref().filter(|t| !t.trim().is_empty()) {
            Some(title) => format!("{words} — {title}"),
            None => words,
        };
        let reason = s
            .reason
            .as_deref()
            .filter(|r| !r.trim().is_empty() && s.takeover.is_none())
            .filter(|_| matches!(s.state, S::Suspended))
            .map(str::to_owned);
        StatusLine {
            state: s.state.as_str().to_string(),
            line,
            reason,
            steps: self
                .steps
                .iter()
                .map(|(label, done)| StatusStep {
                    label: label.clone(),
                    done: *done,
                })
                .collect(),
            links: self
                .links
                .iter()
                .map(|(label, url)| StatusLink {
                    label: label.clone(),
                    url: url.clone(),
                })
                .collect(),
        }
    }
}

fn thread_event(e: &Event, me: Option<&Address>, locale: Locale) -> ThreadEvent {
    let carried = e.carried();
    ThreadEvent {
        seq: e.seq.0,
        kind: e.kind.as_str().to_string(),
        kind_label: text::kind(locale, &e.kind),
        by: carried.as_ref().map_or_else(
            || e.byline(locale).unwrap_or_else(|| e.from.in_text()),
            |c| c.author.clone(),
        ),
        via: carried.map(|c| c.line(locale)),
        to: e.to.iter().map(Address::in_text).collect(),
        body: e.body.clone(),
        at: e.ts.into(),
        mine: me.is_some_and(|m| *m == e.from && e.on_behalf_of.is_none()),
        status: None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::Utc;
    use senate_core::{CommitteeId, EventId, Kind, SenatorName, Seq};

    fn event(seq: u64, kind: Kind, from: Address, body: &str) -> Event {
        Event {
            seq: Seq(seq),
            committee: CommitteeId::new("refund-v2").unwrap(),
            id: EventId::new(),
            ts: Utc::now(),
            kind,
            from,
            to: Vec::new(),
            thread: (seq != 1).then_some(Seq(1)),
            on_behalf_of: None,
            source: None,
            body: body.into(),
            refs: None,
            mirror: None,
            meta: Default::default(),
        }
    }

    #[test]
    fn a_thread_is_its_events_with_bylines_and_the_persons_own_marked() {
        let alice = Name::new("alice").unwrap();
        let claude: SenatorName = "claude".parse().unwrap();
        let reply = ThreadReply {
            committee: CommitteeId::new("refund-v2").unwrap(),
            root: Seq(1),
            events: vec![
                event(1, Kind::Question, Address::senator(&claude), "which env?"),
                event(2, Kind::Answer, Address::senior(&alice), "staging"),
            ],
        };
        let view = thread_view(reply, Some(&alice), Locale::En);
        assert_eq!(view.root, 1);
        assert_eq!(view.events.len(), 2);
        assert_eq!(view.events[0].by, "[senator::claude]");
        assert_eq!(view.events[0].kind_label, "question");
        assert!(!view.events[0].mine);
        assert!(view.events[1].mine);
        assert_eq!(view.events[1].body, "staging");

        let zh = thread_view(
            ThreadReply {
                committee: CommitteeId::new("refund-v2").unwrap(),
                root: Seq(1),
                events: vec![event(1, Kind::Question, Address::senator(&claude), "?")],
            },
            None,
            Locale::Zh,
        );
        assert_eq!(zh.events[0].kind_label, "问题");
    }

    fn status(seq: u64, from: Address, payload: &senate_core::StatusPayload) -> Event {
        let mut e = event(seq, Kind::Status, from, "");
        e.meta
            .insert(senate_core::StatusPayload::META_KEY.into(), payload.value());
        e
    }

    #[test]
    fn an_archived_approval_shows_why_the_wait_ended() {
        use senate_core::{StatusPayload, StatusState, meta};
        let by = Address::senator(&"claude".parse().unwrap());
        let question = event(1, Kind::Question, by.clone(), "waiting for permission");
        let reason = "The terminal prompt went away unanswered.";
        let mut payload = StatusPayload::new(StatusState::Archived);
        payload.reason = Some(reason.into());
        let mut archive = status(2, by, &payload);
        archive
            .meta
            .insert(meta::HOSTED_APPROVAL.into(), serde_json::json!(question.id));
        let events = thread_events(&[question, archive], None, Locale::En);
        let line = events[1].status.as_ref().unwrap();
        assert_eq!(line.state, "archived");
        assert_eq!(line.line, reason);
    }

    /// `senate#145`–`#147` as stored: a request the desk forwarded from
    /// commons (`from` the Speaker, wendell at the desk), the chair's
    /// escalation to wendell with a long Markdown reason, wendell's reply.
    fn forwarded_thread() -> Vec<Event> {
        use senate_core::{Origin, StatusPayload, StatusState, meta};
        let mut root = event(
            1,
            Kind::Request,
            Address::Speaker,
            "Please merge `feat/phase1-parent-memory` and publish a release.",
        );
        root.committee = CommitteeId::new("senate").unwrap();
        root.to = vec![Address::senator(&"agy-68aa".parse().unwrap())];
        let origin = Origin {
            locator: "/senate/dark-conclave::commons::senator::agy-442d"
                .parse()
                .unwrap(),
            thread: Seq(109),
        };
        root.meta.insert(meta::ORIGIN.into(), origin.value());
        root.meta
            .insert(meta::ACTED_BY.into(), serde_json::json!("wendell"));
        root.meta
            .insert(meta::DESK_PERSON.into(), serde_json::json!("wendell"));
        let mut esc = StatusPayload::new(StatusState::Suspended);
        esc.escalated_to = Some(Name::new("wendell").unwrap());
        esc.reason = Some(ESCALATION.into());
        let mut status = status(2, Address::senator(&"agy-68aa".parse().unwrap()), &esc);
        status.committee = root.committee.clone();
        let mut reply = event(
            3,
            Kind::Answer,
            Address::senior(&Name::new("wendell").unwrap()),
            "1. 0.14.3\n2. yes, main",
        );
        reply.committee = root.committee.clone();
        reply.to = vec![Address::Speaker];
        vec![root, status, reply]
    }

    const ESCALATION: &str = "Thread 145 asks to merge and publish.\n\n\
        Current state:\n- `main` is at `7af3c689`\n- **clean** tree\n\n\
        1. Version: `0.14.3`?\n2. Scope: release `main`?\n\
        <script>alert(1)</script> [docs](javascript:alert(1))";

    #[test]
    fn a_forwarded_request_is_its_askers_with_the_desk_that_carried_it() {
        let wendell = Name::new("wendell").unwrap();
        let events = forwarded_thread();
        let view = thread_view(
            ThreadReply {
                committee: CommitteeId::new("senate").unwrap(),
                root: Seq(1),
                events: events.clone(),
            },
            Some(&wendell),
            Locale::En,
        );
        let root = &view.events[0];
        assert_eq!(
            root.by, "[/senate/dark-conclave::commons::senator::agy-442d]",
            "the asker on commons, not the Speaker"
        );
        assert_eq!(
            root.via.as_deref(),
            Some("forwarded from commons #109 by the Speaker's desk ([senator::wendell])")
        );
        assert!(!root.mine, "wendell forwarded it; he did not write it");
        assert_eq!(root.body, events[0].body);
        // wendell's own reply is his, and nobody else's words.
        let reply = &view.events[2];
        assert!(reply.mine);
        assert_eq!(reply.via, None);
        assert_eq!(reply.by, "[senator::wendell]");
        // Nothing was rewritten: the stored root is still the office's.
        assert_eq!(events[0].from, Address::Speaker);

        let zh = thread_view(
            ThreadReply {
                committee: CommitteeId::new("senate").unwrap(),
                root: Seq(1),
                events,
            },
            Some(&wendell),
            Locale::Zh,
        );
        assert_eq!(
            zh.events[0].via.as_deref(),
            Some("由议长办公桌（[senator::wendell]）转自 commons #109")
        );
    }

    #[test]
    fn a_secretary_forwarding_with_a_persons_approval_names_both() {
        use senate_core::meta;
        let mut root = forwarded_thread().remove(0);
        root.meta
            .insert(meta::ACTED_BY.into(), serde_json::json!("claude"));
        let line = thread_event(&root, None, Locale::En).via.unwrap();
        assert_eq!(
            line,
            "forwarded from commons #109 by the Speaker's desk ([senator::claude], approved by \
             [senator::wendell])"
        );
    }

    #[test]
    fn a_relayed_answer_is_its_answerers_relayed_by_the_desk() {
        use senate_core::meta;
        // commons#111: the reply, back in the asking thread.
        let mut relay = event(2, Kind::Answer, Address::Speaker, "1. 0.14.3");
        relay.meta.insert(
            meta::ANSWERED_BY.into(),
            serde_json::json!("/senate/dark-conclave::senate::senator::wendell"),
        );
        relay
            .meta
            .insert(meta::ACTED_BY.into(), serde_json::json!("wendell"));
        let shown = thread_event(&relay, None, Locale::En);
        assert_eq!(
            shown.by,
            "[/senate/dark-conclave::senate::senator::wendell]"
        );
        assert_eq!(
            shown.via.as_deref(),
            Some("relayed back by the Speaker's desk ([senator::wendell])")
        );
    }

    #[test]
    fn only_the_office_carries_anothers_words() {
        use senate_core::meta;
        // An agent's event with the office's keys is shown as the agent's:
        // nobody is made to look like someone else.
        let claude = Address::senator(&"claude".parse().unwrap());
        let mut forged = forwarded_thread().remove(0);
        forged.from = claude.clone();
        let mut relay = event(2, Kind::Answer, claude, "yes");
        relay.meta.insert(
            meta::ANSWERED_BY.into(),
            serde_json::json!("/senate::senate::senator::wendell"),
        );
        for e in [forged, relay] {
            let shown = thread_event(&e, None, Locale::En);
            assert_eq!(shown.by, "[senator::claude]");
            assert_eq!(shown.via, None);
        }
        // The office's own words (a note, no origin) are the office's.
        let note = event(2, Kind::Note, Address::Speaker, "forwarded");
        let shown = thread_event(&note, None, Locale::En);
        assert_eq!(shown.by, "[speaker]");
        assert_eq!(shown.via, None);
    }

    #[test]
    fn an_escalations_reason_is_kept_whole_under_a_compact_line() {
        let events = thread_events(&forwarded_thread(), None, Locale::En);
        let line = events[1].status.as_ref().unwrap();
        assert_eq!(line.state, "suspended");
        assert_eq!(line.line, "waiting on wendell", "the chip is one line");
        assert!(!line.line.contains('\n'));
        assert_eq!(
            line.reason.as_deref(),
            Some(ESCALATION),
            "newlines, lists, code and bold as written"
        );
        let zh = thread_events(&forwarded_thread(), None, Locale::Zh);
        assert_eq!(zh[1].status.as_ref().unwrap().line, "等待 wendell");
        // Its Markdown is the message renderer's: structure kept, raw
        // HTML as text, the unsafe link not a link.
        use senate_markdown::{Color, Document, Format, RenderCache, Terminal};
        let doc = Document::parse(ESCALATION);
        let mut cache = RenderCache::new(Terminal::new(80, Color::None));
        cache.render_document(&doc, Format::Html, &mut String::new());
        let mut html = String::new();
        for entry in doc.blocks() {
            assert!(cache.write_block(entry.id, Format::Html, &mut html));
        }
        assert!(html.contains("<li>"), "{html}");
        assert!(html.contains("<strong>clean</strong>"), "{html}");
        assert!(html.contains("<code>7af3c689</code>"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
        assert!(!html.contains("href=\"javascript:"), "{html}");
    }

    #[test]
    fn a_progress_status_has_no_reason_block() {
        use senate_core::{StatusPayload, StatusState};
        let by = Address::senator(&"claude".parse().unwrap());
        let mut p = StatusPayload::new(StatusState::Processing);
        p.title = Some("Release".into());
        p.reason = Some("ignored".into());
        let events = thread_events(
            &[event(1, Kind::Request, by.clone(), "go"), status(2, by, &p)],
            None,
            Locale::En,
        );
        let line = events[1].status.as_ref().unwrap();
        assert_eq!(line.line, "working on it — Release");
        assert_eq!(line.reason, None);
    }

    /// Thread 1565's shape: a request, then statuses from the agent working
    /// it, one of which repeats the one before.
    #[test]
    fn statuses_read_as_activity_lines_and_a_repeat_is_left_out() {
        use senate_core::{Link, StatusPayload, StatusState, Step};
        let agent: SenatorName = "announceseats".parse().unwrap();
        let alice = Name::new("alice").unwrap();
        let mut first = StatusPayload::new(StatusState::Processing);
        first.title = Some("Wait for startup seat reports".into());
        let same = StatusPayload::new(StatusState::Processing);
        let mut next = StatusPayload::new(StatusState::Processing);
        next.title = Some("Cover the uplink reconnect cycle".into());
        next.steps = Some(vec![
            Step {
                id: "a".into(),
                label: "Reproduce".into(),
                done: true,
            },
            Step {
                id: "b".into(),
                label: "Fix".into(),
                done: false,
            },
        ]);
        next.links = Some(vec![Link {
            label: "PR".into(),
            url: "https://example.test/pr/1".into(),
        }]);
        let mut waiting = StatusPayload::new(StatusState::Suspended);
        waiting.escalated_to = Some(alice.clone());
        waiting.reason = Some("which uplink to keep?".into());
        let mut taken = StatusPayload::new(StatusState::Active);
        taken.takeover = Some(alice.clone());
        let by = Address::senator(&agent);
        let reply = ThreadReply {
            committee: CommitteeId::new("refund-v2").unwrap(),
            root: Seq(1),
            events: vec![
                event(
                    1,
                    Kind::Request,
                    Address::senior(&alice),
                    "announce the seats",
                ),
                status(2, by.clone(), &first),
                status(3, by.clone(), &same),
                status(4, by.clone(), &next),
                status(5, by.clone(), &waiting),
                status(6, Address::senior(&alice), &taken),
            ],
        };
        let view = thread_view(reply, Some(&alice), Locale::En);
        let seqs: Vec<u64> = view.events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, [1, 2, 4, 5, 6], "the repeat (3) is left out");
        let line = |seq: u64| {
            view.events
                .iter()
                .find(|e| e.seq == seq)
                .unwrap()
                .status
                .clone()
                .unwrap()
        };
        assert_eq!(
            line(2).line,
            "working on it — Wait for startup seat reports"
        );
        assert!(line(2).steps.is_empty() && line(2).links.is_empty());
        assert_eq!(
            line(4).line,
            "working on it — Cover the uplink reconnect cycle"
        );
        assert_eq!(
            line(4).steps,
            [
                StatusStep {
                    label: "Reproduce".into(),
                    done: true
                },
                StatusStep {
                    label: "Fix".into(),
                    done: false
                },
            ]
        );
        assert_eq!(line(4).links[0].url, "https://example.test/pr/1");
        assert_eq!(line(5).state, "suspended");
        assert_eq!(
            line(5).line,
            "waiting on alice — Cover the uplink reconnect cycle"
        );
        assert_eq!(line(5).reason.as_deref(), Some("which uplink to keep?"));
        assert_eq!(line(6).reason, None, "a takeover is no reason to show");
        assert_eq!(
            line(6).line,
            "taken over by alice — Cover the uplink reconnect cycle"
        );
        assert!(view.events[0].status.is_none(), "a message is not a status");

        let zh = thread_view(
            ThreadReply {
                committee: CommitteeId::new("refund-v2").unwrap(),
                root: Seq(1),
                events: vec![
                    event(1, Kind::Request, Address::senior(&alice), "x"),
                    status(2, by, &first),
                ],
            },
            Some(&alice),
            Locale::Zh,
        );
        assert_eq!(
            zh.events[1].status.as_ref().unwrap().line,
            "处理中 — Wait for startup seat reports"
        );
    }

    /// A status event without a readable payload is not shown at all.
    #[test]
    fn a_status_without_a_payload_is_not_shown() {
        let agent: SenatorName = "announceseats".parse().unwrap();
        let reply = ThreadReply {
            committee: CommitteeId::new("refund-v2").unwrap(),
            root: Seq(1),
            events: vec![
                event(1, Kind::Request, Address::senator(&agent), "go"),
                event(2, Kind::Status, Address::senator(&agent), ""),
            ],
        };
        let view = thread_view(reply, None, Locale::En);
        assert_eq!(view.events.len(), 1);
    }
}
