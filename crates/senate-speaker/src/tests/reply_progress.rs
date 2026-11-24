use std::time::Duration;

use senate_core::{Hub, Kind, Locale, PostTarget, StatusState, SurfaceError};

use super::hub::MemoryHub;
use super::rig::{GROUP, Rig, bind_event, config_in, mention, refund};
use super::surface::{Caps, FakeSurface};
use crate::{AgentTurn, BrainError, Intent, ScriptedBrain, SpeakerMode};

const MODEL_DELAY: Duration = Duration::from_secs(3);
const REPLY: &str = "Here is your answer.";

async fn rig(mode: SpeakerMode, locale: Locale, cards: bool) -> Rig {
    let hub = MemoryHub::new();
    hub.append(bind_event(&refund(), GROUP))
        .await
        .expect("bind");
    let mut config = config_in(locale);
    config.mode = mode;
    let caps = Caps {
        cards,
        ..Caps::default()
    };
    Rig::start_config(hub, FakeSurface::with(caps), ScriptedBrain::new(), config).await
}

fn script_reply(rig: &Rig, mode: SpeakerMode, delay: Duration) {
    match mode {
        SpeakerMode::Intent => {
            rig.brain.interprets_after(
                delay,
                Ok(Intent::None {
                    reply: REPLY.into(),
                }),
            );
        }
        SpeakerMode::Agent => {
            rig.brain.converses_after(delay, Ok(AgentTurn::say(REPLY)));
        }
    }
}

async fn yield_to_workers() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

async fn advance(duration: Duration) {
    tokio::time::advance(duration).await;
    yield_to_workers().await;
}

#[tokio::test(start_paused = true)]
async fn slow_turn_shows_threaded_progress_then_keeps_the_normal_reply_in_both_modes() {
    for mode in [SpeakerMode::Intent, SpeakerMode::Agent] {
        let rig = rig(mode, Locale::En, true).await;
        script_reply(&rig, mode, MODEL_DELAY);
        let chat = rig
            .append(mention("pm", "Can you explain this?", "ask"))
            .await;
        yield_to_workers().await;
        advance(Duration::from_millis(900)).await;
        assert!(rig.surface.cards().is_empty(), "{mode:?}");
        advance(Duration::from_millis(200)).await;

        let target = PostTarget::Reply {
            message_ref: "ask".into(),
            in_thread: true,
        };
        let pending = rig
            .surface
            .card("Speaker request")
            .expect("slow turn shows a card");
        assert_eq!(pending.target, target);
        assert_eq!(pending.first.state, StatusState::Processing);
        assert_eq!(
            pending.now.markdown,
            "The Speaker is working on your request."
        );
        assert!(rig.surface.texts().is_empty());

        rig.settle().await;
        yield_to_workers().await;
        let finished = rig.surface.card("Speaker request").expect("progress card");
        assert_eq!(finished.message_ref, pending.message_ref);
        assert_eq!(finished.updates, 1);
        assert_eq!(finished.now.state, StatusState::Archived);
        assert_eq!(finished.now.markdown, "The Speaker finished processing.");
        let posts = rig.surface.posts();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].target, target);
        assert_eq!(posts[0].markdown, REPLY);
        assert_ne!(posts[0].operation_id, pending.operation_id);
        assert_eq!(rig.cursor(&refund()).await, chat.seq);
        assert_eq!(rig.surface.conflicts(), 0);
        rig.stop().await;
    }
}

