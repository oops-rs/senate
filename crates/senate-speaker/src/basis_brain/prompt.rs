//! The Speaker's voice and the shape of its answers.
//!
//! Every prompt is written once, in English, and asks for what people read
//! in the committee's language (the input's [`Locale`]). Examples of what
//! people say are given in both languages, because people type either.
//! Identifiers (handles, agent names, committee ids, seq, tool names) are
//! never translated.
//!
//! The schemas are hand-written because their field descriptions are
//! prompts: they are what the model reads to decide what belongs in each
//! field.

use std::time::Duration;

use basis::OutputSpec;
use chrono::{DateTime, Utc};
use senate_core::{
    Address, Brief, CommitteeId, Event, Locale, OutputLevel, Roster, SenatorName, Seq,
    TriagePayload,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::brain::{
    AgentCandidate, AgentTurn, Agreement, AgreementInput, Answer, CommitteeSeat, Conversation,
    Crossing, CrossingInput, DigestInput, DigestOut, Intent, MemoryDraft, Offline, OfflineInput,
    Pick, PickInput, Question, RelayKind, Takeover, TriageInput, Utterance, apply_threshold,
};

/// How much of one event body a prompt or tool result shows.
pub(crate) const EVENT_BODY_CHARS: usize = 600;

/// The language people read, as the prompts name it.
pub(crate) fn language(locale: Locale) -> &'static str {
    locale.pick("Chinese", "English")
}

/// The persona's instruction on the language of everything people read.
pub(crate) fn write_in(locale: Locale) -> String {
    format!("Write everything people will read in {}.", language(locale))
}

/// Who a target is, when the model writes one: shared by the persona and
/// the schema's target fields.
macro_rules! target_rule {
    () => {
        "A target is one of two things. (1) A person's handle from the roster (e.g. \"alice\"), \
         when the speaker means that person's agent without saying which one (\"tell alice's \
         agent …\", 「告诉 alice 的 agent …」, or a name the person goes by, e.g. 「爱丽丝的 \
         agent」): the system settles which of their agents gets it (their only agent; the person \
         themselves when they have none; when they have several, the one the speaker named, else \
         the system picks by the task or asks). (2) One agent, written by its own name (e.g. \
         \"codex\"), only when the speaker named that agent (\"alice's codex\", 「alice 的 \
         codex」, \"codex\"). Never choose one of a person's agents yourself when the speaker \
         did not name it, and never guess a person who is not named. The one exception is a \
         question or change request that names nobody, relayed by duty (see the rule on \
         duties): its target is that seat's relay target exactly as the seats on this committee \
         list it (e.g. \
         \"codex\" or \"north-desk::responder\")."
    };
}

/// When an unaddressed question goes to a seat by its duty: shared by the
/// persona of both modes.
macro_rules! duty_rule {
    () => {
        "- Duties: a question or a change request that names no person, agent or seat to take \
         it (e.g. \"@Speaker how is the refund service deployed?\", 「@Speaker 退款服务是怎么部署的？」, \
         「@Speaker 请在 refund 仓库的 deploy/README.md 末尾加一节，开分支、本地提交」) may \
         belong to a seat on this committee by what it does there. Read the duty of every seat \
         listed under the seats on this committee (here and in child Senates). When its subject \
         falls within exactly one seat's duty, relay it to that seat: action relay, kind \
         question for a question, kind request for a change or a task to carry out, one target \
         written exactly as that seat's relay target is listed, the body as said. Do this even \
         when the seat is away: the system answers for it or holds the message. When it falls \
         within several seats' duties, or none, do not relay by duty: answer from the log, or \
         say plainly that you cannot and who might know. Never route by duty when the speaker \
         named someone (a person, an agent or a seat): the target is who they named. Route only \
         questions and change requests by duty, never a decision, a binding or another \
         operation. Duty routing is only for the work or a subject area. A question about this \
         Senate's own state is yours to answer, from the seats, the roster, the brief and the \
         log, and is never relayed by duty, even when a seat's duty names the same word \
         (\"committee\", \"Senate\", \"Speaker\"): who is here, the seats and their duties, \
         presence, status, the brief, decisions, open items, who owns what. A seat in a child \
         Senate sees only its own Senate, so it cannot answer for this one. For example, \
         「@Speaker 现在有哪些 agent，他们的职责是什么」 (\"@Speaker which agents are here and \
         what are their duties?\") is answered by you from the seats on this committee, never \
         relayed to a seat whose duty mentions committees.\n\
         - An earlier relay in the log is not a reason to skip a message sent again when that \
         relay is marked as put to a seat that is gone: that seat is no longer on this \
         committee, nothing will answer there, and the message is routed as if new."
    };
}

/// What the persona says the model cannot do; agent mode replaces it.
const ONLY_THE_LOG: &str =
    "- You cannot run commands, and cannot read or write code or files; you can only read the log.";

fn place(committee: Option<&CommitteeId>) -> String {
    match committee {
        Some(committee) => format!("the chat group of committee \"{committee}\""),
        None => "a chat group that is not bound to any committee yet".to_owned(),
    }
}

fn persona_with(
    committee: Option<&CommitteeId>,
    bot_name: &str,
    locale: Locale,
    reach: &str,
) -> String {
    format!(
        "You are the Speaker of the Senate, stationed in {place}. You are the only member of the \
         group who speaks for the system. People address you in chat as @{bot_name}.\n\
         \n\
         Your duties:\n\
         - Pass what people say on to the right colleague's agent (coding assistant), and when \
         that agent is away, answer for it from the record.\n\
         - On request, record the team's decisions, and recall what has already been settled.\n\
         - The record is the only source of truth: the committee's shared log, every entry \
         numbered by seq. Read it with log_read and log_search.\n\
         \n\
         Who is who:\n\
         - People and agents are senators. A person is written senator::<handle> (e.g. \
         senator::alice); an agent is written senator::<agent>, by its own name (e.g. \
         senator::claude). A person may have several agents, each with its own \
         presence, checkpoints and task threads.\n\
         - {targets}\n\
         \n\
         Rules:\n\
         - {write_in} Be concise.\n\
         - Speak only from the log and the context you are given. Never invent decisions, \
         numbers, times or commitments.\n\
         - When you are not sure, say so plainly, and say what information is missing and whom \
         to ask.\n\
         - When citing the log, give the seq, e.g. (seq 118).\n\
         {duties}\n\
         {reach}\n\
         - When relaying, keep the facts and numbers of the original words exactly: add \
         nothing, drop nothing.\n\
         - Identifiers are never translated: handles, agent names, committee ids, seq, tool \
         names.",
        place = place(committee),
        targets = target_rule!(),
        duties = duty_rule!(),
        write_in = write_in(locale),
    )
}

/// The system prompt of every run but `converse`.
pub(crate) fn persona(committee: Option<&CommitteeId>, bot_name: &str, locale: Locale) -> String {
    persona_with(committee, bot_name, locale, ONLY_THE_LOG)
}

/// `persona` with this Senate's extra instructions after it: they add to
/// its rules, never replace them.
pub(crate) fn with_instructions(persona: String, instructions: Option<&str>) -> String {
    match instructions.map(str::trim).filter(|t| !t.is_empty()) {
        Some(text) => format!(
            "{persona}\n\n\
             Additional instructions from this Senate's configuration. They add to the rules \
             above and never replace them; where they disagree, the rules above win:\n{text}"
        ),
        None => persona,
    }
}

/// One event as a line a model can cite: who wrote it as a bracketed
/// mention (the Speaker answering for an agent as `[speaker::<agent>]`).
pub(crate) fn render_event(event: &Event) -> String {
    let author = event
        .byline(Locale::En)
        .unwrap_or_else(|| match &event.on_behalf_of {
            Some(behalf) => format!("{} (for {})", event.from.in_text(), behalf.in_text()),
            None => event.from.in_text(),
        });
    let mut line = format!(
        "seq {} · {} · {} · {author}",
        event.seq,
        event.ts.format("%Y-%m-%d %H:%M"),
        event.kind,
    );
    if !event.to.is_empty() {
        let to: Vec<String> = event.to.iter().map(Address::in_text).collect();
        line.push_str(&format!(" → {}", to.join(", ")));
    }
    if let Some(thread) = event.thread {
        line.push_str(&format!(" · thread {thread}"));
    }
    let body: String = event.body.chars().take(EVENT_BODY_CHARS).collect();
    let more = if event.body.chars().count() > EVENT_BODY_CHARS {
        "…"
    } else {
        ""
    };
    line.push_str(&format!("\n  {}{more}", body.replace('\n', "\n  ")));
    line
}

fn render_context(events: &[Event]) -> String {
    render_context_gone(events, &[])
}

/// What marks a relay in the context put to a seat that is gone.
const GONE_MARK: &str = "  [put to a seat that is gone: no longer on this committee, nothing will \
     answer there; not a duplicate of a message sent again]";

