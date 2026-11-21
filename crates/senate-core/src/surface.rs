//! The chat-platform seam between the Speaker and a gateway.
//!
//! The Speaker is the only voice on the platform: it decides what to say and
//! where; a gateway implements [`ChatSurface`] and only transports. Neither
//! depends on the other; whoever assembles the daemon hands the Speaker an
//! `Arc<dyn ChatSurface>` and the receiving end of an [`UnboundMention`]
//! channel.
//!
//! A surface deals in the platform's own ids (groups, messages, users). The
//! log does not: a gateway turns a platform user into an address before
//! anything is recorded, `senator::<handle>` for a person the roster knows
//! by that account ([`Roster::by_account`](crate::Roster::by_account)) and
//! `guest::<platform>/<id>` for anyone else. Going the other way, the
//! Speaker looks a person's platform ids up in the roster
//! ([`Principal::account_ids`](crate::Principal::account_ids)) when it invites or
//! mentions them.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::address::Address;
use crate::event::StatusState;
use crate::task::{Link, Step};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostTarget {
    /// A new root message in a group (`group_id` is the platform's stable
    /// group identity, as an opaque handle).
    NewRoot { group_id: String },
    /// A reply to a platform message, optionally inside its thread (opening
    /// one if it has none).
    Reply {
        message_ref: String,
        in_thread: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posted {
    pub message_ref: String,
    pub thread_ref: Option<String>,
    /// The platform had already accepted this `operation_id`.
    pub duplicate: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SurfaceError {
    /// The platform refused the post; retrying the same request will not help.
    #[error("rejected: {0}")]
    Rejected(String),
    /// Transient: the gateway or platform is unreachable; retry later.
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// This surface (or its grants) cannot do it at all. The Speaker falls
    /// back: adopt instead of create, a plain post instead of a card, the
    /// person's name as text instead of a native mention.
    #[error("unsupported: {0}")]
    Unsupported(String),
}

/// A task card, platform-neutral. The gateway renders it. Its texts are
/// already in the committee's language when it reaches the surface. Its
/// serde form is the `card` of the chat gateway protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Card {
    pub title: String,
    pub state: StatusState,
    /// Body under the title: who asked, the triage line, the latest answer.
    pub markdown: String,
    pub steps: Vec<Step>,
    pub links: Vec<Link>,
}

/// A group for the Speaker to create.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewGroup {
    pub name: String,
    /// Platform user ids to invite: people's accounts on this surface's
    /// platform.
    pub invite: Vec<String>,
}

/// A group the surface created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatedGroup {
    pub group_id: String,
    /// Invitees the platform did not add (an app without the permission to
    /// add members, or ids it does not know). The group exists regardless.
    #[serde(default)]
    pub not_added: Vec<String>,
}

impl CreatedGroup {
    pub fn new(group_id: impl Into<String>) -> Self {
        Self {
            group_id: group_id.into(),
            not_added: Vec::new(),
        }
    }
}

/// The Speaker's own section of a group's announcement: a title and its
/// markdown, in the committee's language. A surface keeps it apart from the
/// rest of the announcement (how is the surface's own business), so setting
/// it never touches what people wrote there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnouncementSection {
    pub title: String,
    pub markdown: String,
}

/// A group the surface was granted at runtime by
/// [`adopt_group`](ChatSurface::adopt_group).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptedGroup {
    /// The group's identity, unchanged: the `group_id` its messages carry.
    pub group_id: String,
    /// The surface already had it (configured, created, or adopted before).
    pub already_granted: bool,
}

fn unsupported<T>(what: &str) -> Result<T, SurfaceError> {
    Err(SurfaceError::Unsupported(what.to_string()))
}

