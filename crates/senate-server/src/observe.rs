//! Metadata projection of the local authoritative log. Private event and
//! task types never reach the observer serializer.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use chrono::Utc;
use futures::{StreamExt, TryStreamExt};
use senate_core::api::ThreadsQuery;
use senate_core::docket::Awaiting;
use senate_core::event::meta;
use senate_core::{Address, CommitteeId, Hub, Kind, PresenceState, SenatorName, Seq};
use senate_observe::{
    Capture, CardKey, GrantInfo, Hint, LinePolicy, LocalNotice, Preview, ProjectionError, Registry,
    SenateSource,
};
use senate_observe_protocol as public;
use senate_observe_protocol::link::{AppLink, Target};

use crate::ServerHub;

const OPEN_LIMIT: usize = 40;
const SETTLED_LIMIT: usize = 10;
const DESK_LIMIT: usize = 50;
const THREAD_PAGE: usize = 101;
const THREAD_SCAN_PAGES: usize = 8;
const BOOTSTRAP_CURSOR: &str = "AAAAAAAAAAAAAAAAAAAAAA";

pub(super) struct Source {
    hub: Weak<ServerHub>,
    registry: Weak<Registry>,
}

impl Source {
    pub(super) fn new(hub: &Arc<ServerHub>, registry: &Arc<Registry>) -> Self {
        Self {
            hub: Arc::downgrade(hub),
            registry: Arc::downgrade(registry),
        }
    }

    fn hub(&self) -> Result<Arc<ServerHub>, ProjectionError> {
        self.hub.upgrade().ok_or(ProjectionError::NotReady)
    }
}

struct CommitteeView {
    committee: public::Committee,
    senators: Vec<public::Senator>,
    retired: Vec<public::RetiredSenator>,
    threads: Vec<public::Thread>,
    docket: Vec<public::DeskItem>,
    windows: public::Truncation,
    waiting_on_person: BTreeSet<String>,
    head: Seq,
}

#[async_trait]
impl SenateSource for Source {
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<senate_core::Event> {
        self.hub.upgrade().map_or_else(
            || tokio::sync::broadcast::channel(1).1,
            |hub| hub.subscribe(),
        )
    }

    fn idle(&self) {
        if let Some(registry) = self.registry.upgrade() {
            registry.release_machine_observation();
        }
    }

