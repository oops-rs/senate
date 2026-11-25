//! A direct discussion has one root in each committee. The first receipt
//! pins the chair. Recreating the copy from a stored receipt gives it the
//! same id even after a timeout, a server restart, or a change of chair.

use senate_core::{
    Address, Appended, CommitteeId, Event, EventId, Hub, Kind, Locator, NewEvent, Origin, Roster,
    Seq, meta,
};
use serde_json::Value;

use super::ApiFailure;
use crate::hub::ServerHub;

pub(super) fn can_start(roster: &Roster, event: &NewEvent) -> bool {
    event.thread.is_none()
        && matches!(event.kind, Kind::Question | Kind::Note)
        && event.on_behalf_of.is_none()
        && discussion_meta_only(event, meta::INTENDED)
        && event.meta.contains_key(meta::INTENDED)
        && event.from.as_senator().is_some_and(|s| roster.is_agent(s))
}

pub(super) async fn start(
    hub: &ServerHub,
    event: NewEvent,
    target: CommitteeId,
) -> Result<Appended, ApiFailure> {
    let chair =
        hub.chair(&target).await?.chair.ok_or_else(|| {
            ApiFailure::conflict(format!("{target} has no chair to receive this"))
        })?;
    let pinned = Locator::at(hub.senate_path(), target.clone(), Address::Senator(chair));
    let receipt = append_receipt(hub, event, pinned.to_string().into()).await?;
    let recorded = pinned_chair(&receipt.event)
        .filter(|pinned| pinned.committee() == Some(&target))
        .ok_or_else(id_conflict)?;
    append_copy(hub, &receipt.event, &target, recorded.address(), None).await?;
    Ok(receipt)
}

fn pinned_chair(root: &Event) -> Option<Locator> {
    if !root.is_chair_discussion() {
        return None;
    }
    let intended = senate_core::locator::intended(root)?;
    let pinned: Locator = root
        .meta
        .get(meta::CHAIR_DISCUSSION)?
        .as_str()?
        .parse()
        .ok()?;
    (pinned.is_complete()
        && intended.address() == &Address::Chair
        && pinned.committee() == intended.committee()
        && pinned.committee() != Some(&root.committee)
        && pinned.address().as_senator().is_some())
    .then_some(pinned)
}

pub(super) struct ReplyPeer {
    committee: CommitteeId,
    recipient: Address,
    thread: Seq,
}

/// A numbered thread can be read by others, but only the two participants
/// may send a copy to the other committee. Each side learns its peer from
/// the root stored on its own committee.
pub(super) async fn reply_peer(
    hub: &ServerHub,
    event: &NewEvent,
) -> Result<Option<ReplyPeer>, ApiFailure> {
    let Some(thread) = event.thread else {
        return Ok(None);
    };
    if event.on_behalf_of.is_some()
        || !matches!(event.kind, Kind::Question | Kind::Note | Kind::Answer)
    {
        return Ok(None);
    }
    let Some(seen) = reply_read_mark(event) else {
        return Ok(None);
    };
    let Some(root) = hub
        .get(&event.committee, thread)
        .await?
        .filter(|r| r.thread.is_none() && r.is_chair_discussion())
    else {
        return Ok(None);
    };
    if seen != Seq::ZERO
        && seen != thread
        && (seen < thread
            || !hub
                .get(&event.committee, seen)
                .await?
                .is_some_and(|read| read.thread == Some(thread)))
    {
        return Ok(None);
    }
    if root.from == event.from
        && let Some(pinned) = pinned_chair(&root)
        && let Some(committee) = pinned.committee()
    {
        // Reuse the copy's id to find the root and finish any interrupted
        // delivery. The store indexes ids within a committee.
        let copy = append_copy(hub, &root, committee, pinned.address(), None).await?;
        return Ok(Some(ReplyPeer {
            committee: copy.committee,
            recipient: pinned.address().clone(),
            thread: copy.seq,
        }));
    }
    if root.from == Address::Committee
        && root.to.as_slice() == std::slice::from_ref(&event.from)
        && let Some(origin) = Origin::of(&root)
        && let Some(committee) = origin.locator.committee()
    {
        return Ok(Some(ReplyPeer {
            committee: committee.clone(),
            recipient: origin.locator.address().clone(),
            thread: origin.thread,
        }));
    }
    Ok(None)
}