#[tokio::test(start_paused = true)]
async fn fast_fixed_ambient_and_simulated_chat_leave_no_progress_card() {
    for mode in [SpeakerMode::Intent, SpeakerMode::Agent] {
        let rig = rig(mode, Locale::En, true).await;
        script_reply(&rig, mode, Duration::ZERO);
        rig.append(mention("pm", "Can you explain this?", "quick"))
            .await;
        rig.settle().await;
        rig.append(mention("pm", "/status", "fixed")).await;
        rig.settle().await;
        let mut ambient = mention("pm", "The build is done.", "ambient");
        ambient.to.clear();
        rig.append(ambient).await;
        rig.settle().await;
        assert_eq!(
            rig.brain.utterances().len() + rig.brain.conversations().len(),
            1
        );

        script_reply(&rig, mode, MODEL_DELAY);
        let mut simulated = mention("pm", "Can you explain this?", "simulated");
        simulated.source.as_mut().expect("source").simulated = true;
        rig.append(simulated).await;
        yield_to_workers().await;
        advance(Duration::from_millis(1100)).await;
        assert!(rig.surface.cards().is_empty(), "{mode:?}");
        rig.settle().await;
        yield_to_workers().await;
        assert!(rig.surface.cards().is_empty(), "{mode:?}");
        rig.stop().await;
    }
}