/// [`render_context`], each relay in `gone` marked as put to a seat that is
/// gone since.
fn render_context_gone(events: &[Event], gone: &[Seq]) -> String {
    if events.is_empty() {
        return "(none)".to_owned();
    }
    events
        .iter()
        .map(|event| {
            let line = render_event(event);
            if gone.contains(&event.seq) {
                format!("{line}\n{GONE_MARK}")
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The roster as people (handle, the names they go by) with their seated
/// agents: `- alice (also called Alice, 爱丽丝): claude, codex`.
pub(crate) fn render_roster(roster: &Roster) -> String {
    if roster.principals().is_empty() {
        return "(the roster is empty)".to_owned();
    }
    roster
        .principals()
        .iter()
        .map(|senior| {
            let names = if senior.names.is_empty() {
                String::new()
            } else {
                format!(" (also called {})", senior.names.join(", "))
            };
            let agents: Vec<String> = roster
                .agents_of(&senior.handle)
                .into_iter()
                .map(ToString::to_string)
                .collect();
            let agents = if agents.is_empty() {
                "no agent seated".to_owned()
            } else {
                agents.join(", ")
            };
            format!("- {}{names}: {agents}", senior.handle)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

const ROSTER_HEADING: &str =
    "People on the roster (handle, other names) and the agents seated under each:";

/// The seats on the committee, here and in child Senates, one per line:
/// the name a relay writes, presence, duty. Shown in the interpret and the
/// converse prompt alike.
pub(crate) fn render_seats(seats: &[CommitteeSeat]) -> String {
    if seats.is_empty() {
        return "(none known)".to_owned();
    }
    seats
        .iter()
        .map(|seat| {
            let duty = seat
                .duty
                .as_deref()
                .map_or_else(|| "none set".to_owned(), |duty| cut(duty, 300));
            let shown = seat.shown.as_deref().map_or_else(String::new, |shown| {
                format!(" · shown to people as {shown}")
            });
            format!(
                "- {}{shown} · {} · duty: {duty}",
                seat.target,
                match (seat.online, seat.paused) {
                    (true, _) => "online",
                    (false, true) =>
                        "paused (expected back; may be answered as paused, to answer when resumed)",
                    (false, false) => "away",
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

const SEATS_HEADING: &str = "Seats on this committee, written as a relay target (an agent of \
     this Senate by its name; a seat in a child Senate as <child>::<senator>). Whenever you name \
     a seat to people, write the name it is shown to people as, never its relay target or id; a \
     relay still names the relay target. Each with presence and duty:";

fn render_binding(committee: Option<&CommitteeId>) -> String {
    match committee {
        Some(committee) => format!("This group is bound to committee {committee}."),
        None => "This group is not bound to any committee yet.".to_owned(),
    }
}

fn group_name(utterance: &Utterance) -> String {
    utterance
        .group
        .group_name
        .clone()
        .unwrap_or_else(|| utterance.group.group_id.clone())
}

pub(crate) fn interpret(utterance: &Utterance) -> String {
    format!(
        "In the group \"{group}\", {speaker} said to you:\n\n{text}\n\n\
         {binding}\n\n\
         {ROSTER_HEADING}\n{roster}\n\n\
         {SEATS_HEADING}\n{seats}\n\n\
         Recent log:\n{context}\n\n\
         Decide what to do, then call the speaker_intent tool exactly once with your conclusion. \
         When you need older records, use log_search / log_read first.",
        group = group_name(utterance),
        speaker = utterance.speaker.in_text(),
        text = utterance.text,
        binding = render_binding(utterance.committee.as_ref()),
        roster = render_roster(&utterance.roster),
        seats = render_seats(&utterance.seats),
        context = render_context_gone(&utterance.context, &utterance.gone_relays),
    )
}

/// Characters of the brief shown when asking for its top.
const TOP_BRIEF_CHARS: usize = 6_000;

pub(crate) fn brief_top(brief: &Brief, locale: Locale) -> String {
    // The current top is what is being rewritten; leave it out.
    let without_top = Brief {
        top: None,
        ..brief.clone()
    };
    format!(
        "Below is the brief of committee {committee}, built from its shared log.\n\n{markdown}\n\n\
         Write the facts that matter most for this committee right now, for every person's agent \
         to read first when a session starts:\n\
         - At most 3 lines, each one short sentence, the most important first.\n\
         - In {language}; keep numbers, names and seq, e.g. (seq 12).\n\
         - From the brief above only: do not look up the log, do not speculate, and add no \
         heading, numbering notes or pleasantries.\n\
         Output just those lines.",
        committee = brief.committee,
        markdown = without_top.to_markdown(locale, TOP_BRIEF_CHARS),
        language = language(locale),
    )
}

/// How the Speaker's answer for `agent` is labelled.
fn answer_label(agent: &SenatorName) -> String {
    format!("[speaker::{agent}]")
}

/// What the committee knows that bears on a question, as a prompt section.
fn known_section(known: &str) -> String {
    if known.is_empty() {
        String::new()
    } else {
        format!("What the committee knows that may bear on it (its memory):\n{known}\n\n")
    }
}

pub(crate) fn answer(question: &Question, known: &str) -> String {
    if let Some(takeover) = &question.takeover {
        return takeover_prompt(question, takeover);
    }
    let agent = Address::senator(&question.agent).in_text();
    format!(
        "{agent} is offline or has not answered in time; you are to answer the question below \
         for it.\n\n\
         The question (seq {seq}, from {from}{behalf}):\n{body}\n\n\
         {known}Recent log:\n{context}\n\n\
         Look for grounds in what the committee knows first (committee_memory), then with \
         log_search / log_read. Answer from these only, citing seq; \
         when the log does not hold the answer, say plainly that you do not know, and that \
         {agent} will see the question when it is back online. \
         Your answer is posted as {label}, the Speaker answering for {agent}; give the answer \
         itself, with no pleasantries.",
        seq = question.event.seq,
        from = question.event.from.in_text(),
        behalf = asked_for(question),
        body = question.event.body,
        context = render_context(&question.context),
        label = answer_label(&question.agent),
        known = known_section(known),
    )
}

fn asked_for(question: &Question) -> String {
    question
        .event
        .on_behalf_of
        .as_ref()
        .map(|b| format!(", for {}", b.in_text()))
        .unwrap_or_default()
}

/// The first line a takeover answer starts with when the material does not
/// cover the question.
pub(crate) const NOT_COVERED: &str = "[NOT_COVERED]";

/// Characters of the brief shown in a takeover.
const TAKEOVER_BRIEF_CHARS: usize = 4_000;

fn file_section(name: &str, text: Option<&str>) -> String {
    match text.map(str::trim).filter(|t| !t.is_empty()) {
        Some(text) => format!("### {name}\n{text}"),
        None => format!("### {name}\n(no such file)"),
    }
}

/// An age in words: `45 minutes`, `3 hours`.
pub(crate) fn age_words(age: Duration) -> String {
    let minutes = age.as_secs() / 60;
    match minutes {
        1 => "1 minute".to_owned(),
        m if m < 60 => format!("{m} minutes"),
        m if m < 120 => "1 hour".to_owned(),
        m => format!("{} hours", m / 60),
    }
}

fn takeover_prompt(question: &Question, takeover: &Takeover) -> String {
    let checkpoint = &takeover.checkpoint;
    let payload = &checkpoint.checkpoint;
    let agent = Address::senator(&question.agent).in_text();
    let files = [
        file_section("PROGRESS.md", payload.files.progress.as_deref()),
        file_section("DECISIONS.md", payload.files.decisions.as_deref()),
        file_section("OPEN.md", payload.files.open.as_deref()),
    ]
    .join("\n\n");
    let brief = takeover.brief.as_ref().map_or_else(
        || "(the brief could not be read)".to_owned(),
        |b| b.to_markdown(question.locale, TAKEOVER_BRIEF_CHARS),
    );
    let (away, paused) = if takeover.paused {
        (
            "is paused: its operator paused its terminal and will resume it, in the same \
             conversation, and it sees this question then",
            format!(
                " Since {agent} is paused, when this material is not enough you may instead \
                 answer that it is paused and will answer when it is resumed, rather than \
                 writing {not_covered}.",
                not_covered = NOT_COVERED
            ),
        )
    } else {
        ("is away (its lease has expired)", String::new())
    };
    format!(
        "{agent} {away}. You have taken over its inbox and are to \
         answer the question below for it.\n\n\
         The question (seq {seq}, from {from}{behalf}):\n{body}\n\n\
         The latest checkpoint of {agent} (seq {cp_seq}, {age} ago, {at}, {pushed}):\n\n{files}\n\n\
         The committee's brief:\n{brief}\n\n\
         Recent log:\n{context}\n\n\
         Answer only from the checkpoint files above, the brief and the log (look the log up \
         with log_search / log_read when you need to), and say what you relied on, e.g. \
         \"per PROGRESS.md of the checkpoint at seq {cp_seq}\". The checkpoint is this agent's \
         own: never answer from another agent's checkpoint or work, not even another agent of \
         the same person. Do not speculate about details of the code, and do not promise \
         anything on its behalf. \
         If this material is not enough to answer, write only {not_covered} on the first line, \
         and on the second line say in one sentence what is missing.{paused} \
         Your answer is posted as {label} takeover, the Speaker answering for {agent} while it \
         is away; give the answer itself, with no pleasantries.",
        seq = question.event.seq,
        from = question.event.from.in_text(),
        behalf = asked_for(question),
        body = question.event.body,
        cp_seq = checkpoint.seq,
        age = age_words(takeover.age),
        at = payload.at(),
        pushed = if payload.pushed {
            "pushed"
        } else {
            "not pushed"
        },
        context = render_context(&question.context),
        not_covered = NOT_COVERED,
        label = answer_label(&question.agent),
    )
}

/// A takeover run's final message as an [`Answer`]: not covered when it
/// starts with [`NOT_COVERED`], whose line is dropped from the text.
pub(crate) fn takeover_answer(text: &str) -> Answer {
    let text = text.trim();
    match text.strip_prefix(NOT_COVERED) {
        Some(rest) => Answer::not_covered(rest.trim()),
        None => Answer::covered(text),
    }
}

/// The answering tool's description; also how a scripted test finds it.
pub(crate) const INTENT_DESCRIPTION: &str = "Report what the Speaker should do about the utterance. Call this exactly once, \
     after any log lookups you need, and then stop.";

/// What each action of the wire means, `none` aside: part of the `action`
/// field's description in the interpret schema and, whole, in the agent
/// schema (where there is no `none`).
macro_rules! work_described {
    () => {
        "work: the speaker wants you to do coding work yourself, starting from a colleague's checkpoint (e.g. \"finish alice's retry path\", 「把 alice 的重试路径做完」, \"add the tests on top of bob's branch\", 「接着 bob 的分支把测试补上」); it is restated for them to confirm, and a PR is opened on their behalf."
    };
}

/// `actions_described!` when this Senate does no coding work itself: a
/// change someone asks for is relayed (by name, or by duty as a request).
const NO_WORK: &str = "There is no work action here: you do no coding work yourself. A change or task someone asks for is relayed to an agent: to the one they named, or by duty (kind request).";

macro_rules! actions_described {
    () => {
        concat!("forget: remove a point from discussion memory (e.g. 'forget that point about retries', 「把关于重试那条记忆删掉」). correct: amend a remembered point (e.g. 'actually the retry limit is five, not three', 「更正一下，重试上限是五次」). For either action use memory_seq only when the person explicitly identifies the point by seq. Otherwise give the topic of the existing point as memory_query, even if a lookup suggests a seq; the system resolves it and asks when several points match. A correction needs the complete replacement statement in body. ",
        "relay: the speaker wants something passed to one or more colleagues' agents (e.g. \"tell alice's agent …\", 「告诉 alice 的 agent …」, \"ask bob's codex …\", 「问一下 bob 的 agent …」). decide: the speaker wants a decision recorded for the committee (e.g. \"note this: refunds are capped at 30 days\", 「记一下：退款上限 30 天」, \"remember, the idempotency key is always Idempotency-Key\", 「记住，幂等键统一用 Idempotency-Key」, \"decided: …\", 「定了：…」); it is restated for them to confirm. recall: the speaker asks what has been decided or what you remember about the committee (e.g. \"what do you remember\", 「你记得什么」, \"what's settled so far\", 「现在定了哪些」, \"what did we decide before\", 「之前决定了什么」); the brief is sent. bind: the speaker wants this group bound to a committee. unbind: the speaker wants this group's binding removed. status: the speaker asks what this group is bound to or which agents are online. adopt_agent_group: the speaker wants this group to be a committee's Agent group, where agents' traffic and task cards go (e.g. \"make this group refund-v2's agent group\", 「把这个群设为 refund-v2 的 agent 群」, \"this is the agent group\", 「这个群是 agent 群」). invite_to_agent_group: the speaker wants people pulled into the Agent group (e.g. \"add @alice to the agent group\", 「把 @alice 拉进 agent 群」). set_output_level: the speaker wants the Agent group to get less or more traffic (e.g. \"agent group: answers only\" / 「agent 群只回答案」 is answers, \"send status to the agent group\" / 「agent 群发状态」 is status, \"everything, every round trip\" / 「全部往返」 is all). ", work_described!(), " reassign: the speaker wants one colleague's work on this committee handed to another (e.g. \"hand alice's work on refund-v2 to bob\", 「把 alice 在 refund-v2 的工作交给 bob」), or handed back (e.g. \"give refund-v2 back to alice\", 「把 refund-v2 交还给 alice」: to is alice, from is whoever has it now if said, else alice too).")
    };
}

/// The interpret schema's `action` description: every action, `none` last.
const INTENT_ACTION_DESCRIPTION: &str = concat!(
    actions_described!(),
    " none: anything else — a question you can answer from the log, small talk, or something you cannot do; put your answer in reply."
);

/// The relay targets' description.
const TARGETS_DESCRIPTION: &str = concat!(
    "relay and invite_to_agent_group. relay: who receives it, one entry per person or agent named. ",
    target_rule!(),
    " invite_to_agent_group: who is invited: people's handles from the roster (e.g. \"alice\"), one per person named; never an agent. Never guess a person who is not named."
);

/// The actions' description: with the `work` action only when this
/// Senate does coding work itself.
fn actions_for(work: bool, described: &str) -> String {
    if work {
        described.to_owned()
    } else {
        described.replace(work_described!(), NO_WORK)
    }
}

/// `work` is offered only when coding work is configured here: without it
/// the action, and its `target` and `instruction`, are not in the schema.
pub(crate) fn intent_spec(work: bool) -> OutputSpec {
    OutputSpec::new("speaker_intent", INTENT_DESCRIPTION, intent_schema(work)).with_tools()
}

fn intent_schema(work: bool) -> Value {
    let mut schema = full_intent_schema();
    let description = actions_for(work, INTENT_ACTION_DESCRIPTION);
    schema["properties"]["action"]["description"] = Value::String(description);
    if !work {
        let properties = schema["properties"]
            .as_object_mut()
            .expect("the intent schema has properties");
        properties.remove("target");
        properties.remove("instruction");
        if let Some(actions) = properties["action"]["enum"].as_array_mut() {
            actions.retain(|a| a.as_str() != Some("work"));
        }
    }
    schema
}

fn full_intent_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["relay", "decide", "forget", "correct", "recall", "bind", "unbind", "status", "adopt_agent_group", "invite_to_agent_group", "set_output_level", "work", "reassign", "none"],
                "description": INTENT_ACTION_DESCRIPTION
            },
            "targets": {
                "type": "array",
                "items": { "type": "string" },
                "description": TARGETS_DESCRIPTION
            },
            "kind": {
                "type": "string",
                "enum": ["question", "request", "note"],
                "description": "relay only. request when the speaker wants the agent to carry out a change or a task (e.g. 「在 deploy/vm/README.md 末尾加一节，开分支、本地提交」, \"fix the retry path\"); the agent reports back in the thread. question whenever the speaker expects the agent to answer or report back, including a statement followed by a question, even an implicit or casual one (e.g. \"the refund cap is 30 days now, have you changed yours?\", 「退款上限改成 30 天了，你那边改了吗」, \"…, let me know when it's done\", 「…，弄好了说一声」); an unanswered question is answered on the agent's behalf. note only when nothing is asked: information or an instruction for the agent to take into account."
            },
            "body": {
                "type": "string",
                "description": "relay, decide and correct. correct: the complete corrected point as stated. relay: the message for the agent, as a self-contained sentence in the speaker's language; drop the \"tell X's agent\" / 「告诉 X 的 agent」 framing. decide: the decision itself as one self-contained statement in the speaker's language; drop the \"note this\" / 「记一下」 framing. Either way keep every fact, number and name exactly as said, and add nothing."
            },
            "memory_seq": {
                "type": "integer",
                "description": "forget or correct only. The positive seq the person explicitly named."
            },
            "memory_query": {
                "type": "string",
                "description": "forget or correct only. Search words identifying the current point, in its language. Omit when memory_seq is given."
            },
            "supersedes": {
                "type": "integer",
                "description": "decide only. The seq of an earlier decision in the log that this one changes or replaces (look it up with log_search when the speaker says something like \"change it to …\" / 「改成…」 or \"no longer …\" / 「不再…」). Omit when nothing is replaced or you are not sure which."
            },
            "committee": {
                "type": "string",
                "description": "bind and adopt_agent_group. The committee id exactly as the speaker gave it, e.g. \"refund-v2\" (lowercase letters, digits and -). For adopt_agent_group omit it when the speaker names none."
            },
            "target": {
                "type": "string",
                "description": "work only. Whose checkpoint to start from: a person's handle from the roster (e.g. \"alice\"), and the system settles which of their agents as it does for relay targets, or one agent by its own name (e.g. \"codex\") when the speaker named that agent. The work starts from that one agent's own latest checkpoint."
            },
            "instruction": {
                "type": "string",
                "description": "work only. What to do, as a self-contained instruction in the speaker's language; drop the \"finish alice's …\" / 「把 alice 的」 framing but keep every detail exactly as said."
            },
            "from": {
                "type": "string",
                "description": "reassign only. Whose work is handed over: a person's handle from the roster, or one agent by its own name when the speaker named that agent."
            },
            "to": {
                "type": "string",
                "description": "reassign only. Who takes the work (the original owner when it is handed back): a person's handle from the roster, or one agent by its own name when the speaker named that agent."
            },
            "level": {
                "type": "string",
                "enum": ["answers", "status", "all"],
                "description": "set_output_level only. answers: only answers; status: answers and card updates (the default); all: every step and message too."
            },
            "reply": {
                "type": "string",
                "description": "none only. What to say back in the group: concise, in the committee's language (the system prompt names it), citing seq numbers for anything taken from the log, and saying plainly when you are not sure. Empty means say nothing."
            }
        },
        "required": ["action"],
        "additionalProperties": false
    })
}

fn null_as_empty<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(Option::<Vec<String>>::deserialize(d)?.unwrap_or_default())
}

/// The answering tool's payload, before validation into an [`Intent`].
#[derive(Debug, Deserialize)]
pub(crate) struct IntentWire {
    action: String,
    /// Absent (interpret) or null (the agent wire's strict schema): none.
    #[serde(default, deserialize_with = "null_as_empty")]
    targets: Vec<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    supersedes: Option<u64>,
    memory_seq: Option<u64>,
    memory_query: Option<String>,
    #[serde(default)]
    committee: Option<String>,
    #[serde(default)]
    level: Option<OutputLevel>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    instruction: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    reply: Option<String>,
}

/// The relay kinds the schema offers.
const RELAY_KINDS: [&str; 3] = ["question", "request", "note"];

fn filled(field: &Option<String>) -> bool {
    field.as_deref().is_some_and(|f| !f.trim().is_empty())
}

impl IntentWire {
    /// The rules the schema states, checked before the answering tool may
    /// end the run: a broken answer goes back to the model as a tool error
    /// it can correct. `work` is accepted only when coding work is
    /// configured here (`work`); the schema does not offer it otherwise.
    pub(crate) fn validate(&self, work: bool) -> Result<(), String> {
        let needs = |ok: bool, what: &str| {
            if ok {
                Ok(())
            } else {
                Err(format!("{} needs {what}.", self.action))
            }
        };
        match self.action.as_str() {
            "work" if !work => {
                Err("work is not available: no coding work is configured here.".into())
            }
            "relay" => {
                needs(
                    !self.targets.is_empty() && filled(&self.body),
                    "targets and a non-empty body",
                )?;
                match self.kind.as_deref() {
                    Some(kind) if !RELAY_KINDS.contains(&kind) => Err(format!(
                        "kind {kind:?} is not one of question, request, note."
                    )),
                    _ => Ok(()),
                }
            }
            "forget" | "correct" => {
                needs(
                    matches!(
                        (self.memory_seq, self.memory_query.as_deref()),
                        (Some(1..), None)
                    ) || (self.memory_seq.is_none()
                        && self
                            .memory_query
                            .as_deref()
                            .is_some_and(|q| !senate_core::memory::terms(q).is_empty())),
                    "either a positive memory_seq or searchable memory_query",
                )?;
                if self.action == "correct" {
                    needs(filled(&self.body), "a non-empty replacement body")
                } else {
                    needs(!filled(&self.body), "no replacement body")
                }
            }
            "decide" => needs(filled(&self.body), "a non-empty body"),
            "bind" => needs(filled(&self.committee), "a committee"),
            "invite_to_agent_group" => needs(!self.targets.is_empty(), "targets"),
            "set_output_level" => needs(self.level.is_some(), "a level"),
            "work" => needs(
                filled(&self.target) && filled(&self.instruction),
                "a target and an instruction",
            ),
            "reassign" => needs(filled(&self.to), "to"),
            "recall" | "unbind" | "adopt_agent_group" | "status" | "none" => Ok(()),
            other => Err(format!(
                "action {other:?} is not one of the actions the schema lists."
            )),
        }
    }

    /// As an [`Intent`]; only for a wire [`validate`](Self::validate)
    /// accepted.
    pub(crate) fn into_intent(self) -> Intent {
        let text = |field: Option<String>| field.unwrap_or_default();
        match self.action.as_str() {
            "relay" => Intent::Relay {
                targets: self.targets,
                kind: match self.kind.as_deref() {
                    Some("question") => RelayKind::Question,
                    Some("request") => RelayKind::Request,
                    _ => RelayKind::Note,
                },
                body: text(self.body),
            },
            "decide" => Intent::Decide {
                body: text(self.body),
                supersedes: self.supersedes.map(Seq),
            },
            "forget" | "correct" => {
                let replacement = (self.action == "correct").then(|| text(self.body));
                match (self.memory_seq.map(Seq), replacement) {
                    (Some(target), Some(text)) => Intent::Correct { target, text },
                    (Some(target), None) => Intent::Forget {
                        targets: vec![target],
                    },
                    (None, replacement) => Intent::EditMemoryByQuery {
                        query: text(self.memory_query),
                        replacement,
                    },
                }
            }
            "recall" => Intent::Recall,
            "bind" => Intent::Bind {
                committee: text(self.committee),
            },
            "unbind" => Intent::Unbind,
            "adopt_agent_group" => Intent::AdoptAgentGroup {
                committee: self.committee.filter(|f| !f.trim().is_empty()),
            },
            "invite_to_agent_group" => Intent::InviteToAgentGroup { who: self.targets },
            "set_output_level" => Intent::SetOutputLevel {
                level: self.level.unwrap_or_default(),
            },
            "work" => Intent::Work {
                target: text(self.target),
                instruction: text(self.instruction),
            },
            "reassign" => {
                let to = text(self.to);
                let from = self
                    .from
                    .filter(|f| !f.trim().is_empty())
                    .unwrap_or_else(|| to.clone());
                Intent::Reassign { from, to }
            }
            "status" => Intent::Status,
            _ => Intent::None {
                reply: text(self.reply),
            },
        }
    }
}

// ---- Agent mode -------------------------------------------------------

/// The tools an agent-mode run may call, as the persona names them.
const AGENT_TOOLS_SAID: &str = "committee_memory (what the committee knows, first), log_read / \
     log_search (the log), committee_brief (the brief: \
     decisions, open questions, checkpoints, presence), committee_status (the bound groups, which \
     agents are online), presence (each agent's presence), tasks (task threads), checkpoint (one \
     agent's latest checkpoint)";

/// The system prompt of a `converse` run: the persona, plus what agent mode
/// adds. It replaces the workspace's persona for that run only.
pub(crate) fn agent_persona(
    committee: Option<&CommitteeId>,
    bot_name: &str,
    locale: Locale,
    max_actions: usize,
) -> String {
    let base = persona_with(
        committee,
        bot_name,
        locale,
        &format!(
            "- You cannot run commands, and cannot read or write code or files. You can look \
             things up with read-only tools: {AGENT_TOOLS_SAID}."
        ),
    );
    format!(
        "{base}\n\
         \n\
         This is conversation mode: people in the same thread can keep talking to you, and you \
         can see the earlier exchanges.\n\
         - One message may ask you to do several things: list them in actions in the order they \
         were said, one entry each; at most {max_actions}. If there are more, list none of them, \
         and ask the speaker in reply to split them up (this is a refusal, not a question: \
         awaiting_reply is false).\n\
         - You only understand and plan; the system carries it out. Operations that change \
         state (recording a decision, binding or unbinding, setting the agent group, inviting \
         people, changing the output level, doing coding work, handing work over) are restated \
         to the speaker by the system, and run only after they answer yes (\"yes\" / 「对」). \
         So reply must never say anything has been done, recorded, bound or the like: say only \
         what you intend to do, or leave it empty.\n\
         - Relays (relay), status (status) and recall (recall) run at once, with no \
         confirmation.\n\
         - If any of these is unclear: who to relay to, which committee, who to hand over to, \
         what to do (e.g. it cannot be told who \"she\" / 「她」 or \"that module\" / 「那个模块」 \
         is), do not guess: leave actions empty, set awaiting_reply to true, and ask briefly in \
         reply. The speaker's next message comes back to you.\n\
         - awaiting_reply is true only when you are really waiting for the speaker to answer a \
         clarifying question (their next message comes to you even without an @). A refusal, \
         an explanation, an error, asking them to split it up or to say it again later, or an \
         answer to a question: all false.\n\
         - A question you can answer straight from the log or a tool: answer it in reply, leave \
         actions empty, and cite the seq.\n\
         - When the speaker is answering an earlier question of yours, read the earlier \
         exchanges and this message together.\n\
         - Operations act in order: every operation after a bind acts on the new committee \
         (\"rebind to X, then note Y\" / 「改绑到 X，再记一下 Y」 records Y on X); those before it \
         act on the committee this group is bound to now. After an unbind this group is bound \
         to no committee, and only a bind or setting the agent group can follow.\n\
         - Never promise in reply to do something later (\"I'll do it afterwards\", \"once … is \
         done I'll …\", 「之后再做」, 「等…完成后再…」): every thing the speaker asks for either goes \
         into actions now (the system runs them in order), or you ask them in reply to say it \
         again later. Nothing remembers what reply promises to do later."
    )
}

/// How a speaker of an earlier turn reads in the prompt.
fn turn_speaker(speaker: &Address) -> String {
    match speaker {
        Address::Speaker => "you (Speaker)".to_owned(),
        other => other.in_text(),
    }
}

pub(crate) fn converse(conversation: &Conversation, max_actions: usize, known: &str) -> String {
    let utterance = &conversation.utterance;
    let earlier = if conversation.turns.is_empty() {
        "(this is the start of the conversation)".to_owned()
    } else {
        conversation
            .turns
            .iter()
            .map(|turn| {
                format!(
                    "{}: {}",
                    turn_speaker(&turn.speaker),
                    turn.text.trim().replace('\n', "\n  ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "In a thread of the group \"{group}\", {speaker} is talking with you.\n\n\
         {binding}\n\n\
         {ROSTER_HEADING}\n{roster}\n\n\
         {SEATS_HEADING}\n{seats}\n\n\
         {known}Recent log:\n{context}\n\n\
         Earlier in this thread (oldest first):\n{earlier}\n\n\
         {speaker} just said:\n\n{text}\n\n\
         Look things up with the tools first if you need to (they are read-only and change \
         nothing), then call the speaker_turn tool exactly once: reply is what you say in the \
         thread, actions are the operations for the system to run (in order, at most \
         {max_actions}); when you need the speaker to clarify, awaiting_reply is true and \
         actions is empty.",
        group = group_name(utterance),
        speaker = utterance.speaker.in_text(),
        text = utterance.text,
        binding = render_binding(utterance.committee.as_ref()),
        roster = render_roster(&utterance.roster),
        seats = render_seats(&utterance.seats),
        context = render_context_gone(&utterance.context, &utterance.gone_relays),
        known = known_section(known),
    )
}

/// The agent turn's answering tool description; also how a scripted test
/// finds it.
pub(crate) const AGENT_TURN_DESCRIPTION: &str = "Report this turn of the conversation: what to say, which operations the system \
     should run (in order), and whether you asked the speaker something. Call this exactly \
     once, after any lookups you need, and then stop.";

pub(crate) fn agent_turn_spec(max_actions: usize, work: bool) -> OutputSpec {
    OutputSpec::new(
        "speaker_turn",
        AGENT_TURN_DESCRIPTION,
        agent_turn_schema(max_actions, work),
    )
    .with_tools()
}

/// One action of the agent wire: the interpret schema in strict form. Every
/// property is required and every one but `action` nullable (null: does not
/// apply); there is no `none` (and no `reply`): the turn's `reply` says it.
fn agent_action_schema(work: bool) -> Value {
    let mut schema = intent_schema(work);
    let properties = schema["properties"]
        .as_object_mut()
        .expect("the intent schema has properties");
    properties.remove("reply");
    for (name, property) in properties.iter_mut() {
        if name == "action" {
            property["enum"] = Value::Array(
                property["enum"]
                    .as_array()
                    .expect("action has an enum")
                    .iter()
                    .filter(|a| a.as_str() != Some("none"))
                    .cloned()
                    .collect(),
            );
            property["description"] = Value::String(actions_for(work, actions_described!()));
            continue;
        }
        let kind = property["type"].clone();
        property["type"] = json!([kind, "null"]);
        if let Some(values) = property.get_mut("enum").and_then(Value::as_array_mut) {
            values.push(Value::Null);
        }
        if let Some(description) = property.get_mut("description") {
            let text = description.as_str().unwrap_or_default();
            *description = Value::String(format!("{text} null when it does not apply."));
        }
    }
    let required: Vec<Value> = properties.keys().cloned().map(Value::String).collect();
    schema["required"] = Value::Array(required);
    schema
}

fn agent_turn_schema(max_actions: usize, work: bool) -> Value {
    json!({
        "type": "object",
        "properties": {
            "reply": {
                "type": "string",
                "description": "What to say in the thread: concise, in the committee's language (the system prompt names it), citing seq numbers for anything taken from the log or a tool. Never say an operation in actions has been done: the system restates the ones that change state and waits for a yes (\"yes\" / 「对」). Never promise to do something later (\"I'll record it once the rebind is done\", 「改绑完成后再记录…」): nothing remembers it; put every operation asked for in actions now, in order (operations after a bind act on the new committee), or ask the speaker to say it again later. Empty when the actions speak for themselves."
            },
            "actions": {
                "type": "array",
                "items": agent_action_schema(work),
                "description": format!("The operations the speaker asked for, in the order they said them, one entry each; at most {max_actions}. Empty when you only answer, or ask.")
            },
            "awaiting_reply": {
                "type": "boolean",
                "description": "true only when reply asks the speaker a clarifying question you need answered before you can act (who, which committee, what exactly) and actions is empty: their next message in the thread comes back to you without an @. false for everything else: a refusal (e.g. too many operations, please split them), an explanation, an error, asking them to say it again later, or an answer."
            }
        },
        "required": ["reply", "actions", "awaiting_reply"],
        "additionalProperties": false
    })
}

/// The agent turn's payload, before validation into an [`AgentTurn`].
#[derive(Debug, Deserialize)]
pub(crate) struct AgentTurnWire {
    /// All three are required: a turn missing one is out of shape and falls
    /// back (a missing `awaiting_reply` would silently end a clarification;
    /// the runtime decodes the answering tool without checking its schema).
    reply: String,
    actions: Vec<IntentWire>,
    awaiting_reply: bool,
}

impl AgentTurnWire {
    /// Each action by the [`IntentWire`] rules, and at most `max_actions`
    /// of them (`none` aside): one out of shape rejects the whole turn, so
    /// a plan is never run with a step silently missing.
    pub(crate) fn validate(&self, work: bool, max_actions: usize) -> Result<(), String> {
        for (i, wire) in self.actions.iter().enumerate() {
            wire.validate(work)
                .map_err(|reason| format!("actions[{i}]: {reason}"))?;
        }
        let planned = self.actions.iter().filter(|a| a.action != "none").count();
        if planned > max_actions {
            return Err(format!(
                "{planned} actions, at most {max_actions}: list none of them and ask the \
                 speaker in reply to split them up (awaiting_reply false)."
            ));
        }
        Ok(())
    }

    /// As an [`AgentTurn`]; only for a wire [`validate`](Self::validate)
    /// accepted. `none` actions are dropped; when the turn says nothing
    /// else, the first one's reply becomes the turn's.
    pub(crate) fn into_turn(self) -> AgentTurn {
        let mut reply = self.reply.trim().to_owned();
        let mut actions = Vec::new();
        for wire in self.actions {
            match wire.into_intent() {
                Intent::None { reply: said } => {
                    if reply.is_empty() {
                        reply = said.trim().to_owned();
                    }
                }
                intent => actions.push(intent),
            }
        }
        AgentTurn {
            reply,
            actions,
            awaiting_reply: self.awaiting_reply,
        }
    }
}

// ---- Triage -----------------------------------------------------------

/// The triage answering tool's description; also how a scripted test finds it.
pub(crate) const TRIAGE_DESCRIPTION: &str = "Report how likely this message needs the person rather than their coding agent. \
     Call this exactly once.";

pub(crate) fn triage_spec() -> OutputSpec {
    OutputSpec::new("speaker_triage", TRIAGE_DESCRIPTION, triage_schema())
}

fn triage_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "p_needs_human": {
                "type": "number",
                "description": "0..1. The message asks for a judgement, a commitment, a priority, a deadline, money, people, or anything only the person may decide or promise; or it changes what someone else owns."
            },
            "p_agent": {
                "type": "number",
                "description": "0..1. The agent can answer it from the code, the repository or the shared log: a fact, a file, a header name, what a function does."
            },
            "p_unsure": {
                "type": "number",
                "description": "0..1. It is unclear which. The three should add up to about 1."
            },
            "reason": {
                "type": "string",
                "description": "One short line, shown to people, in the language the prompt names: why."
            }
        },
        "required": ["p_needs_human", "p_agent", "p_unsure"],
        "additionalProperties": false
    })
}

pub(crate) fn triage(input: &TriageInput) -> String {
    let to: Vec<String> = input.to.iter().map(Address::in_text).collect();
    format!(
        "In committee {committee}, {asker} sent {to} a {kind}:\n\n{body}\n\n\
         Recent log:\n{context}\n\n\
         This message passes through your triage first; then it is decided whether to push it \
         to the recipient's coding agent. The person is the one the agent belongs to: the \
         handle before the /. Judge how likely it needs that person themselves to judge or \
         commit, and how likely the agent can answer it from the code and the log. When in \
         doubt, lean toward needing the person. Write the reason in {language}. Call the \
         speaker_triage tool exactly once.",
        committee = input.committee,
        asker = input.asker.in_text(),
        to = to.join(", "),
        kind = input.kind,
        body = input.body,
        context = render_context(&input.context),
        language = language(input.locale),
    )
}

/// The triage tool's payload.
#[derive(Debug, Deserialize)]
pub(crate) struct TriageWire {
    p_needs_human: f32,
    p_agent: f32,
    p_unsure: f32,
    #[serde(default)]
    reason: Option<String>,
}

impl TriageWire {
    pub(crate) fn validate(&self) -> Result<(), String> {
        let valid = |p: f32| p.is_finite() && (0.0..=1.0).contains(&p);
        if valid(self.p_needs_human) && valid(self.p_agent) && valid(self.p_unsure) {
            Ok(())
        } else {
            Err("p_needs_human, p_agent and p_unsure must each be a number in 0..1.".into())
        }
    }

    /// As a [`TriagePayload`], the threshold applied; only for a wire
    /// [`validate`](Self::validate) accepted.
    pub(crate) fn into_payload(self) -> TriagePayload {
        apply_threshold(TriagePayload {
            verdict: senate_core::TriageVerdict::Unsure,
            p_needs_human: self.p_needs_human,
            p_agent: self.p_agent,
            p_unsure: self.p_unsure,
            rule: None,
            reason: self
                .reason
                .map(|r| r.trim().chars().take(120).collect::<String>())
                .filter(|r| !r.is_empty()),
        })
    }
}

// ---- Picking one of a person's agents ---------------------------------

/// The pick answering tool's description; also how a scripted test finds it.
pub(crate) const PICK_DESCRIPTION: &str = "Report which of the person's agents should get this task, or null when it cannot be \
     told. Call this exactly once.";

/// Open task threads shown per candidate.
const PICK_THREADS: usize = 5;

pub(crate) fn pick_spec() -> OutputSpec {
    OutputSpec::new("speaker_pick", PICK_DESCRIPTION, pick_schema())
}

fn pick_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "agent": {
                "type": ["string", "null"],
                "description": "The chosen agent's name exactly as listed, e.g. \"claude\". null when you cannot tell which: the Speaker then asks."
            },
            "reason": {
                "type": "string",
                "description": "One short line: why this agent, or why it cannot be told. For the record; not shown to people."
            }
        },
        "required": ["agent", "reason"],
        "additionalProperties": false
    })
}

fn cut(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

fn render_candidate(candidate: &AgentCandidate, now: DateTime<Utc>) -> String {
    let mut out = format!(
        "- {} · {}",
        candidate.agent,
        if candidate.present { "online" } else { "away" }
    );
    if candidate.owns_thread {
        out.push_str(" · the thread this was asked in is this agent's");
    }
    match &candidate.duty {
        Some(duty) => out.push_str(&format!("\n  duty: {}", cut(duty, 300))),
        None => out.push_str("\n  duty: none set"),
    }
    match &candidate.checkpoint {
        Some(record) => {
            let age = (now - record.ts).to_std().unwrap_or_default();
            out.push_str(&format!(
                "\n  latest checkpoint: seq {}, {} ago, {}: {}",
                record.seq,
                age_words(age),
                record.checkpoint.at(),
                cut(&record.body, 200),
            ));
        }
        None => out.push_str("\n  latest checkpoint: none"),
    }
    if candidate.open_threads.is_empty() {
        out.push_str("\n  open task threads: none");
    } else {
        let threads: Vec<String> = candidate
            .open_threads
            .iter()
            .take(PICK_THREADS)
            .map(|t| format!("seq {} \"{}\"", t.root, cut(&t.title, 80)))
            .collect();
        let more = candidate.open_threads.len().saturating_sub(PICK_THREADS);
        out.push_str(&format!("\n  open task threads: {}", threads.join("; ")));
        if more > 0 {
            out.push_str(&format!(" (and {more} more)"));
        }
    }
    out
}

pub(crate) fn pick(input: &PickInput, now: DateTime<Utc>) -> String {
    let thread = match input.thread {
        Some(root) => format!("It was asked in the task thread rooted at seq {root}."),
        None => "It was not asked in a task thread.".to_owned(),
    };
    let candidates: Vec<String> = input
        .candidates
        .iter()
        .map(|c| render_candidate(c, now))
        .collect();
    let (whose, which, listed) = match &input.owner {
        Some(owner) => {
            let owner = Address::senior(owner).in_text();
            (
                format!("an agent of {owner}"),
                format!("which of {owner}'s agents"),
                format!("{owner}'s agents here"),
            )
        }
        None => (
            "an agent on this committee".to_owned(),
            "which agent".to_owned(),
            "The agents on this committee".to_owned(),
        ),
    };
    format!(
        "In committee {committee}, {asker} wants {whose} to take this on, without \
         saying {which}:\n\n{task}\n\n\
         {thread}\n\n\
         {listed}:\n{candidates}\n\n\
         Pick the agent this belongs to. First read each agent's duty, what it does on this \
         committee: when the task falls within exactly one agent's duty, pick that agent. When \
         it fits several duties or none, weigh the task against what each agent is working on: \
         its latest checkpoint (summary, branch) and its open task threads. Prefer the agent \
         whose thread it was asked in, or whose current work it continues. When it fits two \
         agents equally, prefer one that is online, since it answers now. Do not pick by an \
         agent's name alone, and do not guess: when you cannot tell, answer null, and the \
         Speaker asks in the thread which one is meant. Call the speaker_pick tool exactly once.",
        committee = input.committee,
        asker = input.asker.in_text(),
        task = input.task,
        candidates = candidates.join("\n"),
    )
}

/// `agent` must be present, though it may be null: a payload without it is
/// out of shape, not a silent "ask".
fn required_option<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(d)
}

/// The pick tool's payload.
#[derive(Debug, Deserialize)]
pub(crate) struct PickWire {
    #[serde(deserialize_with = "required_option")]
    agent: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

impl PickWire {
    /// The candidate `agent` names (as a path, or as the mention
    /// `[senator::<path>]`); `Ok(None)` for null.
    fn named(&self, candidates: &[AgentCandidate]) -> Result<Option<SenatorName>, String> {
        let Some(raw) = &self.agent else {
            return Ok(None);
        };
        let raw = raw.trim().trim_start_matches('[').trim_end_matches(']');
        let bare = raw.strip_prefix("senator::").unwrap_or(raw);
        match bare.parse::<SenatorName>() {
            Ok(path) if candidates.iter().any(|c| c.agent == path) => Ok(Some(path)),
            _ => {
                let listed: Vec<String> = candidates.iter().map(|c| c.agent.to_string()).collect();
                Err(format!(
                    "agent {raw:?} is not one of the listed agents ({}): name one of them \
                     exactly as listed, or null when you cannot tell.",
                    listed.join(", ")
                ))
            }
        }
    }

    pub(crate) fn validate(&self, candidates: &[AgentCandidate]) -> Result<(), String> {
        self.named(candidates).map(|_| ())
    }

    /// The pick; [`Pick::Ask`] for null. Only for a wire
    /// [`validate`](Self::validate) accepted against the same candidates.
    pub(crate) fn into_pick(self, candidates: &[AgentCandidate]) -> Pick {
        tracing::debug!(agent = ?self.agent, reason = ?self.reason, "agent pick");
        match self.named(candidates) {
            Ok(Some(path)) => Pick::Agent(path),
            _ => Pick::Ask,
        }
    }
}

// ---- A message crossing into another committee ------------------------

/// The crossing answering tool's description; also how a scripted test finds it.
pub(crate) const CROSSING_DESCRIPTION: &str = "Report what the Speaker's desk does with a message an agent wrote for a seat outside \
     its committee. Call this exactly once.";

pub(crate) fn crossing_spec() -> OutputSpec {
    OutputSpec::new("speaker_crossing", CROSSING_DESCRIPTION, crossing_schema())
}

fn crossing_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "decision": {
                "type": "string",
                "enum": ["forward", "refuse", "leave"],
                "description": "forward: deliver it where it was meant to go (a plain question someone there can answer). refuse: it should not go there; the asker is told why. leave: the Speaker must decide (a request that commits someone, anything about priorities, people, money or deadlines, anything you are unsure of)."
            },
            "reason": {
                "type": "string",
                "description": "Required for refuse: one short line the asker reads, in the language the prompt names. Otherwise why, briefly."
            }
        },
        "required": ["decision"],
        "additionalProperties": false
    })
}

