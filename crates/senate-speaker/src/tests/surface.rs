//! A [`ChatSurface`] that records posts, mints refs, and honours
//! `operation_id` idempotency the way a gateway does: a replay of the same
//! post is a duplicate, the same key with other content is a conflict.
//! Platform-neutral: its platform is `chat`, its refs are opaque.
//!
//! By default it can only post, like the plainest gateway: mentions, cards,
//! pins and group management are [`SurfaceError::Unsupported`]. [`Caps`]
//! turns each on.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use senate_core::{Card, ChatSurface, NewGroup, PostTarget, Posted, SurfaceError};

/// The platform this surface reports, in mirrors, sources and accounts.
pub(crate) const PLATFORM: &str = "chat";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Post {
    pub(crate) target: PostTarget,
    pub(crate) markdown: String,
    pub(crate) operation_id: String,
    pub(crate) message_ref: String,
    pub(crate) thread_ref: Option<String>,
    /// The platform user natively mentioned, for `post_mentioning`.
    pub(crate) mention: Option<String>,
}

/// A card as posted, and as it stands after its updates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CardPost {
    pub(crate) target: PostTarget,
    pub(crate) message_ref: String,
    pub(crate) operation_id: String,
    pub(crate) first: Card,
    pub(crate) now: Card,
    /// Updates applied (replays excluded).
    pub(crate) updates: usize,
}

/// What the surface can do beyond plain posts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Caps {
    pub(crate) mentions: bool,
    pub(crate) cards: bool,
    pub(crate) pins: bool,
    /// Create, invite, rename.
    pub(crate) groups: bool,
    /// Add members when creating a group (an app may create chats but not
    /// add people to them).
    pub(crate) members: bool,
}

impl Caps {
    pub(crate) const ALL: Self = Self {
        mentions: true,
        cards: true,
        pins: true,
        groups: true,
        members: true,
    };

    /// Everything but group management: an Agent group is adopted.
    pub(crate) const ADOPTED: Self = Self {
        groups: false,
        ..Self::ALL
    };
}

enum Fault {
    Fail(SurfaceError),
    /// Accept the call and never answer.
    Hang,
    /// A bug in the surface, as far as the caller can tell.
    Panic,
}

#[derive(Default)]
struct State {
    caps: Caps,
    posts: Vec<Post>,
    /// operation id → (what was posted, where, the answer).
    operations: HashMap<String, (PostTarget, String, Posted)>,
    /// message ref → the thread it is in (or opened by replying in thread).
    threads: HashMap<String, String>,
    duplicates: usize,
    conflicts: usize,
    next: u64,
    faults: VecDeque<Fault>,
    invite_faults: VecDeque<SurfaceError>,
    invite_attempts: Vec<String>,
    cards: Vec<CardPost>,
    card_operations: HashMap<String, (PostTarget, Card, Posted)>,
    update_operations: HashSet<String>,
    pins: Vec<String>,
    /// operation id → (the group asked for, its id).
    groups: Vec<(String, NewGroup, String)>,
    invites: Vec<(String, Vec<String>)>,
    invite_operations: HashSet<String>,
    renames: Vec<(String, String)>,
}

#[derive(Default)]
pub(crate) struct FakeSurface {
    state: Mutex<State>,
}

fn unsupported<T>(what: &str) -> Result<T, SurfaceError> {
    Err(SurfaceError::Unsupported(format!(
        "fake surface: no {what}"
    )))
}

impl FakeSurface {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn with(caps: Caps) -> Arc<Self> {
        let surface = Self::default();
        surface.lock().caps = caps;
        Arc::new(surface)
    }

