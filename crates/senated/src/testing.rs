//! A chat platform in memory, for end-to-end tests of an assembled daemon.
//!
//! [`FakeChat`] is both halves of a chat layer at once:
//!
//! - the platform the Speaker speaks on ([`ChatSurface`]): it records every
//!   post, card and card update, pin, group created, invite, rename, join
//!   and leave, mints message and thread refs, and honours `operation_id`
//!   idempotency as a platform does;
//! - the people in it: [`FakeChat::deliver`] has someone say something in a
//!   group, mentioning the bot or not, in a thread or not, and hands it to
//!   a real [`ChatGateway`], which writes the `chat` event or the unbound
//!   mention a real gateway would.
//!
//! Its platform is `chat`. Senders are the platform's user ids: a person is
//! known by an account `{ platform = "chat", id = … }` on the roster, and
//! anyone else arrives as `guest::chat/<id>`.
//!
//! What the platform can do is switchable ([`Caps`]), like a platform app
//! with or without a permission. By default it posts, mentions, shows and
//! pins cards ([`Caps::PLAIN`]). Group creation, membership and announcement
//! edits are opt-in.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

use async_trait::async_trait;
use senate_core::{
    AdoptedGroup, AnnouncementSection, Card, ChatSurface, CreatedGroup, NewGroup, PostTarget,
    Posted, SurfaceError,
};

use crate::chat::{Addressing, ChatGateway, Delivered, GatewayError, Inbound, PlatformMention};

/// Start the test daemon on a listener its fixture keeps across restarts.
pub async fn start_on_listener(
    assembly: crate::Assembly,
    listener: tokio::net::TcpListener,
) -> Result<crate::Senated, crate::AssemblyError> {
    assembly.start_with_listener(Some(listener)).await
}

/// The platform name.
pub const PLATFORM: &str = "chat";

/// What the platform lets the bot do beyond plain posts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub mentions: bool,
    pub cards: bool,
    pub pins: bool,
    pub announcements: bool,
    /// Create groups, invite, rename.
    pub groups: bool,
    /// Add people when creating a group.
    pub members: bool,
    /// Adopt and release existing groups (bind = join).
    pub membership: bool,
}

impl Caps {
    pub const ALL: Self = Self {
        mentions: true,
        cards: true,
        pins: true,
        announcements: true,
        groups: true,
        members: true,
        membership: true,
    };

    /// Posts, mentions, cards and pins; no group creation (an Agent group is
    /// adopted with a command) and no joining (every group's chat arrives).
    pub const PLAIN: Self = Self {
        announcements: false,
        groups: false,
        membership: false,
        ..Self::ALL
    };
}

impl Default for Caps {
    fn default() -> Self {
        Self::PLAIN
    }
}

/// Where a post went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostedTo {
    Root {
        group: String,
    },
    Reply {
        message_ref: String,
        in_thread: bool,
    },
}

/// One post the bot made: a message, or a card (whose `markdown` is its
/// title and body as first posted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Post {
    pub to: PostedTo,
    /// The group it landed in, when the platform knows it.
    pub group: Option<String>,
    pub markdown: String,
    pub message_ref: String,
    pub thread_ref: Option<String>,
    /// The platform user natively mentioned.
    pub mention: Option<String>,
    pub operation_id: String,
    pub card: bool,
}

impl Post {
    /// A reply to `message_ref` in its thread.
    pub fn replies_to(&self, message_ref: &str) -> bool {
        matches!(&self.to, PostedTo::Reply { message_ref: m, in_thread: true } if m == message_ref)
    }

    /// A new root message in `group`.
    pub fn is_root_in(&self, group: &str) -> bool {
        matches!(&self.to, PostedTo::Root { group: g } if g == group)
    }
}

/// A group the bot created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub group_id: String,
    pub name: String,
    /// Who the platform added.
    pub members: Vec<String>,
}

/// One message someone says on the platform.
#[derive(Debug, Clone)]
pub struct Message(Inbound);