fn reply_read_mark(event: &NewEvent) -> Option<Seq> {
    if !discussion_meta_only(event, meta::SEEN) {
        return None;
    }
    match event.meta.get(meta::SEEN) {
        None => Some(Seq::ZERO),
        Some(seen) => {
            // Both SQL backends store sequence numbers as signed 64-bit integers.
            let seen = seen.as_i64()?;
            u64::try_from(seen).ok().map(Seq)
        }
    }
}

fn discussion_meta_only(event: &NewEvent, key: &str) -> bool {
    // The HTTP append guard restricts these keys to the verified local bridge.
    event.meta.keys().all(|candidate| {
        candidate == key || crate::rules::BRIDGE_KEYS.contains(&candidate.as_str())
    })
}

pub(super) async fn reply(
    hub: &ServerHub,
    event: NewEvent,
    peer: ReplyPeer,
) -> Result<Appended, ApiFailure> {
    let intended = Locator::at(
        hub.senate_path(),
        peer.committee.clone(),
        peer.recipient.clone(),
    );
    let event = event.with_meta(meta::INTENDED, intended.to_string().into());
    let receipt = append_receipt(hub, event, Value::Bool(true)).await?;
    append_copy(
        hub,
        &receipt.event,
        &peer.committee,
        &peer.recipient,
        Some(peer.thread),
    )
    .await?;
    Ok(receipt)
}

fn id_conflict() -> ApiFailure {
    ApiFailure::conflict("event id was already used outside this discussion")
}

async fn append_receipt(
    hub: &ServerHub,
    mut event: NewEvent,
    marker: Value,
) -> Result<Appended, ApiFailure> {
    let author = event.from.clone();
    let kind = event.kind.clone();
    let thread = event.thread;
    event.to.clear();
    let receipt = hub
        .append(event.with_meta(meta::CHAIR_DISCUSSION, marker))
        .await?;
    if receipt.duplicate
        && (receipt.event.from != author
            || receipt.event.kind != kind
            || receipt.event.thread != thread
            || !receipt.event.is_chair_discussion())
    {
        return Err(id_conflict());
    }
    Ok(receipt)
}

async fn append_copy(
    hub: &ServerHub,
    source: &Event,
    committee: &CommitteeId,
    recipient: &Address,
    thread: Option<Seq>,
) -> Result<Event, ApiFailure> {
    let origin = Origin {
        locator: Locator::at(
            hub.senate_path(),
            source.committee.clone(),
            source.from.clone(),
        ),
        thread: source.thread.unwrap_or(source.seq),
    };
    let mut copy = NewEvent::new(
        committee.clone(),
        source.kind.clone(),
        Address::Committee,
        &source.body,
    )
    .with_id(EventId::derived(
        u64::try_from(source.ts.timestamp_millis()).unwrap_or_default(),
        &format!("chair-discussion:{}:{}", source.committee, source.id),
    ))
    .on_behalf_of(source.from.clone())
    .to(recipient.clone())
    .with_meta(meta::CHAIR_DISCUSSION, Value::Bool(true))
    .with_meta(meta::ORIGIN, origin.value());
    copy.thread = thread;
    // Only discussion content crosses. In particular a client's metadata
    // never becomes a control field on an event written by the committee.
    copy.refs.clone_from(&source.refs);
    let appended = hub.append(copy).await?;
    let event = appended.event;
    if appended.duplicate
        && (event.from != Address::Committee
            || event.kind != source.kind
            || event.thread != thread
            || event.to.as_slice() != std::slice::from_ref(recipient)
            || event.on_behalf_of.as_ref() != Some(&source.from)
            || !event.is_chair_discussion()
            || !Origin::of(&event).is_some_and(|recorded| {
                recorded.locator.committee() == Some(&source.committee)
                    && recorded.locator.address() == &source.from
                    && recorded.thread == origin.thread
            }))
    {
        return Err(id_conflict());
    }
    Ok(event)
}