pub(crate) fn crossing(input: &CrossingInput) -> String {
    format!(
        "In committee {committee}, {asker} wrote a {kind} meant for {intended}, which is outside \
         its committee:\n\n{body}\n\n\
         Recent log:\n{context}\n\n\
         An agent's message that leaves its own committee passes the Speaker's desk on the way. \
         Decide: forward it when it is a plain question that someone there can answer; refuse it \
         when it should not go there, with a reason; leave it for the Speaker when it asks \
         someone to do something, commits anyone to anything, or you are not sure. When in \
         doubt, leave it. Write the reason in {language}. Call the speaker_crossing tool \
         exactly once.",
        committee = input.committee,
        asker = input.event.from.in_text(),
        kind = input.event.kind,
        intended = input.intended.in_text(),
        body = input.event.body,
        context = render_context(&input.context),
        language = language(input.locale),
    )
}

/// The crossing tool's payload.
#[derive(Debug, Deserialize)]
pub(crate) struct CrossingWire {
    decision: String,
    #[serde(default)]
    reason: Option<String>,
}

impl CrossingWire {
    fn reason(&self) -> Option<String> {
        self.reason
            .as_deref()
            .map(|r| r.trim().chars().take(200).collect::<String>())
            .filter(|r| !r.is_empty())
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        match self.decision.trim() {
            "forward" | "leave" => Ok(()),
            "refuse" if self.reason().is_some() => Ok(()),
            "refuse" => Err("refuse needs a non-empty reason.".into()),
            other => Err(format!(
                "decision {other:?} is not one of forward, refuse, leave."
            )),
        }
    }