    async fn capture(&self, grant: &GrantInfo) -> Result<Capture, ProjectionError> {
        let hub = self.hub()?;
        let registry = self.registry.upgrade().ok_or(ProjectionError::NotReady)?;
        let id = registry
            .installation_id()
            .map_err(|_| ProjectionError::NotReady)?;
        let (open, archived) = hub.committees_by_archive().await.map_err(not_ready)?;
        let eligible: BTreeMap<CommitteeId, bool> = open
            .into_iter()
            .map(|c| (c, false))
            .chain(archived.into_iter().map(|c| (c, true)))
            .filter(|(id, _)| {
                grant
                    .committees
                    .as_ref()
                    .is_none_or(|allow| allow.contains(id))
            })
            .collect();
        if eligible.len() > 256 {
            return Err(ProjectionError::TooLarge);
        }
        let roster = hub.roster_now();
        let desk = roster
            .office()
            .is_some_and(|office| office.desk_owner().is_some());
        let aliases = hub.aliases().await;
        let context = CommitteeContext {
            hub: &hub,
            roster: &roster,
            aliases: &aliases,
            id: &id,
            desk,
            topology: grant.topology,
        };
        let rows: Vec<_> = futures::stream::iter(eligible)
            .map(|(committee, archived)| committee_view(&context, committee, archived))
            .buffered(8)
            .try_collect()
            .await?;
        let mut snapshot = public::Snapshot {
            schema_version: public::V1,
            capabilities: vec!["state_patch".into(), "app_links".into()],
            as_of: Utc::now(),
            cursor: BOOTSTRAP_CURSOR
                .parse()
                .map_err(|_| ProjectionError::NotReady)?,
            senate: public::Senate {
                id: id.clone(),
                name: hub.senate_name().to_string(),
                link: None,
                person: None,
                desk_owner: None,
                desk_workers: Vec::new(),
            },
            navigation: Some(public::Navigation {
                desk: desk.then(|| link(&id, Target::Desk)).flatten(),
                approvals: link(&id, Target::Approvals),
                inbox: link(&id, Target::Inbox),
                history: link(&id, Target::History),
                profile: link(&id, Target::Profile),
            }),
            links: Vec::new(),
            committees: Vec::new(),
            projects: Vec::new(),
            senators: Vec::new(),
            retired: Vec::new(),
            threads: Vec::new(),
            desk_items: Vec::new(),
            approvals: Vec::new(),
            windows: public::Windows {
                open_threads_per_committee: OPEN_LIMIT as u16,
                settled_threads_per_committee: SETTLED_LIMIT as u16,
                desk_items_per_committee: DESK_LIMIT as u16,
                truncated: Vec::new(),
            },
        };
        let mut heads = BTreeMap::new();
        let mut waiting = BTreeMap::new();
        for row in rows {
            heads.insert(
                row.committee
                    .id
                    .parse()
                    .map_err(|_| ProjectionError::NotReady)?,
                row.head,
            );
            waiting.insert(row.committee.id.clone(), row.waiting_on_person);
            snapshot.committees.push(row.committee);
            snapshot.senators.extend(row.senators);
            snapshot.retired.extend(row.retired);
            snapshot.threads.extend(row.threads);
            snapshot.desk_items.extend(row.docket);
            if row.windows.open_threads || row.windows.settled_threads || row.windows.desk_items {
                snapshot.windows.truncated.push(row.windows);
            }
        }
        if desk {
            snapshot.capabilities.push("desk".into());
        }
        if grant.topology {
            snapshot.capabilities.push("topology".into());
            if let Some(office) = hub.office() {
                snapshot.senate.desk_owner = office.desk_owner().map(ToString::to_string);
                snapshot.senate.desk_workers = office
                    .desk
                    .iter()
                    .filter(|agent| {
                        snapshot
                            .senators
                            .iter()
                            .any(|seat| seat.id == agent.as_str())
                    })
                    .map(ToString::to_string)
                    .collect();
            }
            if let Some(parent) = hub.parent() {
                snapshot.links.push(public::SenateLink {
                    id: "parent".into(),
                    kind: public::LinkKind::Parent,
                    name: parent.name().to_string(),
                    state: public::LinkState::Unknown,
                    link: None,
                });
            }
            for child in hub.children() {
                let now = Utc::now();
                let state = if child.is_online(now) {
                    public::LinkState::Up
                } else if child.is_offline(now) {
                    public::LinkState::Down
                } else {
                    public::LinkState::Unknown
                };
                snapshot.links.push(public::SenateLink {
                    id: format!("child:{}", child.name),
                    kind: public::LinkKind::Child,
                    name: child.name.to_string(),
                    state,
                    link: None,
                });
            }
        }
        snapshot.capabilities.extend([
            "cards".into(),
            "retired_senators".into(),
            "tool_lifecycle".into(),
            "tool_lifecycle:claude".into(),
            "tool_lifecycle:codex".into(),
            "project_releases".into(),
            "approval_metadata".into(),
            "message_routing".into(),
            "hosted_viewers".into(),
        ]);
        if let Some(policy) = grant.lines {
            snapshot.capabilities.push(
                match policy {
                    LinePolicy::Auto => "lines_auto",
                    LinePolicy::Published => "published_lines",
                }
                .into(),
            );
        }
        registry.merge_machine_facts(grant, &mut snapshot).await?;
        for seat in &mut snapshot.senators {
            if matches!(
                seat.state,
                public::SenatorState::Idle | public::SenatorState::Unknown
            ) && waiting
                .get(&seat.committee_id)
                .is_some_and(|owners| owners.contains(&seat.id))
            {
                seat.state = public::SenatorState::WaitingOnPerson;
            }
        }
        for committee in &mut snapshot.committees {
            committee.card = registry.card(&CardKey::Committee {
                committee_id: committee
                    .id
                    .parse()
                    .map_err(|_| ProjectionError::NotReady)?,
            });
        }
        for senator in &mut snapshot.senators {
            let senator_id = senator.id.parse().map_err(|_| ProjectionError::NotReady)?;
            let key = if senator.kind == public::SenatorKind::Person {
                CardKey::Person { senator_id }
            } else {
                CardKey::Seat {
                    committee_id: senator
                        .committee_id
                        .parse()
                        .map_err(|_| ProjectionError::NotReady)?,
                    senator_id,
                }
            };
            senator.card = registry.card(&key);
        }
        for senator in &mut snapshot.retired {
            senator.card = registry.card(&CardKey::Seat {
                committee_id: senator
                    .committee_id
                    .parse()
                    .map_err(|_| ProjectionError::NotReady)?,
                senator_id: senator.id.parse().map_err(|_| ProjectionError::NotReady)?,
            });
        }
        if let Some(person) = &mut snapshot.senate.person {
            person.card = registry.card(&CardKey::Person {
                senator_id: person.id.parse().map_err(|_| ProjectionError::NotReady)?,
            });
        }
        Ok(Capture { snapshot, heads })
    }