#[tokio::test(start_paused = true)]
async fn agent_progress_stops_when_the_confirmation_is_posted() {
    let rig = rig(SpeakerMode::Agent, Locale::Zh, true).await;
    rig.brain.converses_after(
        MODEL_DELAY,
        Ok(AgentTurn::act(
            "",
            vec![Intent::Decide {
                body: "采用指数退避".into(),
                supersedes: None,
            }],
        )),
    );
    rig.append(mention("pm", "记下这个决定", "ask")).await;
    yield_to_workers().await;
    advance(Duration::from_millis(1100)).await;
    let pending = rig
        .surface
        .card("Speaker 请求")
        .expect("localized progress");
    assert_eq!(pending.now.markdown, "Speaker 正在处理请求。");
    rig.settle().await;
    yield_to_workers().await;
    let finished = rig.surface.card("Speaker 请求").expect("progress card");
    assert_eq!(finished.now.state, StatusState::Archived);
    assert_eq!(finished.now.markdown, "Speaker 正在等待你的确认。");
    assert!(
        rig.surface
            .texts()
            .iter()
            .any(|text| text.contains("采用指数退避"))
    );
    assert!(
        !rig.hub
            .events(&refund())
            .iter()
            .any(|event| event.kind == Kind::Decision)
    );
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn slow_model_failure_archives_progress_without_publishing_diagnostics() {
    let rig = rig(SpeakerMode::Agent, Locale::En, true).await;
    rig.brain.converses_after(
        MODEL_DELAY,
        Err(BrainError::Unavailable("private model error".into())),
    );
    rig.append(mention("pm", "Can you explain this?", "ask"))
        .await;
    yield_to_workers().await;
    advance(Duration::from_millis(1100)).await;
    assert_eq!(
        rig.surface
            .card("Speaker request")
            .expect("progress card")
            .now
            .state,
        StatusState::Processing
    );
    rig.settle().await;
    yield_to_workers().await;
    let card = rig.surface.card("Speaker request").expect("progress card");
    assert_eq!(card.updates, 1);
    assert_eq!(card.now.state, StatusState::Archived);
    assert_eq!(
        card.now.markdown,
        "The Speaker’s model is temporarily unavailable."
    );
    assert_eq!(
        rig.surface.texts(),
        ["The Speaker's model is unavailable right now; try again shortly."]
    );
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn reply_stall_ends_progress_and_ordinary_retry_still_answers() {
    let mode = SpeakerMode::Intent;
    let rig = rig(mode, Locale::En, true).await;
    script_reply(&rig, mode, MODEL_DELAY);
    rig.append(mention("pm", "Can you explain this?", "ask"))
        .await;
    yield_to_workers().await;
    advance(Duration::from_millis(1100)).await;
    rig.surface
        .fail_next(SurfaceError::Unavailable("offline".into()));
    advance(Duration::from_secs(2)).await;
    let card = rig.surface.card("Speaker request").expect("progress card");
    assert_eq!(card.now.state, StatusState::Archived);
    assert_eq!(
        card.now.markdown,
        "The Speaker could not finish this attempt."
    );
    assert!(rig.surface.texts().is_empty());
    advance(Duration::from_secs(3)).await;
    rig.settle().await;
    assert_eq!(rig.surface.texts(), [REPLY]);
    assert_eq!(rig.brain.utterances().len(), 1);
    assert_eq!(rig.surface.cards().len(), 1);
    assert_eq!(rig.surface.conflicts(), 0);
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn unsupported_cards_do_not_interfere_with_the_reply() {
    let mode = SpeakerMode::Intent;
    let rig = rig(mode, Locale::En, false).await;
    script_reply(&rig, mode, MODEL_DELAY);
    rig.append(mention("pm", "Can you explain this?", "ask"))
        .await;
    rig.settle().await;
    assert_eq!(rig.surface.texts(), [REPLY]);
    assert!(rig.surface.cards().is_empty());
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn an_in_flight_card_receipt_neither_delays_the_reply_nor_leaves_the_card_busy() {
    let mode = SpeakerMode::Intent;
    let rig = rig(mode, Locale::En, true).await;
    let receipt = rig.surface.hold_card_receipt();
    script_reply(&rig, mode, MODEL_DELAY);
    let chat = rig
        .append(mention("pm", "Can you explain this?", "ask"))
        .await;
    yield_to_workers().await;
    advance(Duration::from_millis(1100)).await;
    let card = rig
        .surface
        .card("Speaker request")
        .expect("progress was posted");
    assert_eq!(card.now.state, StatusState::Processing);

    advance(Duration::from_secs(2)).await;
    assert_eq!(rig.surface.texts(), [REPLY]);
    assert_eq!(rig.cursor(&refund()).await, chat.seq);
    assert_eq!(
        rig.surface.card("Speaker request").expect("card").updates,
        0
    );

    receipt.notify_one();
    yield_to_workers().await;
    let finished = rig.surface.card("Speaker request").expect("progress card");
    assert_eq!(finished.message_ref, card.message_ref);
    assert_eq!(finished.now.state, StatusState::Archived);
    assert_eq!(finished.updates, 1);
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn lost_card_receipt_is_recovered_with_the_same_operation() {
    let mode = SpeakerMode::Intent;
    let rig = rig(mode, Locale::En, true).await;
    let _lost_receipt = rig.surface.hold_card_receipt();
    script_reply(&rig, mode, MODEL_DELAY);
    rig.append(mention("pm", "Can you explain this?", "ask"))
        .await;
    yield_to_workers().await;
    advance(Duration::from_millis(1100)).await;
    advance(Duration::from_secs(31)).await;
    assert_eq!(rig.surface.texts(), [REPLY]);
    advance(Duration::from_secs(3)).await;
    let cards = rig.surface.cards();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].now.state, StatusState::Archived);
    assert_eq!(cards[0].updates, 1);
    assert_eq!(rig.surface.conflicts(), 0);
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn stopping_the_actor_clears_a_visible_busy_card_while_the_runtime_is_live() {
    let mode = SpeakerMode::Intent;
    let rig = rig(mode, Locale::En, true).await;
    script_reply(&rig, mode, MODEL_DELAY);
    rig.append(mention("pm", "Can you explain this?", "ask"))
        .await;
    yield_to_workers().await;
    advance(Duration::from_millis(1100)).await;
    let surface = rig.surface.clone();
    assert_eq!(
        surface
            .card("Speaker request")
            .expect("progress card")
            .now
            .state,
        StatusState::Processing
    );
    rig.stop().await;
    yield_to_workers().await;
    let card = surface.card("Speaker request").expect("progress card");
    assert_eq!(card.now.state, StatusState::Archived);
    assert_eq!(
        card.now.markdown,
        "The Speaker could not finish this attempt."
    );
    assert!(surface.texts().is_empty());
}