impl Message {
    /// `sender` mentions the bot in `group`.
    pub fn mention(group: &str, sender: &str, message_ref: &str, text: &str) -> Self {
        Self(Inbound {
            group_id: group.to_owned(),
            group_name: None,
            sender: sender.to_owned(),
            message_ref: message_ref.to_owned(),
            thread_ref: None,
            thread_root: None,
            reply_to: None,
            addressing: Addressing::Mention,
            mentions: Vec::new(),
            text: text.to_owned(),
        })
    }

    /// `sender` says something in `group`, mentioning nobody.
    pub fn ambient(group: &str, sender: &str, message_ref: &str, text: &str) -> Self {
        let mut message = Self::mention(group, sender, message_ref, text);
        message.0.addressing = Addressing::Ambient;
        message
    }

    /// `sender` replies to the bot's message `reply_to`, without a mention.
    pub fn reply(group: &str, sender: &str, message_ref: &str, text: &str, reply_to: &str) -> Self {
        let mut message = Self::mention(group, sender, message_ref, text);
        message.0.addressing = Addressing::Reply;
        message.0.reply_to = Some(reply_to.to_owned());
        message
    }

    /// In the platform thread `thread_ref`, whose root is `root`.
    pub fn in_thread(mut self, thread_ref: &str, root: &str) -> Self {
        self.0.thread_ref = Some(thread_ref.to_owned());
        self.0.thread_root = Some(root.to_owned());
        self
    }

    pub fn replying_to(mut self, message_ref: &str) -> Self {
        self.0.reply_to = Some(message_ref.to_owned());
        self
    }

    pub fn titled(mut self, group_name: &str) -> Self {
        self.0.group_name = Some(group_name.to_owned());
        self
    }

    pub fn mentioning(mut self, user: &str, name: &str) -> Self {
        self.0.mentions.push(PlatformMention {
            user: user.to_owned(),
            name: name.to_owned(),
        });
        self
    }

    pub fn inbound(&self) -> &Inbound {
        &self.0
    }
}

#[derive(Default)]
struct State {
    caps: Caps,
    posts: Vec<Post>,
    /// operation id → the post as first made, and what it was.
    operations: HashMap<String, (PostTarget, String, Posted)>,
    card_operations: HashMap<String, (PostTarget, Card, Posted)>,
    update_operations: HashSet<String>,
    duplicates: usize,
    conflicts: usize,
    next: u64,
    /// message ref → (its group, its thread).
    messages: HashMap<String, (Option<String>, Option<String>)>,
    cards: HashMap<String, Card>,
    card_updates: Vec<(String, Card)>,
    pins: Vec<String>,
    groups: Vec<(String, Group)>,
    invites: Vec<(String, Vec<String>)>,
    invite_operations: HashSet<String>,
    renames: Vec<(String, String)>,
    joined: BTreeSet<String>,
    adopt_calls: Vec<String>,
    release_calls: Vec<String>,
    adopt_delay: Duration,
    release_delay: Duration,
    /// Adopts (releases) still to fail as unavailable.
    adopt_failures: usize,
    release_failures: usize,
    /// Adopts never return.
    adopt_hangs: bool,
    /// Root posts into a group the bot has not joined (and not configured) are
    /// refused, as a platform does for a chat the bot is not in.
    posts_need_membership: bool,
    replies_need_membership: bool,
    membership_calls: Vec<MembershipCall>,
    /// When set, the groups configured on the platform: unmentioned chat
    /// from any other group the bot has not joined is not delivered.
    configured: Option<BTreeSet<String>>,
    /// Posts still to fail, each as the caller sees a bug in the surface.
    panics: usize,
    /// Posts still to refuse as unavailable.
    outages: usize,
    /// Posts still to refuse outright, joined or not.
    refusals: usize,
    /// Every post refused for want of membership, or on request.
    rejected: usize,
}