    async fn read(
        &self,
        committee: &CommitteeId,
        after: Seq,
        limit: usize,
    ) -> Result<Vec<senate_core::Event>, ProjectionError> {
        self.hub()?
            .read(committee, after, limit)
            .await
            .map_err(not_ready)
    }

    async fn hint(
        &self,
        grant: &GrantInfo,
        event: &senate_core::Event,
        state: &public::Snapshot,
    ) -> Result<Option<Hint>, ProjectionError> {
        let committee = event.committee.as_str();
        if let Some(change) = senate_core::CommitteeSeatChange::of(event) {
            let id = change.senator.to_string();
            let target = Target::Senator(format!("{committee}::{id}"));
            let row = public::SenatorRef { senator_id: id };
            return Ok(Some(Hint {
                at: None,
                committee_id: Some(committee.into()),
                link: link(&state.senate.id, target),
                event: match change.action {
                    senate_core::CommitteeSeatAction::Join => public::Event::SenatorJoined(row),
                    senate_core::CommitteeSeatAction::Unseat => public::Event::SenatorLeft(row),
                },
            }));
        }
        let Some(message_kind) = eligible(event) else {
            return Ok(None);
        };
        if let Some(root) = event.thread
            && self
                .hub()?
                .get(&event.committee, root)
                .await
                .map_err(not_ready)?
                .as_ref()
                .and_then(eligible)
                .is_none()
        {
            return Ok(None);
        }
        let seats: BTreeSet<_> = state
            .senators
            .iter()
            .filter(|seat| seat.committee_id == committee)
            .map(|seat| seat.id.as_str())
            .collect();
        let seq = wire_seq(event.seq)?;
        let root = match event.thread {
            Some(root) => Some(wire_seq(root)?),
            None if matches!(
                message_kind,
                public::MessageKind::Question | public::MessageKind::Request
            ) =>
            {
                Some(seq)
            }
            None => None,
        };
        let target = root.map_or_else(
            || Target::Committee(committee.to_owned()),
            |root| Target::Thread(committee.to_owned(), root),
        );
        Ok(Some(Hint {
            at: None,
            committee_id: Some(committee.into()),
            link: link(&state.senate.id, target),
            event: public::Event::MessagePosted(public::MessagePosted {
                seq,
                message_kind,
                from: visible_address(&event.from, &seats),
                to: event
                    .to
                    .iter()
                    .map(|addr| visible_address(addr, &seats))
                    .collect(),
                root_seq: root,
                line: self
                    .registry
                    .upgrade()
                    .and_then(|registry| registry.line_for(grant, event)),
            }),
        }))
    }
    async fn preview(&self, committee: &CommitteeId, seq: Seq) -> Option<Preview> {
        let event = self.hub().ok()?.get(committee, seq).await.ok().flatten()?;
        eligible(&event)?;
        if let Some(root) = event.thread {
            let root = self.hub().ok()?.get(committee, root).await.ok().flatten()?;
            eligible(&root)?;
        }
        let line = senate_observe::first_line(&event.body)?;
        Some(Preview {
            committee: committee.clone(),
            event_id: event.id,
            line,
        })
    }