    pub(crate) fn set_caps(&self, caps: Caps) {
        self.lock().caps = caps;
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Every distinct post (replays excluded).
    pub(crate) fn posts(&self) -> Vec<Post> {
        self.lock().posts.clone()
    }

    pub(crate) fn texts(&self) -> Vec<String> {
        self.posts().into_iter().map(|p| p.markdown).collect()
    }

    /// Replays the surface answered with `duplicate: true`.
    pub(crate) fn duplicates(&self) -> usize {
        self.lock().duplicates
    }

    /// Posts refused because their key was already used for other content.
    pub(crate) fn conflicts(&self) -> usize {
        self.lock().conflicts
    }

    /// Every card posted, as it stands now.
    pub(crate) fn cards(&self) -> Vec<CardPost> {
        self.lock().cards.clone()
    }

    /// The card titled `title`, as it stands now.
    pub(crate) fn card(&self, title: &str) -> Option<CardPost> {
        self.cards()
            .into_iter()
            .find(|c| c.first.title == title || c.now.title == title)
    }

    pub(crate) fn pins(&self) -> Vec<String> {
        self.lock().pins.clone()
    }

    /// Groups created: (the request, its id).
    pub(crate) fn groups(&self) -> Vec<(NewGroup, String)> {
        self.lock()
            .groups
            .iter()
            .map(|(_, g, id)| (g.clone(), id.clone()))
            .collect()
    }

    pub(crate) fn invites(&self) -> Vec<(String, Vec<String>)> {
        self.lock().invites.clone()
    }

    pub(crate) fn renames(&self) -> Vec<(String, String)> {
        self.lock().renames.clone()
    }

    pub(crate) fn fail_next_invite(&self, error: SurfaceError) {
        self.lock().invite_faults.push_back(error);
    }

    pub(crate) fn invite_attempts(&self) -> Vec<String> {
        self.lock().invite_attempts.clone()
    }

    pub(crate) fn fail_next(&self, error: SurfaceError) {
        self.lock().faults.push_back(Fault::Fail(error));
    }

    pub(crate) fn hang_next(&self) {
        self.lock().faults.push_back(Fault::Hang);
    }

    pub(crate) fn panic_next(&self) {
        self.lock().faults.push_back(Fault::Panic);
    }

    fn mint(state: &mut State, target: &PostTarget) -> Posted {
        state.next += 1;
        let message_ref = format!("m_s{}", state.next);
        let thread_ref = match target {
            PostTarget::NewRoot { .. } => None,
            PostTarget::Reply {
                message_ref: parent,
                in_thread: true,
            } => {
                let thread = state
                    .threads
                    .get(parent)
                    .cloned()
                    .unwrap_or_else(|| format!("omt_{parent}"));
                state.threads.insert(parent.clone(), thread.clone());
                Some(thread)
            }
            PostTarget::Reply { .. } => None,
        };
        if let Some(thread) = &thread_ref {
            state.threads.insert(message_ref.clone(), thread.clone());
        }
        Posted {
            message_ref,
            thread_ref,
            duplicate: false,
        }
    }

    async fn post_inner(
        &self,
        target: PostTarget,
        markdown: &str,
        operation_id: &str,
        mention: Option<&str>,
    ) -> Result<Posted, SurfaceError> {
        let fault = self.lock().faults.pop_front();
        match fault {
            Some(Fault::Fail(error)) => return Err(error),
            Some(Fault::Hang) => std::future::pending::<()>().await,
            Some(Fault::Panic) => panic!("fake surface: post panics on request"),
            None => {}
        }
        let mut state = self.lock();
        let content = match mention {
            Some(user) => format!("<at {user}>{markdown}"),
            None => markdown.to_owned(),
        };
        if let Some((first_target, first_content, first)) =
            state.operations.get(operation_id).cloned()
        {
            if first_target != target || first_content != content {
                state.conflicts += 1;
                return Err(SurfaceError::Rejected(format!(
                    "operation_conflict: {operation_id}"
                )));
            }
            state.duplicates += 1;
            return Ok(Posted {
                duplicate: true,
                ..first
            });
        }
        let posted = Self::mint(&mut state, &target);
        state.operations.insert(
            operation_id.to_owned(),
            (target.clone(), content, posted.clone()),
        );
        state.posts.push(Post {
            target,
            markdown: markdown.to_owned(),
            operation_id: operation_id.to_owned(),
            message_ref: posted.message_ref.clone(),
            thread_ref: posted.thread_ref.clone(),
            mention: mention.map(str::to_owned),
        });
        Ok(posted)
    }
}

#[async_trait]
impl ChatSurface for FakeSurface {
    fn platform(&self) -> &str {
        PLATFORM
    }

    async fn post(
        &self,
        target: PostTarget,
        markdown: &str,
        operation_id: &str,
    ) -> Result<Posted, SurfaceError> {
        self.post_inner(target, markdown, operation_id, None).await
    }

