//! Deterministic identities, so every append and post the Speaker makes is
//! idempotent across retries and restarts.

use senate_core::{CommitteeId, EventId, Seq};
use sha2::{Digest, Sha256};

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The longest `operation_id` a chat gateway is held to, in bytes. The
/// strictest gateway in use caps it at 64.
const OPERATION_ID_MAX: usize = 64;
const OPERATION_PREFIX: &str = "senate:";
/// Hex digits of the hashed form: 192 bits.
const OPERATION_HASH_HEX: usize = 48;

/// An [`EventId`] that is a pure function of `parts`: the first 128 bits of
/// a SHA-256 over the NUL-separated parts, rendered as a ULID. Re-deciding
/// the same thing after a crash re-appends the same id, which the log stores
/// once.
pub(crate) fn derived_event_id(parts: &[&str]) -> EventId {
    let mut hasher = Sha256::new();
    hasher.update(b"senate-speaker\0");
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    let value = u128::from_be_bytes(bytes);
    let text: String = (0..26)
        .map(|i| CROCKFORD[((value >> (125 - 5 * i)) & 0x1f) as usize] as char)
        .collect();
    // Cannot fail: a ULID's alphabet and length by construction (tested).
    text.parse().unwrap_or_default()
}

/// The gateway's idempotency key for the copy of log event `seq` posted to `target`
/// (the group posted into; see [`senate_core::MirrorRef`]):
/// `senate:<epoch>:<committee>:<seq>:<target>`.
///
/// The epoch is what keeps a reset log (seq starting over at 1) from reusing
/// a key the gateway already holds, which it would refuse as a conflict.
/// The key is capped at 64 bytes, so a longer one is sent as `senate:` plus
/// 48 hex digits of its SHA-256; that form has no further `:`, so it never
/// equals a verbatim one.
///
/// Public because the chat gateway derives it too: a new root whose key is
/// that of an event descended from a simulated message (`senate chat send`)
/// is kept off the platform with the rest of that conversation.
pub fn operation_id(epoch: &str, committee: &CommitteeId, seq: Seq, target: &str) -> String {
    let full = format!("{OPERATION_PREFIX}{epoch}:{committee}:{seq}:{target}");
    if full.len() <= OPERATION_ID_MAX {
        return full;
    }
    let digest = hex::encode(Sha256::digest(full.as_bytes()));
    format!("{OPERATION_PREFIX}{}", &digest[..OPERATION_HASH_HEX])
}

/// The key for an update that can repeat at the same `seq`: a card
/// refreshed in place, an announcement set again. What it sends is part of
/// the key ([`operation_id`] with `target~<hash of payload>`), because the
/// gateway holds an id to one payload: the same content again (a retry,
/// or a restart that renders what it sent before) reuses the id, and
/// anything else gets a new one.
pub(crate) fn content_operation_id<T: serde::Serialize>(
    epoch: &str,
    committee: &CommitteeId,
    seq: Seq,
    target: &str,
    payload: &T,
) -> String {
    // Cannot fail: cards and sections are plain strings and enums.
    let bytes = serde_json::to_vec(payload).unwrap_or_default();
    let digest = hex::encode(Sha256::digest(&bytes));
    operation_id(
        epoch,
        committee,
        seq,
        &format!("{target}~{}", &digest[..12]),
    )
}

/// The key for a reply in a group bound to no committee: there is no seq, so
/// it is derived from the message being answered.
pub(crate) fn unbound_operation_id(group_id: &str, message_ref: &str) -> String {
    let digest = hex::encode(Sha256::digest(
        format!("{group_id}\0{message_ref}").as_bytes(),
    ));
    format!("senate:unbound:{}", &digest[..20])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_content_key_follows_what_is_sent() {
        let committee: CommitteeId = "refund-v2".parse().expect("id");
        let key = |text: &str| content_operation_id("e1", &committee, Seq(9), "g#waiting", &text);
        assert_eq!(key("a"), key("a"), "a retry reuses its id");
        assert_ne!(key("a"), key("b"), "new content, new id");
        assert_ne!(
            key("a"),
            content_operation_id("e1", &committee, Seq(10), "g#waiting", &"a"),
            "the same content at a later seq is another update"
        );
        assert!(key("a").len() <= OPERATION_ID_MAX);
    }

    #[test]
    fn derived_ids_are_stable_valid_ulids() {
        let a = derived_event_id(&["relay", "refund-v2", "7"]);
        assert_eq!(a, derived_event_id(&["relay", "refund-v2", "7"]));
        assert_ne!(a, derived_event_id(&["relay", "refund-v2", "8"]));
        let text = a.to_string();
        assert_eq!(text.len(), 26);
        assert_eq!(text.parse::<EventId>().ok(), Some(a));
    }

    #[test]
    fn operation_ids_name_the_epoch_and_the_target_and_fit_the_gateway() {
        let committee = CommitteeId::new("refund-v2").expect("committee");
        assert_eq!(
            operation_id("e1", &committee, Seq(120), "backend"),
            "senate:e1:refund-v2:120:backend"
        );
        // A reset log reuses the seq, never the key.
        assert_ne!(
            operation_id("e1", &committee, Seq(1), "backend"),
            operation_id("e2", &committee, Seq(1), "backend")
        );
        // A second copy of one event gets its own key.
        assert_ne!(
            operation_id("e1", &committee, Seq(1), "backend"),
            operation_id("e1", &committee, Seq(1), "backend#ack")
        );
        // Long parts are hashed down to the limit, still deterministic.
        let epoch = "01JBZ6V3K8X5Q2W9R7T4Y1M0N6";
        let alias = format!("g-{}", "f".repeat(60));
        let long = operation_id(epoch, &committee, Seq(118), &alias);
        assert!(long.len() <= OPERATION_ID_MAX, "{long}");
        assert!(long.starts_with("senate:"));
        assert_eq!(long, operation_id(epoch, &committee, Seq(118), &alias));
        assert_ne!(long, operation_id(epoch, &committee, Seq(119), &alias));
        assert_ne!(
            unbound_operation_id("backend", "m-1"),
            unbound_operation_id("backend", "m-2")
        );
    }
}