    async fn notice(
        &self,
        grant: &GrantInfo,
        notice: LocalNotice,
        state: &public::Snapshot,
    ) -> Result<Option<Hint>, ProjectionError> {
        let registry = self.registry.upgrade().ok_or(ProjectionError::NotReady)?;
        match notice {
            LocalNotice::Playback(_) => Ok(None),
            LocalNotice::Preview { committee, seq }
                if grant.lines == Some(LinePolicy::Published) =>
            {
                let Some(message) = self.visible_message(grant, state, &committee, seq).await?
                else {
                    return Ok(None);
                };
                let public::Event::MessagePosted(data) = message.event else {
                    return Ok(None);
                };
                Ok(data.line.map(|line| Hint {
                    at: None,
                    committee_id: message.committee_id,
                    link: message.link,
                    event: public::Event::MessageLinePublished(public::MessageLinePublished {
                        seq: data.seq,
                        line,
                    }),
                }))
            }
            LocalNotice::Preview { .. } => Ok(None),
            LocalNotice::Route {
                committee,
                seq,
                senator,
                phase,
                reason,
            } => {
                let Some(message) = self.visible_message(grant, state, &committee, seq).await?
                else {
                    return Ok(None);
                };
                let route_id = registry
                    .route_id(grant, &committee, seq, senator.as_ref())
                    .map_err(|_| ProjectionError::NotReady)?;
                let target = senator.map(|senator| {
                    if visible_seat(state, &committee, &senator) {
                        public::Address::Senator {
                            id: senator.to_string(),
                        }
                    } else {
                        public::Address::External
                    }
                });
                Ok(Some(Hint {
                    at: None,
                    committee_id: message.committee_id,
                    link: message.link,
                    event: public::Event::MessageRouted(public::MessageRouted {
                        route_id,
                        seq: Some(wire_seq(seq)?),
                        target,
                        phase,
                        reason,
                    }),
                }))
            }
            LocalNotice::Tool {
                committee,
                senator,
                session,
                call,
                kind,
                started,
            } => {
                if !visible_seat(state, &committee, &senator) {
                    return Ok(None);
                }
                let tool_kind = kind.parse().map_err(|_| ProjectionError::NotReady)?;
                let data = public::ToolActivity {
                    senator_id: senator.to_string(),
                    session_id: registry
                        .tool_identity(grant, &session)
                        .map_err(|_| ProjectionError::NotReady)?,
                    call_id: registry
                        .tool_identity(grant, &call)
                        .map_err(|_| ProjectionError::NotReady)?,
                    tool_kind,
                };
                Ok(Some(Hint {
                    at: None,
                    committee_id: Some(committee.to_string()),
                    link: link(
                        &state.senate.id,
                        Target::Senator(format!("{committee}::{senator}")),
                    ),
                    event: if started {
                        public::Event::ToolStarted(data)
                    } else {
                        public::Event::ToolEnded(data)
                    },
                }))
            }
            LocalNotice::Card { key, revision } => Ok(card_hint(state, key, revision)),
            LocalNotice::ApprovalDecided {
                native_id,
                committee,
                senator,
                tool,
                decision,
            } => {
                if !visible_seat(state, &committee, &senator) {
                    return Ok(None);
                }
                let id = registry
                    .approval_id(&native_id)
                    .map_err(|_| ProjectionError::NotReady)?;
                Ok(Some(Hint {
                    at: None,
                    committee_id: Some(committee.to_string()),
                    link: link(&state.senate.id, Target::Approvals),
                    event: public::Event::ApprovalDecided(public::ApprovalDecided {
                        id,
                        tool_kind: tool,
                        decision,
                    }),
                }))
            }
        }
    }
}