    /// As a [`Crossing`]; only for a wire [`validate`](Self::validate)
    /// accepted.
    pub(crate) fn into_crossing(self) -> Crossing {
        match self.decision.trim() {
            "forward" => Crossing::Forward,
            "refuse" => Crossing::Refuse(self.reason().unwrap_or_default()),
            _ => Crossing::Leave,
        }
    }
}

// ---- A question for a child Senate that is offline -------------------------

/// The offline answering tool's description; also how a scripted test finds it.
pub(crate) const OFFLINE_DESCRIPTION: &str = "Report what the Speaker's office does with a question for a child Senate that is \
     offline. Call this exactly once.";

pub(crate) fn offline_spec() -> OutputSpec {
    OutputSpec::new("speaker_offline", OFFLINE_DESCRIPTION, offline_schema())
}

fn offline_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "decision": {
                "type": "string",
                "enum": ["answer", "hold"],
                "description": "answer: the material in the prompt answers the question, and you answer it on the child Senate's behalf. hold: it does not (or you are unsure); the asker is told the child Senate is offline and the question waits for it."
            },
            "answer": {
                "type": "string",
                "description": "Required for answer: the answer itself, in the language the prompt names, saying what it relies on. No pleasantries."
            }
        },
        "required": ["decision"],
        "additionalProperties": false
    })
}