    async fn post_mentioning(
        &self,
        target: PostTarget,
        markdown: &str,
        user: &str,
        operation_id: &str,
    ) -> Result<Posted, SurfaceError> {
        if !self.lock().caps.mentions {
            return unsupported("mentions");
        }
        self.post_inner(target, markdown, operation_id, Some(user))
            .await
    }

    async fn post_card(
        &self,
        target: PostTarget,
        card: &Card,
        operation_id: &str,
    ) -> Result<Posted, SurfaceError> {
        let mut state = self.lock();
        if !state.caps.cards {
            return unsupported("cards");
        }
        if let Some((first_target, first_card, first)) =
            state.card_operations.get(operation_id).cloned()
        {
            if first_target != target || first_card != *card {
                state.conflicts += 1;
                return Err(SurfaceError::Rejected(format!(
                    "operation_conflict: {operation_id}"
                )));
            }
            state.duplicates += 1;
            return Ok(Posted {
                duplicate: true,
                ..first
            });
        }
        let posted = Self::mint(&mut state, &target);
        state.card_operations.insert(
            operation_id.to_owned(),
            (target.clone(), card.clone(), posted.clone()),
        );
        state.cards.push(CardPost {
            target,
            message_ref: posted.message_ref.clone(),
            operation_id: operation_id.to_owned(),
            first: card.clone(),
            now: card.clone(),
            updates: 0,
        });
        Ok(posted)
    }

    async fn update_card(
        &self,
        message_ref: &str,
        card: &Card,
        operation_id: &str,
    ) -> Result<(), SurfaceError> {
        let mut state = self.lock();
        if !state.caps.cards {
            return unsupported("card updates");
        }
        if !state.update_operations.insert(operation_id.to_owned()) {
            state.duplicates += 1;
            return Ok(());
        }
        let Some(posted) = state
            .cards
            .iter_mut()
            .find(|c| c.message_ref == message_ref)
        else {
            return Err(SurfaceError::Rejected(format!("no card {message_ref}")));
        };
        posted.now = card.clone();
        posted.updates += 1;
        Ok(())
    }

    async fn pin(&self, message_ref: &str, _operation_id: &str) -> Result<(), SurfaceError> {
        let mut state = self.lock();
        if !state.caps.pins {
            return unsupported("pins");
        }
        if !state.pins.iter().any(|p| p == message_ref) {
            state.pins.push(message_ref.to_owned());
        }
        Ok(())
    }

    async fn create_group(
        &self,
        group: &NewGroup,
        operation_id: &str,
    ) -> Result<senate_core::CreatedGroup, SurfaceError> {
        let mut state = self.lock();
        if !state.caps.groups {
            return unsupported("group creation");
        }
        let not_added = if state.caps.members {
            Vec::new()
        } else {
            group.invite.clone()
        };
        if let Some((_, _, id)) = state.groups.iter().find(|(op, _, _)| op == operation_id) {
            return Ok(senate_core::CreatedGroup {
                group_id: id.clone(),
                not_added,
            });
        }
        let id = format!("g_agents_{}", state.groups.len() + 1);
        state
            .groups
            .push((operation_id.to_owned(), group.clone(), id.clone()));
        Ok(senate_core::CreatedGroup {
            group_id: id,
            not_added,
        })
    }

    async fn invite(
        &self,
        group_id: &str,
        users: &[String],
        operation_id: &str,
    ) -> Result<(), SurfaceError> {
        let mut state = self.lock();
        state.invite_attempts.push(operation_id.to_owned());
        if let Some(error) = state.invite_faults.pop_front() {
            return Err(error);
        }
        if !state.caps.groups {
            return unsupported("invites");
        }
        if state.invite_operations.insert(operation_id.to_owned()) {
            state.invites.push((group_id.to_owned(), users.to_vec()));
        }
        Ok(())
    }

    async fn rename_group(
        &self,
        group_id: &str,
        name: &str,
        _operation_id: &str,
    ) -> Result<(), SurfaceError> {
        let mut state = self.lock();
        if !state.caps.groups {
            return unsupported("renames");
        }
        state.renames.push((group_id.to_owned(), name.to_owned()));
        Ok(())
    }
}