impl Source {
    async fn visible_message(
        &self,
        grant: &GrantInfo,
        state: &public::Snapshot,
        committee: &CommitteeId,
        seq: Seq,
    ) -> Result<Option<Hint>, ProjectionError> {
        if !state
            .committees
            .iter()
            .any(|row| row.id == committee.as_str())
        {
            return Ok(None);
        }
        let Some(event) = self.hub()?.get(committee, seq).await.map_err(not_ready)? else {
            return Ok(None);
        };
        if eligible(&event).is_none() {
            return Ok(None);
        }
        self.hint(grant, &event, state).await
    }
}

fn visible_seat(state: &public::Snapshot, committee: &CommitteeId, senator: &SenatorName) -> bool {
    state
        .senators
        .iter()
        .any(|row| row.committee_id == committee.as_str() && row.id == senator.as_str())
}

fn card_hint(state: &public::Snapshot, key: CardKey, revision: String) -> Option<Hint> {
    let (subject, committee_id, link) = match key {
        CardKey::Committee { committee_id } => {
            let row = state
                .committees
                .iter()
                .find(|row| row.id == committee_id.as_str())?;
            (
                public::CardSubject::Committee {
                    committee_id: row.id.clone(),
                },
                Some(row.id.clone()),
                row.link.clone(),
            )
        }
        CardKey::Seat {
            committee_id,
            senator_id,
        } => {
            let row = state.senators.iter().find(|row| {
                row.committee_id == committee_id.as_str() && row.id == senator_id.as_str()
            })?;
            (
                public::CardSubject::Senator {
                    committee_id: row.committee_id.clone(),
                    senator_id: row.id.clone(),
                },
                Some(row.committee_id.clone()),
                row.link.clone(),
            )
        }
        CardKey::Person { senator_id } => {
            let link = state
                .senate
                .person
                .as_ref()
                .filter(|row| row.id == senator_id.as_str())
                .and_then(|row| row.link.clone())
                .or_else(|| {
                    state
                        .senators
                        .iter()
                        .find(|row| {
                            row.kind == public::SenatorKind::Person && row.id == senator_id.as_str()
                        })
                        .and_then(|row| row.link.clone())
                })?;
            (
                public::CardSubject::Person {
                    person_id: senator_id.to_string(),
                },
                None,
                Some(link),
            )
        }
    };
    Some(Hint {
        at: None,
        committee_id,
        link,
        event: public::Event::CardChanged(public::CardChanged { subject, revision }),
    })
}

struct CommitteeContext<'a> {
    hub: &'a ServerHub,
    roster: &'a senate_core::Roster,
    aliases: &'a senate_core::alias::CommitteeAliases,
    id: &'a str,
    desk: bool,
    topology: bool,
}