pub(crate) fn offline(input: &OfflineInput, now: DateTime<Utc>) -> String {
    let child = &input.child.name;
    let asker = input
        .event
        .on_behalf_of
        .as_ref()
        .unwrap_or(&input.event.from);
    let meant = input.intended.as_ref().map_or_else(String::new, |seat| {
        format!(", meant for the seat {}", seat.in_text())
    });
    let away = input.child.last_seen.map_or_else(
        || "at a time this Senate does not know".to_owned(),
        |seen| {
            format!(
                "{} ago",
                age_words((now - seen).to_std().unwrap_or_default())
            )
        },
    );
    let seats = if input.child.seats.is_empty() {
        "(it declared no seats)".to_owned()
    } else {
        input
            .child
            .seats
            .iter()
            .map(|seat| {
                format!(
                    "- {} on {} · {} when last declared · duty: {}",
                    Address::senator(&seat.senator).in_text(),
                    seat.committee,
                    seat.presence,
                    seat.duty
                        .as_deref()
                        .map_or_else(|| "none set".to_owned(), |duty| cut(duty, 300)),
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let brief = input.brief.as_ref().map_or_else(
        || "(the brief could not be read)".to_owned(),
        |b| b.to_markdown(input.locale, TAKEOVER_BRIEF_CHARS),
    );
    format!(
        "In committee {committee}, {asker} asked the child Senate {child} this question{meant}:\n\n\
         {body}\n\n\
         {child} is offline: it last declared itself here {away}. The question stays in the log \
         and reaches {child} when it reconnects, whatever you decide.\n\n\
         The seats {child} declared here, which is all this Senate knows of what is inside it:\n\
         {seats}\n\n\
         The committee's brief here:\n{brief}\n\n\
         Recent log:\n{context}\n\n\
         Decide. Answer on {child}'s behalf only when the brief (its decisions and checkpoints), \
         the declared seats and duties, or the log above answer the question; say what you \
         relied on, do not speculate about anything inside {child}, and promise nothing in its \
         name. Otherwise hold: the asker is told {child} is offline and the question waits for \
         it. When in doubt, hold. Write an answer in {language}. Call the speaker_offline tool \
         exactly once.",
        committee = input.committee,
        asker = asker.in_text(),
        body = input.event.body,
        context = render_context(&input.context),
        language = language(input.locale),
    )
}

/// The offline tool's payload.
#[derive(Debug, Deserialize)]
pub(crate) struct OfflineWire {
    decision: String,
    #[serde(default)]
    answer: Option<String>,
}

impl OfflineWire {
    fn answer(&self) -> Option<String> {
        self.answer
            .as_deref()
            .map(|a| a.trim().to_owned())
            .filter(|a| !a.is_empty())
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        match self.decision.trim() {
            "hold" => Ok(()),
            "answer" if self.answer().is_some() => Ok(()),
            "answer" => Err("answer needs its text in answer.".into()),
            other => Err(format!("decision {other:?} is not one of answer, hold.")),
        }
    }

    /// As an [`Offline`]; only for a wire [`validate`](Self::validate)
    /// accepted.
    pub(crate) fn into_offline(self) -> Offline {
        match self.answer() {
            Some(answer) if self.decision.trim() == "answer" => Offline::Answer(answer),
            _ => Offline::Hold,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_offline_wire_is_an_answer_with_its_text_or_a_hold() {
        let wire = |value: Value| {
            let wire = serde_json::from_value::<OfflineWire>(value).expect("wire");
            wire.validate().map(|()| wire.into_offline())
        };
        assert_eq!(wire(json!({"decision": "hold"})), Ok(Offline::Hold));
        assert_eq!(
            wire(json!({"decision": "answer", "answer": " 30 days, per the brief "})),
            Ok(Offline::Answer("30 days, per the brief".into()))
        );
        assert!(wire(json!({"decision": "answer", "answer": "  "})).is_err());
        assert!(wire(json!({"decision": "forward"})).is_err());
    }

    fn wire(value: Value) -> Result<Intent, String> {
        let wire = serde_json::from_value::<IntentWire>(value).map_err(|e| e.to_string())?;
        wire.validate(true).map(|()| wire.into_intent())
    }

    #[test]
    fn memory_edit_wire_requires_one_selector_and_a_complete_correction() {
        for bad in [
            json!({"action":"forget"}),
            json!({"action":"forget","memory_query":" ? "}),
            json!({"action":"forget","memory_seq":0}),
            json!({"action":"forget","memory_seq":2,"memory_query":"重试"}),
            json!({"action":"correct","memory_seq":2,"body":" "}),
            json!({"action":"forget","memory_seq":2,"body":"new point"}),
        ] {
            assert!(wire(bad.clone()).is_err(), "{bad}");
        }
        assert_eq!(
            wire(json!({"action":"correct","memory_seq":2,"body":"五次"})),
            Ok(Intent::Correct {
                target: Seq(2),
                text: "五次".into()
            })
        );
        assert_eq!(
            wire(json!({"action":"forget","memory_query":"retries"})),
            Ok(Intent::EditMemoryByQuery {
                query: "retries".into(),
                replacement: None
            })
        );
    }

    #[test]
    fn the_wire_validates_per_action() {
        assert_eq!(
            wire(
                json!({"action": "relay", "targets": ["alice"], "kind": "question", "body": "哪个 header？"})
            ),
            Ok(Intent::Relay {
                targets: vec!["alice".into()],
                kind: RelayKind::Question,
                body: "哪个 header？".into()
            })
        );
        assert_eq!(
            wire(json!({"action": "relay", "targets": ["codex"], "body": "rebase first"})),
            Ok(Intent::Relay {
                targets: vec!["codex".into()],
                kind: RelayKind::Note,
                body: "rebase first".into()
            })
        );
        assert_eq!(
            wire(json!({"action": "status", "reply": ""})),
            Ok(Intent::Status)
        );
        assert!(wire(json!({"action": "relay", "targets": [], "body": "x"})).is_err());
        assert_eq!(
            wire(json!({"action": "decide", "body": "退款上限 30 天", "supersedes": 12})),
            Ok(Intent::Decide {
                body: "退款上限 30 天".into(),
                supersedes: Some(Seq(12)),
            })
        );
        assert_eq!(wire(json!({"action": "recall"})), Ok(Intent::Recall));
        assert!(wire(json!({"action": "decide", "body": " "})).is_err());
        assert!(wire(json!({"action": "bind"})).is_err());
        assert!(wire(json!({"action": "launch"})).is_err());
        assert_eq!(
            wire(json!({"action": "adopt_agent_group"})),
            Ok(Intent::AdoptAgentGroup { committee: None })
        );
        assert_eq!(
            wire(json!({"action": "set_output_level", "level": "all"})),
            Ok(Intent::SetOutputLevel {
                level: OutputLevel::All
            })
        );
        assert!(wire(json!({"action": "invite_to_agent_group"})).is_err());
        assert_eq!(
            wire(json!({"action": "work", "target": "alice", "instruction": "把重试路径做完"})),
            Ok(Intent::Work {
                target: "alice".into(),
                instruction: "把重试路径做完".into()
            })
        );
        assert!(wire(json!({"action": "work", "target": "alice"})).is_err());
        assert_eq!(
            wire(json!({"action": "reassign", "from": "alice", "to": "bob"})),
            Ok(Intent::Reassign {
                from: "alice".into(),
                to: "bob".into()
            })
        );
        assert_eq!(
            wire(json!({"action": "reassign", "to": "alice"})),
            Ok(Intent::Reassign {
                from: "alice".into(),
                to: "alice".into()
            })
        );
    }

    #[test]
    fn triage_applies_the_conservative_threshold() {
        use senate_core::TriageVerdict::{Agent, NeedsHuman, Unsure};
        let payload = |nh: f32, a: f32, u: f32| {
            let wire = TriageWire {
                p_needs_human: nh,
                p_agent: a,
                p_unsure: u,
                reason: None,
            };
            wire.validate().map(|()| wire.into_payload().verdict)
        };
        assert_eq!(payload(0.5, 0.5, 0.0), Ok(NeedsHuman));
        assert_eq!(payload(0.2, 0.7, 0.1), Ok(Agent));
        assert_eq!(payload(0.3, 0.3, 0.4), Ok(Unsure));
        assert!(payload(1.5, 0.0, 0.0).is_err());
    }

    #[test]
    fn the_schema_names_every_action() {
        let schema = intent_schema(true);
        let actions = schema["properties"]["action"]["enum"]
            .as_array()
            .expect("enum");
        assert_eq!(actions.len(), 14);
        let action = schema["properties"]["action"]["description"]
            .as_str()
            .expect("description");
        // People type either language: both are taught.
        for example in [
            "记一下",
            "记住",
            "你记得什么",
            "现在定了哪些",
            "note this",
            "what do you remember",
            "tell alice's agent",
            "告诉 alice 的 agent",
        ] {
            assert!(action.contains(example), "{example}");
        }
    }

    /// Thread 797 item 8: without coding work configured, `work` is not
    /// offered (nor its fields), not accepted, and a change is relayed.
    #[test]
    fn work_is_offered_only_when_it_is_configured() {
        for schema in [
            intent_schema(false),
            agent_turn_schema(5, false)["properties"]["actions"]["items"].clone(),
        ] {
            let properties = &schema["properties"];
            let actions = properties["action"]["enum"].as_array().expect("enum");
            assert!(!actions.contains(&json!("work")), "{actions:?}");
            assert!(properties.get("target").is_none());
            assert!(properties.get("instruction").is_none());
            let described = properties["action"]["description"]
                .as_str()
                .expect("description");
            assert!(
                !described.contains("work: the speaker wants you"),
                "{described}"
            );
            assert!(
                described.contains("There is no work action here"),
                "{described}"
            );
            assert!(described.contains("reassign:"), "{described}");
        }
        assert_strict(&agent_turn_schema(5, false), "turn");
        let with = intent_schema(true);
        assert!(
            with["properties"]["action"]["enum"]
                .as_array()
                .expect("enum")
                .contains(&json!("work"))
        );
        assert!(
            with["properties"]["action"]["description"]
                .as_str()
                .expect("description")
                .contains("work: the speaker wants you")
        );

        let work = json!({"action": "work", "target": "alice", "instruction": "加测试"});
        let parse = |value: &Value, work: bool| {
            let wire = serde_json::from_value::<IntentWire>(value.clone()).expect("wire");
            wire.validate(work).map(|()| wire.into_intent())
        };
        assert!(parse(&work, false).is_err());
        assert!(parse(&work, true).is_ok());
        assert_eq!(
            parse(
                &json!({"action": "relay", "targets": ["bob-desk::codex"], "kind": "request", "body": "在 README 末尾加一节"}),
                false
            ),
            Ok(Intent::Relay {
                targets: vec!["bob-desk::codex".into()],
                kind: RelayKind::Request,
                body: "在 README 末尾加一节".into(),
            })
        );
    }

    /// Thread 797 item 8: a change request that names nobody goes by duty
    /// as a request.
    #[test]
    fn change_requests_are_routed_by_duty_too() {
        let committee = CommitteeId::new("payments").expect("committee");
        let text = persona(Some(&committee), "Speaker", Locale::Zh);
        for rule in [
            "a question or a change request that names no person",
            "kind request for a change or a task to carry out",
            "Route only questions and change requests by duty",
            "A question about this Senate's own state is yours to answer",
            "marked as put to a seat that is gone",
        ] {
            assert!(text.contains(rule), "{rule}");
        }
        let kind = intent_schema(false)["properties"]["kind"].clone();
        assert!(
            kind["enum"]
                .as_array()
                .expect("enum")
                .contains(&json!("request"))
        );
    }

    #[test]
    fn a_target_is_a_person_or_one_agent_named_outright() {
        let schema = intent_schema(true);
        let targets = schema["properties"]["targets"]["description"]
            .as_str()
            .expect("targets");
        for rule in [
            "A person's handle from the roster",
            "the system settles which of their agents",
            "written by its own name",
            "only when the speaker named that agent",
            "「alice 的 codex」",
            "Never choose one of a person's agents yourself",
            "never guess a person who is not named",
        ] {
            assert!(targets.contains(rule), "{rule} in {targets}");
        }
        for field in ["target", "from", "to"] {
            let text = schema["properties"][field]["description"]
                .as_str()
                .expect(field);
            assert!(text.contains("by its own name"), "{field}: {text}");
        }
    }

    /// Strict mode (what providers apply to the answering tool) wants every
    /// property required, at every level, and no extra ones.
    fn assert_strict(schema: &Value, path: &str) {
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            let required: Vec<&str> = schema["required"]
                .as_array()
                .unwrap_or_else(|| panic!("{path}: required"))
                .iter()
                .filter_map(Value::as_str)
                .collect();
            for (key, property) in properties {
                assert!(
                    required.contains(&key.as_str()),
                    "{path}.{key} not required"
                );
                assert_strict(property, &format!("{path}.{key}"));
            }
            assert_eq!(schema["additionalProperties"], false, "{path}");
        }
        if let Some(items) = schema.get("items") {
            assert_strict(items, &format!("{path}[]"));
        }
    }

    #[test]
    fn the_agent_turn_schema_is_strict_and_has_no_none() {
        let schema = agent_turn_schema(5, true);
        assert_strict(&schema, "turn");
        let action = &schema["properties"]["actions"]["items"];
        let actions = action["properties"]["action"]["enum"]
            .as_array()
            .expect("enum");
        assert_eq!(actions.len(), 13);
        assert!(!actions.contains(&json!("none")));
        assert!(action["properties"].get("reply").is_none());
        for (name, property) in action["properties"].as_object().expect("properties") {
            if name != "action" {
                assert!(
                    property["type"]
                        .as_array()
                        .is_some_and(|t| t.contains(&json!("null"))),
                    "{name} is nullable"
                );
            }
        }
        assert_eq!(
            action["properties"]["level"]["enum"]
                .as_array()
                .map(|e| e.contains(&Value::Null)),
            Some(true)
        );
        let reply = schema["properties"]["reply"]["description"]
            .as_str()
            .expect("reply");
        assert!(
            reply.contains("Never promise to do something later"),
            "{reply}"
        );
        // A refusal is not a question.
        let awaiting = schema["properties"]["awaiting_reply"]["description"]
            .as_str()
            .expect("awaiting_reply");
        assert!(awaiting.contains("true only when"), "{awaiting}");
        assert!(
            awaiting.contains("a refusal (e.g. too many operations"),
            "{awaiting}"
        );
        assert!(
            schema["properties"]["actions"]["description"]
                .as_str()
                .is_some_and(|d| d.contains("at most 5"))
        );
    }

    #[test]
    fn the_agent_tool_schemas_are_strict() {
        for schema in super::super::agent_tools::schemas() {
            assert_strict(&schema, "tool");
        }
    }

    #[test]
    fn the_pick_schema_is_strict() {
        assert_strict(&pick_schema(), "pick");
    }

    #[test]
    fn a_strict_action_with_nulls_reads_like_interpret() {
        let wire: AgentTurnWire = serde_json::from_value(json!({
            "reply": " 请确认 ",
            "actions": [{
                "action": "reassign", "targets": null, "kind": null, "body": null,
                "supersedes": null, "committee": null, "level": null, "target": null,
                "instruction": null, "from": "alice", "to": "bob"
            }],
            "awaiting_reply": false
        }))
        .expect("wire");
        assert_eq!(
            wire.validate(true, 5).map(|()| wire.into_turn()),
            Ok(AgentTurn::act(
                "请确认",
                vec![Intent::Reassign {
                    from: "alice".into(),
                    to: "bob".into()
                }]
            ))
        );
    }

    /// A turn missing any top-level field fails rather than reading as an
    /// empty plan or a clarification that waits for nothing.
    #[test]
    fn a_turn_missing_a_field_is_out_of_shape() {
        for value in [
            json!({}),
            json!({ "reply": "请确认" }),
            json!({ "reply": "是谁？", "actions": [] }),
            json!({ "actions": [], "awaiting_reply": false }),
            json!({ "reply": null, "actions": [], "awaiting_reply": false }),
        ] {
            assert!(
                serde_json::from_value::<AgentTurnWire>(value.clone()).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn a_statement_with_an_implicit_question_is_a_question() {
        let schema = intent_schema(true);
        let kind = schema["properties"]["kind"]["description"]
            .as_str()
            .expect("description");
        assert!(kind.contains("你那边改了吗"), "{kind}");
        assert!(kind.contains("have you changed yours?"), "{kind}");
        assert!(kind.contains("note only when nothing is asked"), "{kind}");
    }

    #[test]
    fn the_persona_names_the_bot_and_the_language() {
        let committee = CommitteeId::new("refund-v2").expect("committee");
        let zh = persona(Some(&committee), "Helper", Locale::Zh);
        assert!(
            zh.contains("People address you in chat as @Helper."),
            "{zh}"
        );
        assert!(zh.contains("Write everything people will read in Chinese."));
        assert!(zh.contains("refund-v2"));
        let en = agent_persona(None, "Speaker", Locale::En, 3);
        assert!(en.contains("@Speaker"));
        assert!(en.contains("Write everything people will read in English."));
        assert!(en.contains("at most 3"));
        assert!(!en.contains(ONLY_THE_LOG), "agent mode can look things up");
        assert!(en.contains("committee_brief"));
    }

    #[test]
    fn a_pick_names_a_candidate_or_asks() {
        let candidate = |path: &str| AgentCandidate {
            agent: path.parse().expect("path"),
            present: false,
            checkpoint: None,
            open_threads: Vec::new(),
            owns_thread: false,
            duty: None,
        };
        let candidates = [candidate("claude"), candidate("codex")];
        let pick = |value: Value| {
            let wire = serde_json::from_value::<PickWire>(value).expect("wire");
            wire.validate(&candidates)
                .map(|()| wire.into_pick(&candidates))
                .unwrap_or_else(|reason| panic!("{reason}"))
        };
        let refused = |value: Value| {
            serde_json::from_value::<PickWire>(value)
                .expect("wire")
                .validate(&candidates)
                .expect_err("not a candidate")
        };
        let codex: SenatorName = "codex".parse().expect("path");
        assert_eq!(
            pick(json!({ "agent": "codex", "reason": "r" })),
            Pick::Agent(codex.clone())
        );
        assert_eq!(
            pick(json!({ "agent": "[senator::codex]", "reason": "r" })),
            Pick::Agent(codex)
        );
        assert_eq!(pick(json!({ "agent": null, "reason": "r" })), Pick::Ask);
        // Anything else goes back to the model, with the listed agents.
        for named in ["copilot", "alice", ""] {
            let reason = refused(json!({ "agent": named, "reason": "r" }));
            assert!(reason.contains("claude, codex"), "{reason}");
        }
        assert!(serde_json::from_value::<PickWire>(json!({ "reason": "r" })).is_err());
    }
}

// ---- Digesting the discussion -----------------------------------------

/// The digest tool's description; also how a scripted test finds it.
pub(crate) const DIGEST_DESCRIPTION: &str =
    "Report the digest of this discussion and the memory points it adds. Call this exactly once.";

pub(crate) fn digest_spec() -> OutputSpec {
    OutputSpec::new("speaker_digest", DIGEST_DESCRIPTION, digest_schema())
}

fn digest_schema() -> Value {
    let refs = json!({
        "type": "array",
        "items": { "type": "integer" },
        "description": "The seq of every message listed above this comes from, e.g. [12, 15]. Only seqs listed above."
    });
    json!({
        "type": "object",
        "properties": {
            "points": {
                "type": "array",
                "maxItems": senate_core::memory::MAX_DIGEST_POINTS,
                "description": "What was discussed, most important first: one short line each.",
                "items": {
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "description": "One line, in the language asked for." },
                        "refs": refs.clone()
                    },
                    "required": ["text", "refs"],
                    "additionalProperties": false
                }
            },
            "memory": {
                "type": "array",
                "description": "Facts from this discussion that will still matter in weeks: who owns what, how something works, a constraint, a date. Empty when nothing lasting was said.",
                "items": {
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "description": "The fact, one self-contained line, in the language asked for." },
                        "refs": refs,
                        "supersedes": {
                            "type": "array",
                            "items": { "type": "integer" },
                            "description": "The seq of each point under \"What we know\" this fact replaces (it is no longer true, or this says it better). Never a point marked (by a person). Usually empty."
                        }
                    },
                    "required": ["text", "refs", "supersedes"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["points", "memory"],
        "additionalProperties": false
    })
}

pub(crate) fn digest(input: &DigestInput) -> String {
    let discussion: Vec<String> = input.events.iter().map(render_event).collect();
    let memory = if input.memory.is_empty() {
        "(nothing yet)".to_owned()
    } else {
        input
            .memory
            .iter()
            .map(|p| {
                let by = if p.by_person() { " (by a person)" } else { "" };
                format!("- seq {}{by}: {}", p.seq, cut(&p.text, 200))
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let decisions = if input.decisions.is_empty() {
        "(none)".to_owned()
    } else {
        input
            .decisions
            .iter()
            .map(|d| format!("- seq {}: {}", d.seq, cut(&d.body, 200)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "You keep the memory of committee {committee}. Below is what was said in its groups since \
         the last digest (and what crossed in from other committees), oldest first.\n\n\
         {discussion}\n\n\
         What we know (memory points in force):\n{memory}\n\n\
         Decisions in force (already recorded; do not restate them):\n{decisions}\n\n\
         Report:\n\
         - points: at most {max} one-line points of what was discussed, settled or left open, \
         most important first. Group the messages by topic; skip greetings and chit-chat.\n\
         - memory: only facts that will still matter later, each one self-contained line. When \
         one replaces a point under \"What we know\", name that point's seq in supersedes; never \
         a point marked (by a person).\n\
         Every point and fact cites the seqs listed above it comes from, and nothing else. Write \
         in {language}; keep names, numbers and identifiers as written. Use only what is above: \
         do not guess.",
        committee = input.committee,
        discussion = discussion.join("\n"),
        max = senate_core::memory::MAX_DIGEST_POINTS,
        language = language(input.locale),
    )
}

/// The digest tool's payload.
#[derive(Debug, Deserialize)]
pub(crate) struct DigestWire {
    #[serde(default)]
    points: Vec<DigestPointWire>,
    #[serde(default)]
    memory: Vec<MemoryWire>,
}

#[derive(Debug, Deserialize)]
struct DigestPointWire {
    text: String,
    #[serde(default)]
    refs: Vec<u64>,
}

#[derive(Debug, Deserialize)]
struct MemoryWire {
    text: String,
    #[serde(default)]
    refs: Vec<u64>,
    #[serde(default)]
    supersedes: Vec<u64>,
}

impl DigestWire {
    /// As the Brain's answer; the engine checks every seq against what it
    /// showed.
    pub(crate) fn into_out(self) -> DigestOut {
        let seqs = |v: Vec<u64>| v.into_iter().map(Seq).collect();
        DigestOut {
            points: self
                .points
                .into_iter()
                .map(|p| senate_core::DigestPoint {
                    text: p.text,
                    refs: seqs(p.refs),
                })
                .collect(),
            memory: self
                .memory
                .into_iter()
                .map(|m| MemoryDraft {
                    text: m.text,
                    refs: seqs(m.refs),
                    supersedes: seqs(m.supersedes),
                })
                .collect(),
        }
    }
}

// ---- Agreements in discussion -----------------------------------------

/// The agreement tool's description; also how a scripted test finds it.
pub(crate) const AGREEMENT_DESCRIPTION: &str = "Report the one clear agreement in this discussion, or null when there is none. Call this exactly once.";

pub(crate) fn agreement_spec() -> OutputSpec {
    OutputSpec::new(
        "speaker_agreement",
        AGREEMENT_DESCRIPTION,
        agreement_schema(),
    )
}

fn agreement_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "agreement": {
                "type": ["object", "null"],
                "properties": {
                    "statement": { "type": "string", "description": "What was agreed, as one concrete sentence a decision would read, in the language asked for." },
                    "topic": { "type": "string", "description": "What it is about, two to four words." },
                    "refs": { "type": "array", "items": { "type": "integer" }, "description": "The seq of each message above that shows the agreement." },
                    "agreeing": { "type": "array", "items": { "type": "string" }, "description": "Who agreed, as written above, e.g. \"[senator::ana]\"." }
                },
                "required": ["statement", "topic", "refs", "agreeing"],
                "additionalProperties": false,
                "description": "null unless two people or more clearly agreed on one concrete statement and nobody objected or hesitated."
            }
        },
        "required": ["agreement"],
        "additionalProperties": false
    })
}

pub(crate) fn agreement(input: &AgreementInput) -> String {
    let discussion: Vec<String> = input.events.iter().map(render_event).collect();
    let proposed = if input.proposed.is_empty() {
        "(none)".to_owned()
    } else {
        input.proposed.join("; ")
    };
    let decisions = if input.decisions.is_empty() {
        "(none)".to_owned()
    } else {
        input
            .decisions
            .iter()
            .map(|d| format!("- {}", cut(&d.body, 200)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "Below is a burst of discussion in a group of committee {committee}, oldest first.\n\n\
         {discussion}\n\n\
         Topics already proposed as decisions: {proposed}\n\
         Decisions already recorded:\n{decisions}\n\n\
         Is there one clear agreement here: two people or more agreeing on one concrete \
         statement (a choice, a number, a rule, an owner), with nobody objecting, hesitating or \
         asking to wait? A question, an idea one person floated, a vague \"sounds good\" to \
         nothing specific, or anything already proposed or recorded is not one. When in doubt, \
         answer null: people are asked only when it is clear. Write the statement in \
         {language}; keep names and numbers as written.",
        committee = input.committee,
        discussion = discussion.join("\n"),
        language = language(input.locale),
    )
}

/// The agreement tool's payload.
#[derive(Debug, Deserialize)]
pub(crate) struct AgreementWire {
    #[serde(default)]
    agreement: Option<AgreementBody>,
}

#[derive(Debug, Deserialize)]
struct AgreementBody {
    statement: String,
    topic: String,
    #[serde(default)]
    refs: Vec<u64>,
    #[serde(default)]
    agreeing: Vec<String>,
}

impl AgreementWire {
    /// As the Brain's answer; who agreed is read as addresses, and the
    /// engine checks them against the burst.
    pub(crate) fn into_agreement(self) -> Option<Agreement> {
        let body = self.agreement?;
        Some(Agreement {
            statement: body.statement,
            topic: body.topic,
            refs: body.refs.into_iter().map(Seq).collect(),
            agreeing: body
                .agreeing
                .iter()
                .filter_map(|who| {
                    who.trim()
                        .trim_start_matches('[')
                        .trim_end_matches(']')
                        .parse()
                        .ok()
                })
                .collect(),
        })
    }
}