/// One call to [`ChatSurface::adopt_group`] or
/// [`ChatSurface::release_group`], when it was made (the runtime's clock, so
/// a test with a paused clock reads exact times) and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MembershipCall {
    pub kind: MembershipKind,
    /// The message ref an adopt named its group by, or the group a release
    /// named.
    pub subject: String,
    pub at: Instant,
    pub outcome: MembershipOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MembershipKind {
    Adopt,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MembershipOutcome {
    Done,
    /// Refused as unavailable on request ([`FakeChat::fail_adopts`]).
    Failed,
    /// Made and never answered ([`FakeChat::hang_adopts`]), or not answered
    /// yet.
    Pending,
    Unsupported,
    Rejected,
}

/// The fake platform. See the module docs.
pub struct FakeChat {
    state: Mutex<State>,
    gateway: Mutex<Option<ChatGateway>>,
}

impl FakeChat {
    /// The platform, and the gateway over it to mount
    /// ([`Assembly::chat_gateway`](crate::Assembly::chat_gateway)).
    pub fn new() -> (Arc<Self>, ChatGateway) {
        let fake = Arc::new(Self {
            state: Mutex::default(),
            gateway: Mutex::new(None),
        });
        let gateway = ChatGateway::new(fake.clone());
        *lock(&fake.gateway) = Some(gateway.clone());
        (fake, gateway)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    pub fn set_caps(&self, caps: Caps) {
        self.lock().caps = caps;
    }

    /// Only these groups get unmentioned chat delivered, besides groups the
    /// bot has joined.
    pub fn gate_ambient(&self, configured: &[&str]) {
        self.lock().configured = Some(configured.iter().map(|g| (*g).to_owned()).collect());
    }

    /// The next `n` posts panic, as a bug in a surface would.
    pub fn panic_next_posts(&self, n: usize) {
        self.lock().panics = n;
    }

    /// The next `n` posts are refused as unavailable, as during an outage.
    pub fn fail_next_posts(&self, n: usize) {
        self.lock().outages = n;
    }

    /// Every adopt takes this long to answer.
    pub fn delay_adopts(&self, delay: Duration) {
        self.lock().adopt_delay = delay;
    }

    /// Every release takes this long to answer.
    pub fn delay_releases(&self, delay: Duration) {
        self.lock().release_delay = delay;
    }

    /// The next `n` adopts are refused as unavailable.
    pub fn fail_adopts(&self, n: usize) {
        self.lock().adopt_failures = n;
    }

    /// The next `n` releases are refused as unavailable.
    pub fn fail_releases(&self, n: usize) {
        self.lock().release_failures = n;
    }

    /// Adopts never answer (or answer again, with `false`).
    pub fn hang_adopts(&self, hang: bool) {
        self.lock().adopt_hangs = hang;
    }

    /// Root posts into a group the bot has not joined (and not configured with
    /// [`gate_ambient`](Self::gate_ambient)) are refused, until the group is
    /// adopted.
    pub fn require_membership_to_post(&self, require: bool) {
        self.lock().posts_need_membership = require;
    }

    /// The next `n` posts are refused outright ([`SurfaceError::Rejected`]),
    /// whether or not the bot has joined the group.
    pub fn reject_next_posts(&self, n: usize) {
        self.lock().refusals = n;
    }

    /// How many posts the platform has refused, for want of membership or on
    /// request.
    pub fn rejected_posts(&self) -> usize {
        self.lock().rejected
    }

    /// Replies to a message of a group the bot has not joined (and not configured
    /// with [`gate_ambient`](Self::gate_ambient)) are refused, until the
    /// group is adopted. A reply names a message, never the group: the
    /// platform knows which.
    pub fn require_membership_to_reply(&self, require: bool) {
        self.lock().replies_need_membership = require;
    }

    /// The platform knows `message_ref` as a message of `group_id`, as one
    /// it delivered before the daemon (re)started.
    pub fn know_message(&self, message_ref: &str, group_id: &str) {
        self.lock()
            .messages
            .entry(message_ref.to_owned())
            .or_insert((Some(group_id.to_owned()), None));
    }

    /// Someone says something. Unmentioned chat from a group the platform
    /// does not deliver from is [`Delivered::Dropped`] here, before any
    /// gateway sees it.
    pub async fn deliver(&self, message: Message) -> Result<Delivered, GatewayError> {
        let inbound = message.0;
        {
            let mut state = self.lock();
            let thread = inbound.thread_ref.clone();
            state.messages.insert(
                inbound.message_ref.clone(),
                (Some(inbound.group_id.clone()), thread),
            );
            if let Some(configured) = &state.configured
                && inbound.addressing == Addressing::Ambient
                && !configured.contains(&inbound.group_id)
                && !state.joined.contains(&inbound.group_id)
            {
                return Ok(Delivered::Dropped);
            }
        }
        let gateway = lock(&self.gateway)
            .clone()
            .ok_or(GatewayError::NotRunning)?;
        gateway.deliver(inbound).await
    }

    /// Every distinct post, cards included, in order.
    pub fn posts(&self) -> Vec<Post> {
        self.lock().posts.clone()
    }

    /// The card posted as `message_ref`, as it stands now.
    pub fn card(&self, message_ref: &str) -> Option<Card> {
        self.lock().cards.get(message_ref).cloned()
    }

    /// Every card update applied, in order.
    pub fn card_updates(&self) -> Vec<(String, Card)> {
        self.lock().card_updates.clone()
    }

    pub fn pins(&self) -> Vec<String> {
        self.lock().pins.clone()
    }

    /// Groups the bot created.
    pub fn groups(&self) -> Vec<Group> {
        self.lock().groups.iter().map(|(_, g)| g.clone()).collect()
    }

    /// `(group, users)` per invite.
    pub fn invites(&self) -> Vec<(String, Vec<String>)> {
        self.lock().invites.clone()
    }

    pub fn renames(&self) -> Vec<(String, String)> {
        self.lock().renames.clone()
    }

    /// Groups the bot has joined: created or adopted, not released.
    pub fn joined(&self) -> Vec<String> {
        self.lock().joined.iter().cloned().collect()
    }

    /// The message refs each adopt named its group by.
    pub fn adopt_calls(&self) -> Vec<String> {
        self.lock().adopt_calls.clone()
    }

    pub fn release_calls(&self) -> Vec<String> {
        self.lock().release_calls.clone()
    }

    /// Every adopt and release, in the order made, with when and how it
    /// ended.
    pub fn membership_calls(&self) -> Vec<MembershipCall> {
        self.lock().membership_calls.clone()
    }

    /// [`membership_calls`](Self::membership_calls) of one kind.
    pub fn membership_calls_of(&self, kind: MembershipKind) -> Vec<MembershipCall> {
        self.lock()
            .membership_calls
            .iter()
            .filter(|c| c.kind == kind)
            .cloned()
            .collect()
    }

    fn refuse_nonmembers(state: &mut State, target: &PostTarget) -> Result<(), SurfaceError> {
        let result = Self::nonmember(state, target);
        if result.is_err() {
            state.rejected += 1;
        }
        result
    }

    fn nonmember(state: &mut State, target: &PostTarget) -> Result<(), SurfaceError> {
        if state.refusals > 0 {
            state.refusals -= 1;
            return Err(SurfaceError::Rejected(
                "fake chat: refused on request".into(),
            ));
        }
        let (group, required) = match target {
            PostTarget::NewRoot { group_id } => (Some(group_id), state.posts_need_membership),
            PostTarget::Reply { message_ref, .. } => (
                state
                    .messages
                    .get(message_ref)
                    .and_then(|(group, _)| group.as_ref()),
                state.replies_need_membership,
            ),
        };
        if let Some(group_id) = group
            && required
            && !state.joined.contains(group_id)
            && !state
                .configured
                .as_ref()
                .is_some_and(|c| c.contains(group_id))
        {
            return Err(SurfaceError::Rejected(format!(
                "unknown_channel: {group_id}"
            )));
        }
        Ok(())
    }

    /// Replays answered as duplicates.
    pub fn duplicates(&self) -> usize {
        self.lock().duplicates
    }

    /// Posts refused because their key was used for other content.
    pub fn conflicts(&self) -> usize {
        self.lock().conflicts
    }

    /// The platform thread `message_ref` is in, or the one replying in
    /// thread to it opens.
    pub fn thread_of(&self, message_ref: &str) -> String {
        let mut state = self.lock();
        let thread = state
            .messages
            .get(message_ref)
            .and_then(|(_, t)| t.clone())
            .unwrap_or_else(|| format!("t_{message_ref}"));
        let entry = state
            .messages
            .entry(message_ref.to_owned())
            .or_insert((None, None));
        entry.1 = Some(thread.clone());
        thread
    }

    fn mint(state: &mut State, target: &PostTarget) -> (Posted, Option<String>) {
        state.next += 1;
        let message_ref = format!("m_{}", state.next);
        let (group, thread_ref) = match target {
            PostTarget::NewRoot { group_id } => (Some(group_id.clone()), None),
            PostTarget::Reply {
                message_ref: parent,
                in_thread,
            } => {
                let (group, parent_thread) =
                    state.messages.get(parent).cloned().unwrap_or((None, None));
                let thread = in_thread.then(|| {
                    let thread = parent_thread.unwrap_or_else(|| format!("t_{parent}"));
                    let entry = state
                        .messages
                        .entry(parent.clone())
                        .or_insert((group.clone(), None));
                    entry.1 = Some(thread.clone());
                    thread
                });
                (group, thread)
            }
        };
        state
            .messages
            .insert(message_ref.clone(), (group.clone(), thread_ref.clone()));
        (
            Posted {
                message_ref,
                thread_ref,
                duplicate: false,
            },
            group,
        )
    }

    fn post_inner(
        &self,
        target: PostTarget,
        markdown: &str,
        operation_id: &str,
        mention: Option<&str>,
    ) -> Result<Posted, SurfaceError> {
        let mut state = self.lock();
        if state.panics > 0 {
            state.panics -= 1;
            drop(state);
            panic!("fake chat: a post panics on request");
        }
        if state.outages > 0 {
            state.outages -= 1;
            return Err(SurfaceError::Unavailable("fake chat: an outage".into()));
        }
        Self::refuse_nonmembers(&mut state, &target)?;
        let content = format!("{}\0{markdown}", mention.unwrap_or_default());
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
        let (posted, group) = Self::mint(&mut state, &target);
        state.operations.insert(
            operation_id.to_owned(),
            (target.clone(), content, posted.clone()),
        );
        state.posts.push(Post {
            to: posted_to(&target),
            group,
            markdown: markdown.to_owned(),
            message_ref: posted.message_ref.clone(),
            thread_ref: posted.thread_ref.clone(),
            mention: mention.map(str::to_owned),
            operation_id: operation_id.to_owned(),
            card: false,
        });
        Ok(posted)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn posted_to(target: &PostTarget) -> PostedTo {
    match target {
        PostTarget::NewRoot { group_id } => PostedTo::Root {
            group: group_id.clone(),
        },
        PostTarget::Reply {
            message_ref,
            in_thread,
        } => PostedTo::Reply {
            message_ref: message_ref.clone(),
            in_thread: *in_thread,
        },
    }
}

fn unsupported<T>(what: &str) -> Result<T, SurfaceError> {
    Err(SurfaceError::Unsupported(format!(
        "this platform has no {what}"
    )))
}

#[async_trait]
impl ChatSurface for FakeChat {
    fn platform(&self) -> &str {
        PLATFORM
    }

    async fn post(
        &self,
        target: PostTarget,
        markdown: &str,
        operation_id: &str,
    ) -> Result<Posted, SurfaceError> {
        self.post_inner(target, markdown, operation_id, None)
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
        Self::refuse_nonmembers(&mut state, &target)?;
        let (posted, group) = Self::mint(&mut state, &target);
        state.card_operations.insert(
            operation_id.to_owned(),
            (target.clone(), card.clone(), posted.clone()),
        );
        state.cards.insert(posted.message_ref.clone(), card.clone());
        state.posts.push(Post {
            to: posted_to(&target),
            group,
            markdown: format!("{}\n{}", card.title, card.markdown),
            message_ref: posted.message_ref.clone(),
            thread_ref: posted.thread_ref.clone(),
            mention: None,
            operation_id: operation_id.to_owned(),
            card: true,
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
        if !state.cards.contains_key(message_ref) {
            return Err(SurfaceError::Rejected(format!("no card {message_ref}")));
        }
        state.cards.insert(message_ref.to_owned(), card.clone());
        state
            .card_updates
            .push((message_ref.to_owned(), card.clone()));
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

    async fn set_announcement(
        &self,
        _group_id: &str,
        _section: &AnnouncementSection,
        _operation_id: &str,
    ) -> Result<(), SurfaceError> {
        if !self.lock().caps.announcements {
            return unsupported("announcements");
        }
        Ok(())
    }

    async fn create_group(
        &self,
        group: &NewGroup,
        operation_id: &str,
    ) -> Result<CreatedGroup, SurfaceError> {
        let mut state = self.lock();
        if !state.caps.groups {
            return unsupported("group creation");
        }
        let (members, not_added) = if state.caps.members {
            (group.invite.clone(), Vec::new())
        } else {
            (Vec::new(), group.invite.clone())
        };
        if let Some((_, created)) = state.groups.iter().find(|(op, _)| op == operation_id) {
            return Ok(CreatedGroup {
                group_id: created.group_id.clone(),
                not_added,
            });
        }
        let group_id = format!("g_{}", state.groups.len() + 1);
        state.joined.insert(group_id.clone());
        state.groups.push((
            operation_id.to_owned(),
            Group {
                group_id: group_id.clone(),
                name: group.name.clone(),
                members,
            },
        ));
        Ok(CreatedGroup {
            group_id,
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

    async fn adopt_group(&self, message_ref: &str) -> Result<AdoptedGroup, SurfaceError> {
        let (delay, hang, call) = {
            let mut state = self.lock();
            if !state.caps.membership {
                state.membership_calls.push(MembershipCall {
                    kind: MembershipKind::Adopt,
                    subject: message_ref.to_owned(),
                    at: Instant::now(),
                    outcome: MembershipOutcome::Unsupported,
                });
                return unsupported("group membership");
            }
            state.adopt_calls.push(message_ref.to_owned());
            let outcome = if state.adopt_failures > 0 {
                state.adopt_failures -= 1;
                MembershipOutcome::Failed
            } else {
                MembershipOutcome::Pending
            };
            state.membership_calls.push(MembershipCall {
                kind: MembershipKind::Adopt,
                subject: message_ref.to_owned(),
                at: Instant::now(),
                outcome,
            });
            (
                state.adopt_delay,
                state.adopt_hangs,
                state.membership_calls.len() - 1,
            )
        };
        if self.lock().membership_calls[call].outcome == MembershipOutcome::Failed {
            return Err(SurfaceError::Unavailable("fake chat: an outage".into()));
        }
        if hang {
            std::future::pending::<()>().await;
        }
        tokio::time::sleep(delay).await;
        let mut state = self.lock();
        let Some(group_id) = state.messages.get(message_ref).and_then(|(g, _)| g.clone()) else {
            state.membership_calls[call].outcome = MembershipOutcome::Rejected;
            return Err(SurfaceError::Rejected(format!(
                "unknown message {message_ref}"
            )));
        };
        let already_joined = !state.joined.insert(group_id.clone());
        state.membership_calls[call].outcome = MembershipOutcome::Done;
        Ok(AdoptedGroup {
            group_id,
            already_joined,
        })
    }

    async fn release_group(&self, group_id: &str) -> Result<bool, SurfaceError> {
        let delay = {
            let mut state = self.lock();
            let outcome = if !state.caps.membership {
                MembershipOutcome::Unsupported
            } else if state.release_failures > 0 {
                state.release_failures -= 1;
                MembershipOutcome::Failed
            } else {
                MembershipOutcome::Pending
            };
            state.membership_calls.push(MembershipCall {
                kind: MembershipKind::Release,
                subject: group_id.to_owned(),
                at: Instant::now(),
                outcome,
            });
            match outcome {
                MembershipOutcome::Unsupported => return unsupported("group membership"),
                MembershipOutcome::Failed => {
                    return Err(SurfaceError::Unavailable("fake chat: an outage".into()));
                }
                _ => {}
            }
            state.release_calls.push(group_id.to_owned());
            state.release_delay
        };
        tokio::time::sleep(delay).await;
        let mut state = self.lock();
        if let Some(call) = state
            .membership_calls
            .iter_mut()
            .rev()
            .find(|c| c.kind == MembershipKind::Release && c.subject == group_id)
        {
            call.outcome = MembershipOutcome::Done;
        }
        Ok(state.joined.remove(group_id))
    }
}