async fn committee_view(
    context: &CommitteeContext<'_>,
    committee: CommitteeId,
    archived: bool,
) -> Result<CommitteeView, ProjectionError> {
    let CommitteeContext {
        hub,
        roster,
        aliases,
        id,
        desk,
        topology,
    } = *context;
    // Capture the durable head before any other reads. The worker attached its
    // subscription before capture and will reconcile any later commit.
    let head = hub.head(&committee).await.map_err(not_ready)?;
    let (chair, presence, agents) = tokio::try_join!(
        hub.chair(&committee),
        hub.presence(&committee),
        hub.committee_agents(&committee),
    )
    .map_err(not_ready)?;
    let mut senators = Vec::new();
    let mut seated: BTreeSet<SenatorName> = agents.into_iter().collect();
    if let Some(chair) = &chair.chair {
        seated.insert(chair.clone());
    }
    for senator in &seated {
        let p = presence.agents.iter().find(|p| &p.agent == senator);
        let state = match p {
            Some(p) if p.paused => public::SenatorState::Paused,
            Some(p) if p.state != PresenceState::Online => public::SenatorState::Away,
            _ => public::SenatorState::Unknown,
        };
        senators.push(public::Senator {
            id: senator.to_string(),
            committee_id: committee.to_string(),
            alias: aliases
                .alias_of(&committee, senator)
                .map(ToString::to_string),
            runtime: None,
            kind: if roster.is_person(senator) {
                public::SenatorKind::Person
            } else {
                public::SenatorKind::Agent
            },
            state,
            last_active_at: p.and_then(|p| p.last_seen),
            hosted: public::Hosting::Unknown,
            viewer_attached: None,
            link: link(id, Target::Senator(format!("{committee}::{senator}"))),
            card: None,
        });
    }
    let visible: BTreeSet<_> = seated.iter().map(SenatorName::as_str).collect();
    let mut windows = public::Truncation {
        committee_id: committee.to_string(),
        open_threads: false,
        settled_threads: false,
        desk_items: false,
    };
    let mut threads = Vec::new();
    let mut waiting_on_person = BTreeSet::new();
    let mut before = None;
    let mut counts = [0usize; 2];
    for _ in 0..THREAD_SCAN_PAGES {
        let page = hub
            .committee_threads(
                &committee,
                &ThreadsQuery {
                    before,
                    limit: Some(THREAD_PAGE),
                    include_archived: true,
                    include_discussions: true,
                    ..Default::default()
                },
            )
            .await
            .map_err(not_ready)?;
        before = page.threads.last().map(|t| t.updated);
        let last_page = page.threads.len() < THREAD_PAGE;
        for thread in page.threads {
            if counts[0] > OPEN_LIMIT && counts[1] > SETTLED_LIMIT {
                break;
            }
            let settled = thread.is_settled() || thread.state == senate_core::StatusState::Archived;
            let index = usize::from(settled);
            let limit = [OPEN_LIMIT, SETTLED_LIMIT][index];
            if counts[index] > limit {
                continue;
            }
            let root = hub.get(&committee, thread.root).await.map_err(not_ready)?;
            let Some(kind) = root.as_ref().and_then(eligible) else {
                continue;
            };
            counts[index] += 1;
            if counts[index] > limit {
                continue;
            }
            let seq = wire_seq(thread.root)?;
            if matches!(thread.waits(), Some(senate_core::task::WaitsOn::Person))
                && let Some(owner) = &thread.owner
                && seated.contains(&owner.agent)
            {
                waiting_on_person.insert(owner.agent.to_string());
            }
            threads.push(public::Thread {
                committee_id: committee.to_string(),
                root_seq: seq,
                kind,
                opener: visible_address(&thread.from, &visible),
                addressees: thread
                    .to
                    .iter()
                    .map(|addr| visible_address(addr, &visible))
                    .collect(),
                state: thread_state(thread.state),
                waits_on: match thread.waits() {
                    Some(senate_core::task::WaitsOn::Agents) => public::WaitsOn::Agents,
                    Some(senate_core::task::WaitsOn::Person) => public::WaitsOn::Person,
                    None => public::WaitsOn::None,
                },
                link: link(id, Target::Thread(committee.to_string(), seq)),
            });
        }
        if last_page || (counts[0] > OPEN_LIMIT && counts[1] > SETTLED_LIMIT) {
            break;
        }
    }
    threads.sort_by_key(|thread| thread.root_seq);
    windows.open_threads = counts[0] > OPEN_LIMIT;
    windows.settled_threads = counts[1] > SETTLED_LIMIT;
    let mut docket = Vec::new();
    if desk {
        let mut page = hub.docket(&committee).await.map_err(not_ready)?.items;
        page.sort_by_key(|item| item.event.seq);
        for item in page {
            let Some(kind) = eligible(&item.event) else {
                continue;
            };
            if docket.len() == DESK_LIMIT {
                windows.desk_items = true;
                break;
            }
            let seq = wire_seq(item.event.seq)?;
            docket.push(public::DeskItem {
                committee_id: committee.to_string(),
                root_seq: seq,
                kind,
                awaiting: match item.awaiting {
                    Awaiting::Pick { .. } => public::Awaiting::Pick,
                    Awaiting::Addressed => public::Awaiting::Addressed,
                    Awaiting::Away { .. } => public::Awaiting::Away,
                    Awaiting::ChildOffline { .. } if topology => public::Awaiting::ChildOffline,
                    Awaiting::ChildOffline { .. } => public::Awaiting::Unknown,
                },
                link: link(id, Target::DeskItem(committee.to_string(), seq)),
            });
        }
    }
    let mut retired = Vec::new();
    for (senator, seq) in hub.retired_seats(&committee).await.map_err(not_ready)? {
        if seated.contains(&senator) {
            continue;
        }
        let Some(event) = hub.get(&committee, seq).await.map_err(not_ready)? else {
            continue;
        };
        retired.push(public::RetiredSenator {
            id: senator.to_string(),
            committee_id: committee.to_string(),
            alias: aliases
                .alias_of(&committee, &senator)
                .map(ToString::to_string),
            runtime: None,
            last_active_at: None,
            retired_at: event.ts,
            link: None,
            card: None,
        });
    }
    Ok(CommitteeView {
        committee: public::Committee {
            id: committee.to_string(),
            name: committee.to_string(),
            archived,
            chair_id: chair.chair.map(|agent| agent.to_string()),
            project_id: None,
            link: link(id, Target::Committee(committee.to_string())),
            card: None,
        },
        senators,
        retired,
        threads,
        docket,
        windows,
        waiting_on_person,
        head,
    })
}

