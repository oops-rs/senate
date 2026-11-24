//! Best-effort feedback for a pending natural-language turn in bound chat.

use std::{future::Future, sync::Arc, time::Duration};

use senate_core::{Card, ChatSurface, Event, Locale, PostTarget, StatusState, SurfaceError};
use tokio::sync::oneshot;
use tokio::time::Instant;
use tracing::debug;

use super::{Decision, Engine, Origin, POST_TIMEOUT, STALL_RETRY};
use crate::ids::{content_operation_id, operation_id};

const SHOW_AFTER: Duration = Duration::from_secs(1);

pub(super) enum Finished {
    Processed,
    NeedsConfirmation,
    ModelUnavailable,
    Incomplete,
}

impl Engine {
    pub(super) fn start_reply_progress(&mut self, chat: &Event, locale: Locale) {
        let Some(state) = self.committees.get_mut(&chat.committee) else {
            return;
        };
        if chat.is_simulated()
            || chat
                .thread
                .is_some_and(|root| state.simulated_roots.contains(&root))
        {
            return;
        }
        let platform = self.surface.platform();
        let (Some(source), Some(mirror)) = (&chat.source, &chat.mirror) else {
            return;
        };
        if source.platform != platform || mirror.platform != platform {
            return;
        }

        // This card has its own id and no reply marker. It must never make a
        // retry think the normal reply has already been delivered.
        let key = format!("{}#speaker-reply-progress", source.group_id);
        let Some(epoch) = self.epoch.clone() else {
            return;
        };
        let committee = chat.committee.clone();
        let seq = chat.seq;
        let post_id = operation_id(&epoch, &committee, seq, &key);
        let (finish, completion) = oneshot::channel();
        state.reply_progress = Some(finish);
        let surface = self.surface.clone();
        let target = PostTarget::Reply {
            message_ref: mirror.message_ref.clone(),
            in_thread: true,
        };
        let show_at = Instant::now() + SHOW_AFTER;
        // Keep card I/O off the actor. Once posting starts, let it obtain the
        // receipt even if the turn finishes while the post is in flight.
        tokio::spawn(async move {
            let update_key = format!("{key}#finish");
            let update_id =
                |card: &Card| content_operation_id(&epoch, &committee, seq, &update_key, card);
            show_progress(
                surface, target, locale, show_at, post_id, update_id, completion,
            )
            .await;
        });
    }

    pub(super) fn finish_reply_progress(&mut self, chat: &Event) {
        let Some(state) = self.committees.get_mut(&chat.committee) else {
            return;
        };
        let Some(progress) = state.reply_progress.take() else {
            return;
        };
        let origin = Origin::Bound {
            committee: chat.committee.clone(),
            seq: chat.seq,
        };
        let finished = if matches!(state.decisions.get(&chat.seq), Some(Decision::ModelDown(_))) {
            Finished::ModelUnavailable
        } else if self.pending.iter().any(|pending| pending.origin == origin) {
            Finished::NeedsConfirmation
        } else {
            // Flow::Done does not imply a reply or a handoff was delivered.
            Finished::Processed
        };
        let _ = progress.send(finished);
    }
}

async fn show_progress(
    surface: Arc<dyn ChatSurface>,
    target: PostTarget,
    locale: Locale,
    show_at: Instant,
    post_id: String,
    update_id: impl FnOnce(&Card) -> String,
    mut completion: oneshot::Receiver<Finished>,
) {
    tokio::select! {
        biased;
        _ = &mut completion => return,
        _ = tokio::time::sleep_until(show_at) => {}
    }
    let mut card = Card {
        title: locale.pick("Speaker 请求", "Speaker request").into(),
        state: StatusState::Processing,
        markdown: locale
            .pick(
                "Speaker 正在处理请求。",
                "The Speaker is working on your request.",
            )
            .into(),
        steps: Vec::new(),
        links: Vec::new(),
    };
    let Some(posted) = card_call(&post_id, || {
        surface.post_card(target.clone(), &card, &post_id)
    })
    .await
    else {
        return;
    };

    card.state = StatusState::Archived;
    // A stall or actor shutdown drops the sender. A process exit can stop
    // this worker before the update reaches the surface.
    card.markdown = match completion.await.unwrap_or(Finished::Incomplete) {
        Finished::Processed => {
            locale.pick("Speaker 已结束处理。", "The Speaker finished processing.")
        }
        Finished::NeedsConfirmation => locale.pick(
            "Speaker 正在等待你的确认。",
            "The Speaker is waiting for your confirmation.",
        ),
        Finished::ModelUnavailable => locale.pick(
            "Speaker 的模型暂时不可用。",
            "The Speaker’s model is temporarily unavailable.",
        ),
        Finished::Incomplete => locale.pick(
            "Speaker 暂未完成这次处理。",
            "The Speaker could not finish this attempt.",
        ),
    }
    .into();
    let update_id = update_id(&card);
    let _ = card_call(&update_id, || {
        surface.update_card(&posted.message_ref, &card, &update_id)
    })
    .await;
}

/// Recover a transient failure with the same id, including a lost receipt.
/// A refused operation is final. Neither outcome affects the turn.
async fn card_call<T, F>(operation_id: &str, call: impl Fn() -> F) -> Option<T>
where
    F: Future<Output = Result<T, SurfaceError>>,
{
    for attempt in 0..2 {
        match tokio::time::timeout(POST_TIMEOUT, call()).await {
            Ok(Ok(value)) => return Some(value),
            Ok(Err(SurfaceError::Rejected(_) | SurfaceError::Unsupported(_))) => break,
            Ok(Err(SurfaceError::Unavailable(_))) | Err(_) if attempt == 0 => {
                tokio::time::sleep(STALL_RETRY).await;
            }
            Ok(Err(SurfaceError::Unavailable(_))) | Err(_) => break,
        }
    }
    debug!(
        operation_id,
        "Speaker reply progress card could not be posted or updated"
    );
    None
}
