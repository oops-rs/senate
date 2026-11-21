//! What people see when the Speaker's model fails: the asker gets one line
//! in their language (with the command, when what they wrote is one without
//! its `/`), never the fixed-syntax dump; the technical error goes to the
//! Agent group, else to the Speaker's person as a note. With no model at
//! all, a message that is not a command is still "not understood".

use senate_core::{Address, Hub, Kind, Locale, PostTarget, SenatorId, SpeakerName};

use super::hub::MemoryHub;
use super::rig::{
    GROUP, Rig, agents_bind_event, assert_senate_vocabulary, bind_event, config_in, mention, path,
    refund, roster, unbound,
};
use super::surface::FakeSurface;
use crate::render;
use crate::{BrainError, ScriptedBrain};

const AGENTS: &str = "agents";
const ERROR: &str = "run completed without invoking the expected terminal tool";

async fn hub(agents_group: bool, speaker: bool) -> std::sync::Arc<MemoryHub> {
    let hub = MemoryHub::new();
    if speaker {
        hub.set_roster(
            roster()
                .with_speaker(SpeakerName::id(SenatorId::here(path("pm"))))
                .expect("pm presides"),
        );
    }
    hub.append(bind_event(&refund(), GROUP))
        .await
        .expect("bind");
    if agents_group {
        hub.append(agents_bind_event(&refund(), AGENTS))
            .await
            .expect("agents");
    }
    hub
}

async fn rig(hub: std::sync::Arc<MemoryHub>, locale: Locale) -> Rig {
    Rig::start_config(
        hub,
        FakeSurface::new(),
        ScriptedBrain::new(),
        config_in(locale),
    )
    .await
}

fn notes_to_pm(rig: &Rig) -> Vec<String> {
    rig.hub
        .events(&refund())
        .into_iter()
        .filter(|e| {
            e.kind == Kind::Note
                && e.from == Address::Speaker
                && e.to == vec![Address::senator(&path("pm"))]
        })
        .map(|e| e.body)
        .collect()
}

#[tokio::test]
async fn zh_the_asker_gets_one_line_and_the_speakers_person_a_note() {
    let rig = rig(hub(false, true).await, Locale::Zh).await;
    rig.brain
        .interprets(Err(BrainError::Unavailable(ERROR.into())));
    let chat = rig.append(mention("bob", "帮我看看这个", "m_d1")).await;
    rig.settle().await;
    let said = rig.surface.texts();
    assert_eq!(said, vec!["议长的模型暂时不可用，请稍后再试。".to_owned()]);
    assert!(!said[0].contains("/tell"), "no fixed-syntax dump");
    assert!(!said[0].contains("没法理解"));

    let notes = notes_to_pm(&rig);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains(ERROR), "{}", notes[0]);
    assert!(notes[0].contains(&format!("seq {}", chat.seq)));
    for text in said.iter().chain(&notes) {
        assert_senate_vocabulary(text);
    }
    rig.stop().await;
}

#[tokio::test]
async fn en_the_asker_is_offered_the_command_they_almost_wrote() {
    let rig = rig(hub(false, true).await, Locale::En).await;
    rig.brain
        .interprets(Err(BrainError::Unavailable(ERROR.into())));
    rig.append(mention("bob", "tell alice the build is green", "m_d2"))
        .await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts(),
        vec![
            "The Speaker's model is unavailable right now; try again shortly, or use the command: /tell alice the build is green"
                .to_owned()
        ]
    );
    rig.brain
        .interprets(Err(BrainError::Invalid("unknown action \"x\"".into())));
    rig.append(mention("bob", "have a look at this", "m_d3"))
        .await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("The Speaker's model is unavailable right now; try again shortly.")
    );
    assert_eq!(notes_to_pm(&rig).len(), 2);
    rig.stop().await;
}

#[tokio::test]
async fn with_an_agent_group_the_error_goes_there_not_into_a_note() {
    let rig = rig(hub(true, true).await, Locale::Zh).await;
    rig.brain
        .interprets(Err(BrainError::Unavailable(ERROR.into())));
    rig.append(mention("bob", "帮我看看这个", "m_d4")).await;
    rig.settle().await;
    let into_agents: Vec<String> = rig
        .surface
        .posts()
        .into_iter()
        .filter(|p| matches!(&p.target, PostTarget::NewRoot { group_id } if group_id == AGENTS))
        .map(|p| p.markdown)
        .collect();
    assert_eq!(into_agents.len(), 1, "{into_agents:?}");
    assert!(into_agents[0].contains(ERROR));
    assert!(notes_to_pm(&rig).is_empty());
    assert!(
        rig.surface
            .texts()
            .contains(&render::model_unavailable(Locale::Zh, None))
    );
    rig.stop().await;
}

#[tokio::test]
async fn without_a_speakers_person_the_error_is_only_logged() {
    let rig = rig(hub(false, false).await, Locale::Zh).await;
    rig.brain
        .interprets(Err(BrainError::Unavailable(ERROR.into())));
    rig.append(mention("bob", "帮我看看这个", "m_d5")).await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts(),
        vec![render::model_unavailable(Locale::Zh, None)]
    );
    assert!(
        rig.hub
            .events(&refund())
            .iter()
            .all(|e| e.kind != Kind::Note)
    );
    rig.stop().await;
}

#[tokio::test]
async fn with_no_model_a_message_is_not_understood_as_before() {
    let rig = rig(hub(false, true).await, Locale::Zh).await;
    rig.brain.interprets(Err(BrainError::NoModel));
    rig.append(mention("bob", "帮我看看这个", "m_d6")).await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts(),
        vec![render::not_understood(Locale::Zh)]
    );
    assert!(notes_to_pm(&rig).is_empty());
    rig.stop().await;
}

#[tokio::test]
async fn an_unbound_mention_gets_the_same_line() {
    let rig = rig(MemoryHub::new(), Locale::Zh).await;
    rig.brain
        .interprets(Err(BrainError::Unavailable(ERROR.into())));
    rig.mention_unbound(unbound("pm", "status", "m_d7")).await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts(),
        vec![render::model_unavailable(Locale::Zh, Some("/status"))]
    );
    rig.stop().await;
}