#[async_trait]
pub trait ChatSurface: Send + Sync + 'static {
    /// The platform's name: the `platform` recorded on mirrors and sources,
    /// in people's [`Account`](crate::Account)s, and in the addresses of
    /// guests who arrive here. A slug (`[a-z0-9-]`), since it is part of
    /// `guest::<platform>/<id>`.
    fn platform(&self) -> &str;

    /// Post markdown. `operation_id` is an idempotency key: replaying it
    /// returns the first post with `duplicate: true`.
    async fn post(
        &self,
        target: PostTarget,
        markdown: &str,
        operation_id: &str,
    ) -> Result<Posted, SurfaceError>;

    /// [`post`](Self::post) with a native mention of `user` (a platform user
    /// id) at the start.
    async fn post_mentioning(
        &self,
        _target: PostTarget,
        _markdown: &str,
        _user: &str,
        _operation_id: &str,
    ) -> Result<Posted, SurfaceError> {
        unsupported("mentions")
    }

    /// Post a card that [`update_card`](Self::update_card) can change in place.
    async fn post_card(
        &self,
        _target: PostTarget,
        _card: &Card,
        _operation_id: &str,
    ) -> Result<Posted, SurfaceError> {
        unsupported("cards")
    }

    /// Replace a posted card's content. `message_ref` is what
    /// [`post_card`](Self::post_card) returned. Updates of one card are
    /// applied in call order; the surface handles any platform revision.
    async fn update_card(
        &self,
        _message_ref: &str,
        _card: &Card,
        _operation_id: &str,
    ) -> Result<(), SurfaceError> {
        unsupported("card updates")
    }

    /// Pin a message in its group.
    async fn pin(&self, _message_ref: &str, _operation_id: &str) -> Result<(), SurfaceError> {
        unsupported("pins")
    }

    /// Create a group with the bot as owner, and return its `group_id`
    /// (usable in [`PostTarget::NewRoot`], and in bindings) with whoever
    /// could not be added.
    async fn create_group(
        &self,
        _group: &NewGroup,
        _operation_id: &str,
    ) -> Result<CreatedGroup, SurfaceError> {
        unsupported("group creation")
    }

    /// Invite platform users to a group. Already-members are not an error.
    async fn invite(
        &self,
        _group_id: &str,
        _users: &[String],
        _operation_id: &str,
    ) -> Result<(), SurfaceError> {
        unsupported("invites")
    }

    /// Rename a group (to mark it archived, say).
    async fn rename_group(
        &self,
        _group_id: &str,
        _name: &str,
        _operation_id: &str,
    ) -> Result<(), SurfaceError> {
        unsupported("renames")
    }

    /// Replace the Speaker's section of `group_id`'s announcement with
    /// `section`, adding it if the announcement has none. The rest of the
    /// announcement is left as it is. `Unsupported` when the platform has no
    /// announcements or the bot may not edit this group's.
    async fn set_announcement(
        &self,
        _group_id: &str,
        _section: &AnnouncementSection,
        _operation_id: &str,
    ) -> Result<(), SurfaceError> {
        unsupported("announcements")
    }

    /// Ask the platform for the full use of an existing group (unmentioned
    /// chat, proactive posts) because a person bound it. The group is named
    /// by `message_ref`, a message the platform delivered from it; it keeps
    /// its `group_id`. Idempotent: adopting a group the surface already has
    /// is not an error.
    async fn adopt_group(&self, _message_ref: &str) -> Result<AdoptedGroup, SurfaceError> {
        unsupported("group adoption")
    }

    /// Give back what [`adopt_group`](Self::adopt_group) (or
    /// [`create_group`](Self::create_group)) was granted, once the group is
    /// bound to nothing. Returns whether a grant was removed; releasing a
    /// group with none is not an error.
    async fn release_group(&self, _group_id: &str) -> Result<bool, SurfaceError> {
        unsupported("group release")
    }
}

/// The Speaker was mentioned (or a Speaker post was replied to) in a group
/// bound to no committee. There is no log to record it in yet, so the
/// gateway hands it to the Speaker directly; binding the group is the usual
/// reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnboundMention {
    pub platform: String,
    pub group_id: String,
    pub group_name: Option<String>,
    pub message_ref: String,
    pub thread_ref: Option<String>,
    /// The message this one replies to (the Speaker's own request for
    /// confirmation, say), and the root of the thread holding it.
    pub reply_to_message_ref: Option<String>,
    pub thread_root_message_ref: Option<String>,
    /// Why the platform delivered it, lowercase (`mention`, `reply`, ...).
    pub addressing: String,
    /// `senator::<handle>` for a person on the roster, `guest::<platform>/<id>`
    /// for anyone else. Binding a group changes state, so the Speaker acts
    /// only for a sender that [`may_change_state`](Address::may_change_state).
    pub sender: Address,
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Plain;

    #[async_trait]
    impl ChatSurface for Plain {
        fn platform(&self) -> &str {
            "chat"
        }

        async fn post(
            &self,
            _target: PostTarget,
            _markdown: &str,
            operation_id: &str,
        ) -> Result<Posted, SurfaceError> {
            Ok(Posted {
                message_ref: format!("m-{operation_id}"),
                thread_ref: None,
                duplicate: false,
            })
        }
    }

    #[tokio::test]
    async fn a_surface_that_only_posts_reports_the_rest_as_unsupported() {
        let surface: std::sync::Arc<dyn ChatSurface> = std::sync::Arc::new(Plain);
        let target = PostTarget::NewRoot {
            group_id: "backend".into(),
        };
        let posted = surface.post(target.clone(), "hi", "op1").await.unwrap();
        assert_eq!(posted.message_ref, "m-op1");

        let card = Card {
            title: "t".into(),
            state: StatusState::Processing,
            markdown: String::new(),
            steps: Vec::new(),
            links: Vec::new(),
        };
        assert!(matches!(
            surface.post_card(target.clone(), &card, "op2").await,
            Err(SurfaceError::Unsupported(_))
        ));
        assert!(matches!(
            surface.post_mentioning(target, "hi", "u_1", "op3").await,
            Err(SurfaceError::Unsupported(_))
        ));
        let section = AnnouncementSection {
            title: "t".into(),
            markdown: String::new(),
        };
        assert!(matches!(
            surface.set_announcement("backend", &section, "op4").await,
            Err(SurfaceError::Unsupported(_))
        ));
        assert!(matches!(
            surface.adopt_group("m-1").await,
            Err(SurfaceError::Unsupported(_))
        ));
        assert!(matches!(
            surface.release_group("backend").await,
            Err(SurfaceError::Unsupported(_))
        ));
    }

    #[test]
    fn an_unbound_mention_from_a_guest_cannot_bind() {
        let mention = |sender: &str| UnboundMention {
            platform: "chat".into(),
            group_id: "backend".into(),
            group_name: None,
            message_ref: "m-1".into(),
            thread_ref: None,
            reply_to_message_ref: None,
            thread_root_message_ref: None,
            addressing: "mention".into(),
            sender: sender.parse().unwrap(),
            text: "bind refund-v2".into(),
        };
        assert!(mention("senator::alice").sender.may_change_state());
        assert!(!mention("guest::chat/u_8f3a").sender.may_change_state());
    }
}