fn eligible(event: &senate_core::Event) -> Option<public::MessageKind> {
    if event.on_behalf_of.is_some()
        || event.source.is_some()
        || event.is_chair_discussion()
        || event.from.as_senator().is_none() && event.from != Address::Speaker
        || event.meta.keys().any(|key| {
            !matches!(
                key.as_str(),
                meta::SEEN
                    | meta::PICK
                    | meta::PROPOSAL
                    | meta::SUPERSEDES
                    | meta::PROPOSED_BY
                    | meta::SOURCES
                    | meta::TOPIC
                    | meta::COMPANION
            )
        })
    {
        return None;
    }
    match event.kind {
        Kind::Question => Some(public::MessageKind::Question),
        Kind::Request => Some(public::MessageKind::Request),
        Kind::Answer => Some(public::MessageKind::Answer),
        Kind::Note => Some(public::MessageKind::Note),
        Kind::Decision => Some(public::MessageKind::Decision),
        _ => None,
    }
}

fn visible_address(address: &Address, seats: &BTreeSet<&str>) -> public::Address {
    match address {
        Address::Senator(agent) if seats.contains(agent.as_str()) => public::Address::Senator {
            id: agent.to_string(),
        },
        Address::Speaker | Address::Chair => public::Address::Office,
        _ => public::Address::External,
    }
}

fn wire_seq(seq: Seq) -> Result<public::Seq, ProjectionError> {
    std::num::NonZeroU64::new(seq.0)
        .map(public::Seq)
        .ok_or(ProjectionError::NotReady)
}

fn thread_state(state: senate_core::StatusState) -> public::ThreadState {
    match state {
        senate_core::StatusState::Processing => public::ThreadState::Processing,
        senate_core::StatusState::Active => public::ThreadState::Active,
        senate_core::StatusState::Suspended => public::ThreadState::Suspended,
        senate_core::StatusState::Archived => public::ThreadState::Archived,
    }
}

fn link(id: &str, target: Target) -> Option<String> {
    AppLink::for_senate(target, id)
        .ok()
        .map(|link| link.to_string())
}

fn not_ready(_: senate_core::HubError) -> ProjectionError {
    ProjectionError::NotReady
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_message_classification_refuses_private_envelopes()
    -> Result<(), Box<dyn std::error::Error>> {
        let committee = "observe-test".parse()?;
        let sender = Address::senator(&"agent-one".parse()?);
        let event = senate_core::NewEvent::new(committee, Kind::Question, sender, "canary");
        let at = Utc::now();
        assert_eq!(
            eligible(&event.clone().into_event(Seq(1), at)),
            Some(public::MessageKind::Question)
        );
        for key in [
            meta::ORIGIN,
            meta::INTENDED,
            meta::PROMPT_ANSWER,
            meta::HOSTED_APPROVAL,
            meta::CHAIR_DISCUSSION,
            meta::RELAYED_FROM,
            meta::DIGEST,
            "unrecognized_metadata",
        ] {
            assert!(
                eligible(
                    &event
                        .clone()
                        .with_meta(key, serde_json::json!(true))
                        .into_event(Seq(1), at)
                )
                .is_none(),
                "{key}"
            );
        }
        Ok(())
    }
}
