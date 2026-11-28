//! Who sits in one Senate.
//!
//! A [`Roster`] lists the people the Senate knows and the agents seated in
//! it. The rules it keeps:
//!
//! - **A senator is one flat name.** People's handles and agents' names
//!   are senator names of this Senate and share one namespace: no name is
//!   both a person's and an agent's ([`RosterError::NameTaken`]). Nothing
//!   nests inside a name; only Senates nest.
//! - **No agent without a person.** An agent is seated by the person who
//!   first acts as it, and that person answers for it ([`Roster::owner_of`]).
//!   Nobody else acts as it.
//!
//! Who owns and answers for an agent, and how many principals a Senate has,
//! are being revised; this module keeps the rule above (the person who
//! seats an agent owns it) until then.
//!
//! # Who holds the Speaker's seat
//!
//! Every Senate has at most one Speaker ([`SpeakerSeat`]), and a Speaker is
//! always backed by a model: the built-in Speaker when the daemon runs it;
//! else the agent the roster names ([`Roster::named_speaker`], a Senate used
//! alone on one machine); else nobody, and the server only serves the log.
//! Both at once is refused. [`Roster::speaker_seat`] applies these rules; a
//! seat held by a senator is held by that agent's person
//! ([`Roster::seat_held_by`]).
//!
//! # Where a roster comes from
//!
//! People come from configuration ([`RosterConfig`], which is also how a
//! [`Roster`] is serialized). Agents may be listed there too, each with its
//! person, and are otherwise seated as they connect ([`Roster::seat`]): a
//! developer machine authenticates as a person, names the agent it speaks
//! as, and the server seats that agent on first contact if
//! [`Roster::may_act_as`] allows it.
//!
//! # Whichever agent
//!
//! `senator::*` is "whichever agent on this committee": a lookup with three
//! outcomes ([`Roster::any_agent_among`]), never a guess. One agent: it.
//! Several: an agent sender is refused with the candidates, a person's
//! message is put to the Speaker to pick. None: the Senate's person, when
//! it has one; else it is refused.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::address::Address;
use crate::ids::Name;
use crate::senate::check_not_keyword;
use crate::senator::SenatorName;
use crate::senator_id::SenatorId;

/// The name a Senate goes by when its configuration gives none.
pub const DEFAULT_SENATE_NAME: &str = "senate";

/// A person's identity on an outside platform: a chat platform's user id, an
/// identity provider's subject.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Account {
    pub platform: String,
    pub id: String,
}

/// The handles of a Senate's people: what a fold of the log needs to tell a
/// person's address from an agent's, now that both are one flat name
/// ([`Roster::people`]; [`Me::people`](crate::api::Me::people) on the
/// wire). Every `senator::` address that is not one of them is an agent's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct People(BTreeSet<Name>);

impl People {
    pub fn new(handles: impl IntoIterator<Item = Name>) -> Self {
        Self(handles.into_iter().collect())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn is_person(&self, senator: &SenatorName) -> bool {
        self.0.contains(senator.name())
    }

    /// Whether `address` names a person.
    pub fn is_person_address(&self, address: &Address) -> bool {
        address.as_senator().is_some_and(|s| self.is_person(s))
    }

    /// The agent `address` names: a senator that is not a person.
    pub fn as_agent<'a>(&self, address: &'a Address) -> Option<&'a SenatorName> {
        address.as_senator().filter(|s| !self.is_person(s))
    }

    /// Whether `address` names an agent.
    pub fn is_agent_address(&self, address: &Address) -> bool {
        self.as_agent(address).is_some()
    }

    pub fn handles(&self) -> impl Iterator<Item = &Name> {
        self.0.iter()
    }
}

/// What kind of principal a principal is. Only a person confirms decisions
/// and counts as "a person" wherever a rule says so; a team or a service
/// authenticates and runs agents like a person does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    #[default]
    Person,
    Team,
    Service,
}

impl PrincipalKind {
    pub fn is_person(self) -> bool {
        matches!(self, Self::Person)
    }
}

fn is_default_kind(kind: &PrincipalKind) -> bool {
    kind.is_person()
}

/// A principal of a Senate: who a token names. A person, or a team or a
/// service ([`PrincipalKind`]). A principal may run agents; the agent's
/// seat records which principal runs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    pub handle: Name,
    #[serde(default, skip_serializing_if = "is_default_kind")]
    pub kind: PrincipalKind,
    /// Other names people use for this person in chat ("Alice", "爱丽丝").
    /// Matched case-insensitively, as is the handle itself.
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub accounts: Vec<Account>,
    /// Optional committee read/write scope. `None` allows all committees (legacy default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committees: Option<BTreeSet<crate::CommitteeId>>,
}

impl Principal {
    /// A person.
    pub fn new(handle: Name) -> Self {
        Self {
            handle,
            kind: PrincipalKind::Person,
            names: Vec::new(),
            accounts: Vec::new(),
            committees: None,
        }
    }

    /// A principal of `kind`.
    pub fn of_kind(handle: Name, kind: PrincipalKind) -> Self {
        Self {
            kind,
            ..Self::new(handle)
        }
    }

    /// Restrict this principal's committee access list.
    pub fn with_committees(
        mut self,
        committees: impl IntoIterator<Item = crate::CommitteeId>,
    ) -> Self {
        self.committees = Some(committees.into_iter().collect());
        self
    }

    /// The principal as a senator.
    pub fn path(&self) -> SenatorName {
        SenatorName::new(self.handle.clone())
            .unwrap_or_else(|_| unreachable!("a listed person's handle is checked"))
    }

    /// This person's ids on `platform`, in the order listed: who to invite
    /// or mention there.
    pub fn account_ids<'a>(&'a self, platform: &'a str) -> impl Iterator<Item = &'a str> {
        self.accounts
            .iter()
            .filter(move |a| a.platform == platform)
            .map(|a| a.id.as_str())
    }
}

/// An agent seated in a Senate, and the person who answers for it.
///
/// On the wire and in configuration: `{ "name": "claude", "owner": "alice" }`.
/// `owner` is absent only for an agent seated again from the log of a
/// Senate with several people, before anyone has acted as it: the first
/// person to act as it then answers for it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SeatedAgent {
    pub name: SenatorName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<Name>,
}

impl SeatedAgent {
    pub fn new(name: SenatorName, owner: Name) -> Self {
        Self {
            name,
            owner: Some(owner),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RosterError {
    #[error("{0} is listed twice")]
    Duplicate(Name),
    #[error(
        "{name} is already {taken} in this Senate; a person's handle and an agent's name share one namespace"
    )]
    NameTaken { name: Name, taken: &'static str },
    #[error("{0} is a person; only agents are seated")]
    NotAnAgent(SenatorName),
    #[error("{agent} has no person: {owner} is not listed")]
    NoSenior { agent: SenatorName, owner: Name },
    #[error("{0} is not a person this Senate knows")]
    UnknownSenior(Name),
    #[error("{person} may not act as {senator}: {reason}")]
    NotYours {
        person: Name,
        senator: SenatorName,
        reason: &'static str,
    },
    #[error("the Speaker is always backed by a model, and {0} is a person")]
    SpeakerIsAPerson(SenatorName),
    #[error(
        "the Speaker {0} is not seated and this Senate has several people: list it under agents with its owner"
    )]
    SpeakerWithoutOwner(SenatorName),
    #[error("a Senate has one Speaker: {0} is named, and the built-in Speaker runs too")]
    TwoSpeakers(SenatorName),
    #[error("{0}")]
    Keyword(String),
    #[error("{0} cannot be its own secretary")]
    OwnSecretary(SenatorName),
    #[error("{0} is a principal; a secretary is an agent or the built-in session")]
    SecretaryIsAPrincipal(SenatorName),
    #[error("{0} is not a registered secretary of this principal")]
    ExecutionProxyNotSecretary(SenatorName),
}

/// Who holds the Speaker's seat of one Senate.
///
/// On the wire: `"built_in"`, or `{"senator": "claude"}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeakerSeat {
    /// The built-in Speaker (`senate-speaker`), in process with the server.
    BuiltIn,
    /// A senator: the agent a Senate used alone names. Its session does the
    /// Speaker's work and writes as `speaker`.
    Senator(SenatorName),
}

impl SpeakerSeat {
    /// The senator holding the seat; `None` for the built-in Speaker.
    pub fn senator(&self) -> Option<&SenatorName> {
        match self {
            Self::BuiltIn => None,
            Self::Senator(name) => Some(name),
        }
    }

    pub fn is_built_in(&self) -> bool {
        matches!(self, Self::BuiltIn)
    }

    /// Whether this Speaker judges task roots, so a bridge holds a root for
    /// its verdict ([`Me::triaged`](crate::api::Me::triaged)).
    ///
    /// Only the built-in Speaker does. A senator-held seat has no triage
    /// duty (its duties are to pick, cover and escalate), and the session
    /// holding it may be closed at any moment: a bridge that waited for its
    /// verdict would hold every question for nothing. So a Senate presided
    /// over by a senator delivers every root at once, and with nobody
    /// presiding nothing changes.
    pub fn triages(&self) -> bool {
        self.is_built_in()
    }
}

/// Why `senator::*` could not be turned into stored addresses.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    /// Several agents could get it. An agent sender is refused with the
    /// candidates and must name one; for a sender that cannot know, the
    /// Speaker picks among them, and asks the person when unsure.
    #[error("several agents could get this; name one of: {}", list(.candidates))]
    Ambiguous { candidates: Vec<SenatorName> },
    /// No agent is on the committee and the Senate has no one person to
    /// receive it instead.
    #[error("no agent is on this committee; name a person instead")]
    NoAgent,
}

fn list(names: &[SenatorName]) -> String {
    names
        .iter()
        .map(|p| Address::senator(p).to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// What kind of senator the Speaker (or any senator) is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SenatorKind {
    Person,
    Team,
    Service,
    Agent,
    /// The daemon's built-in session (the engine on a model).
    BuiltIn,
}

impl From<PrincipalKind> for SenatorKind {
    fn from(kind: PrincipalKind) -> Self {
        match kind {
            PrincipalKind::Person => Self::Person,
            PrincipalKind::Team => Self::Team,
            PrincipalKind::Service => Self::Service,
        }
    }
}

/// The Speaker as configuration names it: a senator id
/// (`senator::wendell`, `/acme::senator::wendell`), or, read for
/// compatibility, a bare name (`wendell`), which is a senator of this
/// Senate for a root and the parent's senator of that name for a child
/// (the caller that knows the parent decides, and says which it assumed).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SpeakerName {
    pub id: SenatorId,
    /// Written as a bare name.
    pub bare: bool,
}

impl SpeakerName {
    /// The Speaker `id`, written in full.
    pub fn id(id: SenatorId) -> Self {
        Self { id, bare: false }
    }
}

impl From<SenatorName> for SpeakerName {
    /// A bare name, as an older configuration wrote it.
    fn from(name: SenatorName) -> Self {
        Self {
            id: SenatorId::here(name),
            bare: true,
        }
    }
}

impl std::fmt::Display for SpeakerName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.bare {
            write!(f, "{}", self.id.name())
        } else {
            write!(f, "{}", self.id)
        }
    }
}

impl std::str::FromStr for SpeakerName {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.contains("::") {
            return s
                .parse::<SenatorId>()
                .map(Self::id)
                .map_err(|e| e.to_string());
        }
        s.parse::<SenatorName>()
            .map(Self::from)
            .map_err(|e| e.to_string())
    }
}

impl Serialize for SpeakerName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for SpeakerName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// A Speaker's explicit execution proxy grant to one of its secretaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionProxyGrant {
    pub secretary: SenatorName,
    /// Optional UTC expiry (TTL); `None` stays active until explicitly revoked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl ExecutionProxyGrant {
    /// Whether this grant is active at `now`.
    pub fn is_active(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        self.expires_at.is_none_or(|exp| exp > now)
    }
}

/// The Speaker's office of one Senate: who the Speaker is, and who works
/// its desk.
///
/// The desk is everything addressed to `speaker`. Its workers, in order:
/// the Speaker itself when it is an agent or the built-in session (it is
/// "present" with a live session), then the Speaker's secretaries; the
/// first present acts. A person Speaker is never assumed present: the
/// secretaries work the desk and leave the rest for them. With no worker
/// present the desk waits on the Speaker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Office {
    /// The Speaker.
    pub speaker: SenatorId,
    /// What kind of senator the Speaker is.
    pub kind: SenatorKind,
    /// Who works the desk, in order.
    #[serde(default)]
    pub desk: Vec<SenatorName>,
    /// Execution proxy grants currently held by secretaries on the desk.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub execution_proxies: Vec<ExecutionProxyGrant>,
    /// The daemon's built-in session, by name, when it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub built_in: Option<SenatorName>,
}

impl Office {
    /// The Speaker's name, when it is a senator of this Senate.
    pub fn speaker_here(&self) -> Option<&SenatorName> {
        self.speaker.senate().is_none().then(|| self.speaker.name())
    }

    /// The senator this Senate keeps the Speaker's desk under: whose
    /// secretaries work it, and who a matter the desk cannot settle goes
    /// to. The Speaker when it is a senator here; a person Speaker of the
    /// Senate above too, by the same handle (a person is recognised by the
    /// same handle across a tree of Senates, and a child keeps that
    /// person's desk for itself, in its own roster). `None` for an agent
    /// of another Senate: its desk is where it is seated.
    pub fn desk_owner(&self) -> Option<&SenatorName> {
        (self.speaker.senate().is_none() || self.kind == SenatorKind::Person)
            .then(|| self.speaker.name())
    }

    /// Whether `senator` works the desk (the Speaker itself when it is an
    /// agent, or one of its secretaries).
    pub fn works_desk(&self, senator: &SenatorName) -> bool {
        self.desk.contains(senator)
    }

    /// Whether the built-in session works the desk: it judges task roots
    /// (triage), so a bridge holds a root for its verdict.
    pub fn triages(&self) -> bool {
        self.built_in
            .as_ref()
            .is_some_and(|b| self.desk.contains(b))
    }

    /// The desk's agent workers: those a session can be (every worker but
    /// the built-in session).
    pub fn agent_workers(&self) -> impl Iterator<Item = &SenatorName> {
        self.desk
            .iter()
            .filter(|w| Some(*w) != self.built_in.as_ref())
    }
}

/// A roster as it is written in configuration and on the wire: the people,
/// optionally agents already seated, and the agent named Speaker. It is
/// unchecked; [`Roster::try_from`] applies the rules.
///
/// ```toml
/// speaker = "claude"
/// agents = [{ name = "claude", owner = "wendell" }]
///
/// [[principals]]
/// handle = "wendell"
/// names = ["Wendell"]
/// accounts = [{ platform = "chat", id = "u_1" }]
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterConfig {
    /// The Speaker: a senator id (`senator::alice`, or for a child Senate
    /// `/<parent>::senator::alice`), or a bare name read for compatibility.
    /// See [`Roster::office`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<SpeakerName>,
    /// The secretaries registered for a senator, in order: principal →
    /// secretaries. The Speaker's work its desk.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secretaries: BTreeMap<SenatorName, Vec<SenatorName>>,
    /// Execution proxy grants registered for a principal's secretaries.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub execution_proxies: BTreeMap<SenatorName, Vec<ExecutionProxyGrant>>,
    #[serde(default, alias = "seniors")]
    pub principals: Vec<Principal>,
    /// Agents seated from the start, each with its person. The rest are
    /// seated as they connect.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<SeatedAgent>,
}

/// Which agent a message for "whichever agent" reaches when the sender did
/// not say which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentOf {
    /// One agent could get it: it is the one.
    One(SenatorName),
    /// Several could. The sender must name one; a sender that cannot leaves
    /// the choice to the Speaker, who asks the person when unsure.
    Several(Vec<SenatorName>),
    /// None could: it goes to the person, when there is one person.
    None(Option<SenatorName>),
}

/// Whether a person's agents can be reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationPresence {
    /// At least one of the person's agents is present.
    Present,
    /// The person has agents here and none is present.
    Absent,
    /// The person has no agent here.
    NoAgents,
}

/// Serialized as a [`RosterConfig`]; decoding one applies the rules, so a
/// decoded roster is always a valid one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RosterConfig", into = "RosterConfig")]
pub struct Roster {
    speaker: Option<SpeakerName>,
    principals: Vec<Principal>,
    agents: Vec<SeatedAgent>,
    secretaries: BTreeMap<SenatorName, Vec<SenatorName>>,
    execution_proxies: BTreeMap<SenatorName, Vec<ExecutionProxyGrant>>,
    /// The daemon's built-in session, by name, when it runs. Not
    /// configuration: the server says.
    #[serde(skip)]
    built_in: Option<SenatorName>,
}

impl Roster {
    /// The roster of a Senate: its people, and agents already seated.
    pub fn new(principals: Vec<Principal>, agents: Vec<SeatedAgent>) -> Result<Self, RosterError> {
        let mut roster = Self {
            speaker: None,
            principals: Vec::with_capacity(principals.len()),
            agents: Vec::with_capacity(agents.len()),
            secretaries: BTreeMap::new(),
            execution_proxies: BTreeMap::new(),
            built_in: None,
        };
        for senior in principals {
            check_not_keyword(senior.handle.as_str(), "a person")
                .map_err(|e| RosterError::Keyword(e.to_string()))?;
            if roster.principals.iter().any(|s| s.handle == senior.handle) {
                return Err(RosterError::Duplicate(senior.handle));
            }
            roster.principals.push(senior);
        }
        for agent in agents {
            let name = agent.name.clone();
            let seated = match &agent.owner {
                Some(owner) => roster.seat(name.clone(), owner)?,
                None => roster.seat_again(name.clone())?,
            };
            if !seated {
                return Err(RosterError::Duplicate(name.name().clone()));
            }
        }
        Ok(roster)
    }

    /// A Senate of one person and no agent yet.
    pub fn of_person(person: Name) -> Self {
        Self::new(vec![Principal::new(person)], Vec::new())
            .unwrap_or_else(|_| unreachable!("one person is a valid roster"))
    }

    /// Name `speaker` this Senate's Speaker, under [`Roster::name_speaker`]'s
    /// rules.
    pub fn with_speaker(mut self, speaker: impl Into<SpeakerName>) -> Result<Self, RosterError> {
        self.name_speaker(Some(speaker.into()))?;
        Ok(self)
    }

    /// Name this Senate's Speaker, or `None`. A Speaker of this Senate (no
    /// Senate path) is a principal here, an agent of this Senate (an
    /// unattended root: seated now under the only principal when it is not
    /// seated), or the built-in session. A Speaker named with a Senate path
    /// is the parent's senator: the Senate above checks it when this one
    /// registers there.
    pub fn name_speaker(&mut self, speaker: Option<SpeakerName>) -> Result<(), RosterError> {
        if let Some(speaker) = &speaker
            && speaker.id.senate().is_none()
        {
            let name = speaker.id.name();
            let principal = self.principal(name.name()).is_some();
            let built_in = self.built_in.as_ref() == Some(name);
            if !principal && !built_in && self.owner_of(name).is_none() {
                match self.principals.as_slice() {
                    [only] => {
                        let owner = only.handle.clone();
                        self.seat(name.clone(), &owner)?;
                    }
                    _ => return Err(RosterError::SpeakerWithoutOwner(name.clone())),
                }
            }
        }
        self.speaker = speaker;
        Ok(())
    }

    /// The Speaker as configured.
    pub fn speaker(&self) -> Option<&SpeakerName> {
        self.speaker.as_ref()
    }

    /// The Speaker's name, when the Speaker is a senator of this Senate.
    pub fn named_speaker(&self) -> Option<&SenatorName> {
        self.speaker
            .as_ref()
            .filter(|s| s.id.senate().is_none())
            .map(|s| s.id.name())
    }

    /// Say that the daemon runs the built-in session under `name`: it is
    /// seated as an agent, run by the Speaker's principal when it is one
    /// here, else by the only principal.
    pub fn with_built_in(mut self, name: SenatorName) -> Result<Self, RosterError> {
        if self.principal(name.name()).is_some() {
            return Err(RosterError::NameTaken {
                name: name.name().clone(),
                taken: "a person",
            });
        }
        let owner = self
            .named_speaker()
            .filter(|s| self.principal(s.name()).is_some())
            .map(|s| s.name().clone())
            .or_else(|| self.only_person().map(|p| p.handle.clone()));
        match owner {
            Some(owner) => {
                self.seat(name.clone(), &owner)?;
            }
            None => {
                self.seat_again(name.clone())?;
            }
        }
        self.built_in = Some(name);
        Ok(self)
    }

    /// The built-in session's name, when the daemon runs it.
    pub fn built_in(&self) -> Option<&SenatorName> {
        self.built_in.as_ref()
    }

    /// What kind of senator `name` is here: a principal's kind, the
    /// built-in session, or an agent.
    pub fn kind_of(&self, name: &SenatorName) -> SenatorKind {
        if self.built_in.as_ref() == Some(name) {
            return SenatorKind::BuiltIn;
        }
        match self.principal(name.name()) {
            Some(p) => p.kind.into(),
            None => SenatorKind::Agent,
        }
    }

    /// The secretaries registered for `principal`, in order.
    pub fn secretaries_of(&self, principal: &SenatorName) -> &[SenatorName] {
        self.secretaries
            .get(principal)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Every secretary relation: principal → its secretaries, in order.
    pub fn secretaries(&self) -> &BTreeMap<SenatorName, Vec<SenatorName>> {
        &self.secretaries
    }

    /// Register `principal`'s secretaries, in order (empty: none). A senator
    /// is never its own secretary, a secretary is an agent of this Senate
    /// or the built-in session (never a principal), and a name appears
    /// once. Chains are not followed: a secretary's own secretaries do not
    /// act for `principal`.
    pub fn set_secretaries(
        &mut self,
        principal: SenatorName,
        secretaries: Vec<SenatorName>,
    ) -> Result<(), RosterError> {
        for (i, secretary) in secretaries.iter().enumerate() {
            if secretary == &principal {
                return Err(RosterError::OwnSecretary(principal));
            }
            if self.principal(secretary.name()).is_some() {
                return Err(RosterError::SecretaryIsAPrincipal(secretary.clone()));
            }
            if secretaries[..i].contains(secretary) {
                return Err(RosterError::Duplicate(secretary.name().clone()));
            }
        }
        if secretaries.is_empty() {
            self.secretaries.remove(&principal);
            self.execution_proxies.remove(&principal);
        } else {
            if let Some(grants) = self.execution_proxies.get_mut(&principal) {
                grants.retain(|grant| secretaries.contains(&grant.secretary));
                if grants.is_empty() {
                    self.execution_proxies.remove(&principal);
                }
            }
            self.secretaries.insert(principal, secretaries);
        }
        Ok(())
    }

    /// Execution proxy grants registered for `principal`.
    pub fn execution_proxies_of(&self, principal: &SenatorName) -> &[ExecutionProxyGrant] {
        self.execution_proxies
            .get(principal)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Every principal's execution proxy grants.
    pub fn execution_proxies(&self) -> &BTreeMap<SenatorName, Vec<ExecutionProxyGrant>> {
        &self.execution_proxies
    }

    /// Set `principal`'s execution proxy grants (must be a subset of its
    /// registered secretaries, with no duplicates).
    pub fn set_execution_proxies(
        &mut self,
        principal: SenatorName,
        grants: Vec<ExecutionProxyGrant>,
    ) -> Result<(), RosterError> {
        let secretaries = self.secretaries_of(&principal);
        for (i, grant) in grants.iter().enumerate() {
            if self.principal(grant.secretary.name()).is_some() {
                return Err(RosterError::SecretaryIsAPrincipal(grant.secretary.clone()));
            }
            if self.built_in.as_ref() == Some(&grant.secretary)
                || !secretaries.contains(&grant.secretary)
            {
                return Err(RosterError::ExecutionProxyNotSecretary(
                    grant.secretary.clone(),
                ));
            }
            if grants[..i]
                .iter()
                .any(|other| other.secretary == grant.secretary)
            {
                return Err(RosterError::Duplicate(grant.secretary.name().clone()));
            }
        }
        if grants.is_empty() {
            self.execution_proxies.remove(&principal);
        } else {
            self.execution_proxies.insert(principal, grants);
        }
        Ok(())
    }

    /// Whether `secretary` currently holds an active execution proxy grant
    /// from this Senate's Speaker office at `now`.
    pub fn is_active_execution_proxy(
        &self,
        secretary: &SenatorName,
        now: chrono::DateTime<chrono::Utc>,
    ) -> bool {
        let Some(office) = self.office() else {
            return false;
        };
        let Some(owner) = office.desk_owner() else {
            return false;
        };
        self.secretaries_of(owner).contains(secretary)
            && self
                .execution_proxies_of(owner)
                .iter()
                .any(|grant| &grant.secretary == secretary && grant.is_active(now))
    }

    /// The Speaker's office: who the Speaker is and who works its desk.
    /// `None` when no Speaker is named.
    pub fn office(&self) -> Option<Office> {
        let speaker = self.speaker.as_ref()?.id.clone();
        let here = speaker.senate().is_none();
        let name = speaker.name().clone();
        let kind = if here {
            self.kind_of(&name)
        } else {
            // A senator of the Senate above: the person whose machine this
            // is, known here by the same handle. One whose handle is no
            // principal here is not a person of this Senate (an agent above,
            // which a machine refuses at start when it can tell): its desk is
            // not kept here ([`Office::desk_owner`]).
            self.principal(name.name())
                .map_or(SenatorKind::Agent, |p| p.kind.into())
        };
        let mut desk = Vec::new();
        if here && matches!(kind, SenatorKind::Agent | SenatorKind::BuiltIn) {
            desk.push(name.clone());
        }
        for secretary in self.secretaries_of(&name) {
            if !desk.contains(secretary) {
                desk.push(secretary.clone());
            }
        }
        let mut execution_proxies = Vec::new();
        for grant in self.execution_proxies_of(&name) {
            if desk.contains(&grant.secretary)
                && !execution_proxies
                    .iter()
                    .any(|g: &ExecutionProxyGrant| g.secretary == grant.secretary)
            {
                execution_proxies.push(grant.clone());
            }
        }
        Some(Office {
            speaker,
            kind,
            desk,
            execution_proxies,
            built_in: self.built_in.clone(),
        })
    }

    /// Whether `identity` (who a token names) may write for the Speaker's
    /// office as `acted_by`: `acted_by` is the Speaker or works its desk,
    /// and `identity` is it (a principal Speaker) or runs it.
    pub fn may_act_for_office(&self, identity: &Name, acted_by: &SenatorName) -> bool {
        let Some(office) = self.office() else {
            return false;
        };
        let is_speaker = office.speaker.name() == acted_by
            && (office.speaker.senate().is_none() || self.principal(acted_by.name()).is_some());
        if !(is_speaker || office.works_desk(acted_by)) {
            return false;
        }
        self.may_act_as(identity, acted_by).is_ok()
    }

    /// Who works the desk now, as the older single seat: the desk's first
    /// worker (the built-in session as [`SpeakerSeat::BuiltIn`]), `None`
    /// when no one works it. `built_in` says whether the daemon runs the
    /// built-in session; with it and no Speaker named, the built-in session
    /// is the Speaker of an unattended root, as a Senate configured before
    /// Speakers were named had it.
    pub fn speaker_seat(&self, built_in: bool) -> Result<Option<SpeakerSeat>, RosterError> {
        let Some(office) = self.office() else {
            return Ok(built_in.then_some(SpeakerSeat::BuiltIn));
        };
        Ok(office.desk.first().map(|first| {
            if Some(first) == office.built_in.as_ref() {
                SpeakerSeat::BuiltIn
            } else {
                SpeakerSeat::Senator(first.clone())
            }
        }))
    }

    /// Whether the authenticated `person` holds `seat`: it is a desk worker
    /// they run. Nobody holds the built-in session's seat over HTTP.
    pub fn seat_held_by(&self, seat: &SpeakerSeat, person: &Name) -> bool {
        seat.senator()
            .is_some_and(|holder| self.owner_of(holder) == Some(person))
    }

    /// What `name` already is in this Senate, if anything.
    fn taken(&self, name: &Name) -> Option<&'static str> {
        if self.principals.iter().any(|s| &s.handle == name) {
            Some("a person")
        } else if self.agents.iter().any(|a| &a.name == name) {
            Some("an agent")
        } else {
            None
        }
    }

    /// Seat `agent` in this Senate under `owner`. `Ok(true)` when it was
    /// seated now, `Ok(false)` when it already sat here (an agent seated
    /// again from the log with no owner is given this one). Refused when
    /// the name is a person's, the owner is not listed, or another person
    /// answers for it.
    pub fn seat(&mut self, agent: SenatorName, owner: &Name) -> Result<bool, RosterError> {
        if self.is_person(&agent) {
            return Err(RosterError::NotAnAgent(agent));
        }
        if self.principal(owner).is_none() {
            return Err(RosterError::NoSenior {
                agent,
                owner: owner.clone(),
            });
        }
        if let Some(seated) = self.agents.iter_mut().find(|a| a.name == agent) {
            return match &seated.owner {
                Some(existing) if existing != owner => Err(RosterError::NotYours {
                    person: owner.clone(),
                    senator: agent,
                    reason: "another person answers for it",
                }),
                Some(_) => Ok(false),
                None => {
                    seated.owner = Some(owner.clone());
                    Ok(false)
                }
            };
        }
        self.agents.push(SeatedAgent::new(agent, owner.clone()));
        Ok(true)
    }

    /// Seat an agent the log has already seen, with no person
    /// authenticating: seats are not stored, so a restarted server seats
    /// again the agents it finds. With one person listed, the agent is
    /// theirs; otherwise the first person to act as it answers for it.
    pub fn seat_again(&mut self, agent: SenatorName) -> Result<bool, RosterError> {
        if let [only] = self.principals.as_slice() {
            let owner = only.handle.clone();
            return self.seat(agent, &owner);
        }
        if self.is_person(&agent) {
            return Err(RosterError::NotAnAgent(agent));
        }
        if self.agents.iter().any(|a| a.name == agent) {
            return Ok(false);
        }
        self.agents.push(SeatedAgent {
            name: agent,
            owner: None,
        });
        Ok(true)
    }

    /// Remove `agent` from this Senate. Returns whether it sat here. Its
    /// person stays: people are not seated, so they are not unseated.
    pub fn unseat(&mut self, agent: &SenatorName) -> bool {
        let before = self.agents.len();
        self.agents.retain(|a| &a.name != agent);
        self.agents.len() != before
    }

    /// May the authenticated `person` act as `senator` in this Senate?
    ///
    /// - a listed person may act as themselves;
    /// - a listed person may act as an agent they answer for, or one no
    ///   person answers for yet (seated now, under them, on first contact).
    ///
    /// Nobody acts as another person, or as another person's agent.
    pub fn may_act_as(&self, person: &Name, senator: &SenatorName) -> Result<(), RosterError> {
        if self.principal(person).is_none() {
            return Err(RosterError::UnknownSenior(person.clone()));
        }
        if senator == person {
            return Ok(());
        }
        let refuse = |reason| {
            Err(RosterError::NotYours {
                person: person.clone(),
                senator: senator.clone(),
                reason,
            })
        };
        if self.is_person(senator) {
            return refuse("it is another person");
        }
        match self.owner_of(senator) {
            Some(owner) if owner != person => refuse("another person answers for it"),
            _ => Ok(()),
        }
    }

    /// The unchecked form this roster is written as.
    pub fn to_config(&self) -> RosterConfig {
        RosterConfig {
            speaker: self.speaker.clone(),
            secretaries: self.secretaries.clone(),
            execution_proxies: self.execution_proxies.clone(),
            principals: self.principals.clone(),
            agents: self.agents.clone(),
        }
    }

    pub fn principals(&self) -> &[Principal] {
        &self.principals
    }

    /// The people's handles, for a fold that must tell people from agents.
    pub fn people(&self) -> People {
        People::new(self.principals.iter().map(|s| s.handle.clone()))
    }

    /// The agents seated in this Senate, in the order seated.
    pub fn agents(&self) -> Vec<SenatorName> {
        self.agents.iter().map(|a| a.name.clone()).collect()
    }

    /// The senators, people first, whose name starts with `written` or
    /// contains it, ignoring case: what an unknown name was probably meant
    /// to be. At most `limit`, those starting with it first.
    pub fn named_like(&self, written: &str, limit: usize) -> Vec<SenatorName> {
        let written = written.to_lowercase();
        if written.is_empty() {
            return Vec::new();
        }
        let names: Vec<SenatorName> = self
            .principals
            .iter()
            .map(Principal::path)
            .chain(self.agents())
            .collect();
        let starts = names
            .iter()
            .filter(|n| n.as_str().to_lowercase().starts_with(&written));
        let contains = names.iter().filter(|n| {
            let n = n.as_str().to_lowercase();
            !n.starts_with(&written) && n.contains(&written)
        });
        starts.chain(contains).take(limit).cloned().collect()
    }

    /// The agents seated in this Senate, with who answers for each.
    pub fn seated(&self) -> &[SeatedAgent] {
        &self.agents
    }

    /// The person who answers for `agent`, when it is seated and someone
    /// does.
    pub fn owner_of(&self, agent: &SenatorName) -> Option<&Name> {
        self.agents
            .iter()
            .find(|a| &a.name == agent)
            .and_then(|a| a.owner.as_ref())
    }

    pub fn principal(&self, handle: &Name) -> Option<&Principal> {
        self.principals.iter().find(|s| &s.handle == handle)
    }

    /// Fail-closed check whether `person` may read `committee`:
    /// `false` if unknown, or if `principal.committees` is set and excludes `committee`.
    pub fn can_read_committee(&self, person: &Name, committee: &crate::CommitteeId) -> bool {
        let Some(principal) = self.principal(person) else {
            return false;
        };
        principal
            .committees
            .as_ref()
            .is_none_or(|allowed| allowed.contains(committee))
    }

    /// The only person listed, when there is exactly one: whom a message
    /// for "whichever agent" reaches when there is no agent.
    pub fn only_person(&self) -> Option<&Principal> {
        match self.principals.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }

    /// Whether `senator` is a person here.
    pub fn is_person(&self, senator: &SenatorName) -> bool {
        self.principal(senator.name()).is_some()
    }

    /// Whether `senator` is an agent seated here.
    pub fn is_agent(&self, senator: &SenatorName) -> bool {
        self.agents.iter().any(|a| &a.name == senator)
    }

    /// Whether `address` names a person here.
    pub fn is_person_address(&self, address: &Address) -> bool {
        address.as_senator().is_some_and(|s| self.is_person(s))
    }

    /// Whether `address` names an agent: a senator that is not a person
    /// here. An agent not seated yet counts: a name that is not a person's
    /// is an agent's.
    pub fn is_agent_address(&self, address: &Address) -> bool {
        address.as_senator().is_some_and(|s| !self.is_person(s))
    }

    /// The person behind an account on an outside platform.
    pub fn by_account(&self, platform: &str, id: &str) -> Option<&Principal> {
        self.principals.iter().find(|s| {
            s.accounts
                .iter()
                .any(|a| a.platform == platform && a.id == id)
        })
    }

    /// A person by handle or any of their names.
    pub fn resolve(&self, name: &str) -> Option<&Principal> {
        let lower = name.trim().trim_start_matches('@').to_lowercase();
        self.principals.iter().find(|s| {
            s.handle.as_str() == lower || s.names.iter().any(|n| n.to_lowercase() == lower)
        })
    }

    /// Whether `senator` can be addressed in this Senate: a listed person,
    /// or an agent seated here.
    pub fn knows(&self, senator: &SenatorName) -> bool {
        self.is_person(senator) || self.is_agent(senator)
    }

    /// The agents `owner` answers for, in roster order.
    pub fn agents_of(&self, owner: &Name) -> Vec<&SenatorName> {
        self.agents
            .iter()
            .filter(|a| a.owner.as_ref() == Some(owner))
            .map(|a| &a.name)
            .collect()
    }

    /// Who "whichever agent" means among `candidates` (the agents on the
    /// committee), when the sender did not say which.
    pub fn any_agent_among(&self, candidates: &[SenatorName]) -> AgentOf {
        match candidates {
            [] => AgentOf::None(self.only_person().map(Principal::path)),
            [one] => AgentOf::One(one.clone()),
            several => AgentOf::Several(several.to_vec()),
        }
    }

    /// Turn an address a sender wrote into one that can be stored,
    /// `candidates` being the agents on the committee.
    ///
    /// Only `senator::*` changes: one agent, it; none, the only person
    /// (refused when there are several, [`ResolveError::NoAgent`]);
    /// several, [`ResolveError::Ambiguous`] carrying the candidates. Every
    /// other address is returned as it is; whether a named senator sits
    /// here is [`Roster::knows`]'s question, not this one's.
    pub fn resolve_address_among(
        &self,
        address: &Address,
        candidates: &[SenatorName],
    ) -> Result<Address, ResolveError> {
        if !address.is_wildcard() {
            return Ok(address.clone());
        }
        match self.any_agent_among(candidates) {
            AgentOf::One(agent) => Ok(Address::Senator(agent)),
            AgentOf::None(Some(person)) => Ok(Address::Senator(person)),
            AgentOf::None(None) => Err(ResolveError::NoAgent),
            AgentOf::Several(candidates) => Err(ResolveError::Ambiguous { candidates }),
        }
    }

    /// [`Roster::resolve_address_among`] every agent seated here.
    pub fn resolve_address(&self, address: &Address) -> Result<Address, ResolveError> {
        self.resolve_address_among(address, &self.agents())
    }

    /// [`Roster::resolve_address_among`] over an event's `to`, in order,
    /// dropping an address that resolution made a repeat of an earlier one.
    pub fn resolve_recipients_among(
        &self,
        to: &[Address],
        candidates: &[SenatorName],
    ) -> Result<Vec<Address>, ResolveError> {
        let mut out = Vec::with_capacity(to.len());
        for address in to {
            let resolved = self.resolve_address_among(address, candidates)?;
            if !out.contains(&resolved) {
                out.push(resolved);
            }
        }
        Ok(out)
    }

    /// [`Roster::resolve_recipients_among`] every agent seated here.
    pub fn resolve_recipients(&self, to: &[Address]) -> Result<Vec<Address>, ResolveError> {
        self.resolve_recipients_among(to, &self.agents())
    }

    /// A person's agents are reachable while any one of them is present.
    pub fn delegation_presence(
        &self,
        owner: &Name,
        is_present: impl Fn(&SenatorName) -> bool,
    ) -> DelegationPresence {
        let agents = self.agents_of(owner);
        if agents.is_empty() {
            DelegationPresence::NoAgents
        } else if agents.into_iter().any(is_present) {
            DelegationPresence::Present
        } else {
            DelegationPresence::Absent
        }
    }

    /// Whether the name `name` is free for a new agent: neither a person's
    /// nor a seated agent's.
    pub fn name_is_free(&self, name: &Name) -> Result<(), RosterError> {
        match self.taken(name) {
            Some(taken) => Err(RosterError::NameTaken {
                name: name.clone(),
                taken,
            }),
            None => Ok(()),
        }
    }
}

impl TryFrom<RosterConfig> for Roster {
    type Error = RosterError;
    fn try_from(config: RosterConfig) -> Result<Self, Self::Error> {
        let mut roster = Self::new(config.principals, config.agents)?;
        roster.name_speaker(config.speaker)?;
        for (principal, secretaries) in config.secretaries {
            roster.set_secretaries(principal, secretaries)?;
        }
        for (principal, grants) in config.execution_proxies {
            roster.set_execution_proxies(principal, grants)?;
        }
        Ok(roster)
    }
}

impl From<Roster> for RosterConfig {
    fn from(roster: Roster) -> Self {
        roster.to_config()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    fn senator(s: &str) -> SenatorName {
        s.parse().unwrap()
    }

    fn addr(s: &str) -> Address {
        s.parse().unwrap()
    }

    fn principals(handles: &[&str]) -> Vec<Principal> {
        handles.iter().map(|h| Principal::new(name(h))).collect()
    }

    fn agent(s: &str, owner: &str) -> SeatedAgent {
        SeatedAgent::new(senator(s), name(owner))
    }

    /// wendell runs claude and codex; alice runs gemini; carol runs none.
    fn team() -> Roster {
        Roster::new(
            principals(&["wendell", "alice", "carol"]),
            vec![
                agent("claude", "wendell"),
                agent("codex", "wendell"),
                agent("gemini", "alice"),
            ],
        )
        .unwrap()
    }

    #[test]
    fn a_senator_is_a_flat_name_and_the_roster_says_person_or_agent() {
        let roster = team();
        assert!(roster.is_person(&senator("wendell")));
        assert!(roster.is_agent(&senator("claude")));
        assert!(!roster.is_person(&senator("claude")));
        assert!(roster.knows(&senator("gemini")));
        assert!(!roster.knows(&senator("bob")));
        assert!(roster.is_agent_address(&addr("senator::copilot")));
        assert!(!roster.is_agent_address(&addr("senator::alice")));
        assert!(roster.is_person_address(&addr("senator::carol")));
        assert_eq!(roster.owner_of(&senator("gemini")), Some(&name("alice")));
        assert_eq!(roster.owner_of(&senator("wendell")), None);
    }

    #[test]
    fn a_name_like_an_unknown_one_starts_with_it_or_contains_it() {
        let roster = Roster::new(
            principals(&["wendell", "alice"]),
            vec![
                agent("claude-7ddea704-5624", "wendell"),
                agent("codex", "wendell"),
                agent("my-claude", "alice"),
            ],
        )
        .unwrap();
        assert_eq!(
            roster.named_like("claude-7ddea704", 5),
            [senator("claude-7ddea704-5624")]
        );
        assert_eq!(
            roster.named_like("CLAUDE", 5),
            [senator("claude-7ddea704-5624"), senator("my-claude")],
            "starting with it first, ignoring case"
        );
        assert_eq!(roster.named_like("w", 5), [senator("wendell")]);
        assert_eq!(roster.named_like("c", 1).len(), 1, "at most the limit");
        assert!(roster.named_like("zzz", 5).is_empty());
        assert!(roster.named_like("", 5).is_empty());
    }

    #[test]
    fn people_and_agents_share_one_namespace() {
        let mut roster = team();
        assert_eq!(
            roster.seat(senator("alice"), &name("wendell")),
            Err(RosterError::NotAnAgent(senator("alice")))
        );
        assert!(matches!(
            roster.name_is_free(&name("claude")),
            Err(RosterError::NameTaken { .. })
        ));
        let err = roster.name_is_free(&name("carol")).unwrap_err();
        assert!(err.to_string().contains("one namespace"), "{err}");
        assert!(roster.name_is_free(&name("copilot")).is_ok());
        assert!(matches!(
            Roster::new(principals(&["alice"]), vec![agent("alice", "alice")]),
            Err(RosterError::NotAnAgent(_))
        ));
    }

    #[test]
    fn no_agent_without_a_person_and_no_duplicates() {
        assert!(matches!(
            Roster::new(principals(&["alice"]), vec![agent("claude", "wendell")]),
            Err(RosterError::NoSenior { .. })
        ));
        assert_eq!(
            Roster::new(
                principals(&["wendell"]),
                vec![agent("claude", "wendell"), agent("claude", "wendell")]
            ),
            Err(RosterError::Duplicate(name("claude")))
        );
        assert_eq!(
            Roster::new(principals(&["wendell", "wendell"]), vec![]),
            Err(RosterError::Duplicate(name("wendell")))
        );
        assert!(matches!(
            Roster::new(principals(&["speaker"]), vec![]),
            Err(RosterError::Keyword(_))
        ));
    }

    #[test]
    fn seating_holds_an_agent_to_its_person() {
        let mut roster = Roster::of_person(name("wendell"));
        assert_eq!(roster.seat(senator("claude"), &name("wendell")), Ok(true));
        // First contact seats; every later contact finds it seated.
        assert_eq!(roster.seat(senator("claude"), &name("wendell")), Ok(false));
        assert_eq!(roster.seat(senator("codex"), &name("wendell")), Ok(true));
        assert_eq!(roster.agents(), [senator("claude"), senator("codex")]);
        assert!(matches!(
            roster.seat(senator("gemini"), &name("alice")),
            Err(RosterError::NoSenior { .. })
        ));

        let mut two = team();
        assert!(matches!(
            two.seat(senator("claude"), &name("alice")),
            Err(RosterError::NotYours { .. })
        ));
    }

    #[test]
    fn an_agent_seated_again_from_the_log_is_claimed_by_its_first_person() {
        // One person: theirs at once.
        let mut one = Roster::of_person(name("wendell"));
        assert_eq!(one.seat_again(senator("claude")), Ok(true));
        assert_eq!(one.owner_of(&senator("claude")), Some(&name("wendell")));
        // Several: nobody's until someone acts as it.
        let mut several = Roster::new(principals(&["wendell", "alice"]), vec![]).unwrap();
        assert_eq!(several.seat_again(senator("claude")), Ok(true));
        assert_eq!(several.owner_of(&senator("claude")), None);
        assert_eq!(
            several.may_act_as(&name("alice"), &senator("claude")),
            Ok(())
        );
        assert_eq!(several.seat(senator("claude"), &name("alice")), Ok(false));
        assert_eq!(several.owner_of(&senator("claude")), Some(&name("alice")));
        assert!(
            several
                .may_act_as(&name("wendell"), &senator("claude"))
                .is_err()
        );
    }

    #[test]
    fn unseating_removes_the_agent_and_keeps_the_person() {
        let mut roster = team();
        assert!(roster.unseat(&senator("codex")));
        assert!(!roster.unseat(&senator("codex")));
        assert!(!roster.knows(&senator("codex")));
        assert!(!roster.unseat(&senator("wendell")));
        assert!(roster.knows(&senator("wendell")));
    }

    #[test]
    fn a_person_acts_as_themselves_and_as_the_agents_they_answer_for() {
        let roster = team();
        let wendell = name("wendell");
        assert_eq!(roster.may_act_as(&wendell, &senator("wendell")), Ok(()));
        assert_eq!(roster.may_act_as(&wendell, &senator("claude")), Ok(()));
        // Not seated yet: allowed, and seated on first contact.
        assert!(!roster.knows(&senator("copilot")));
        assert_eq!(roster.may_act_as(&wendell, &senator("copilot")), Ok(()));
        assert!(matches!(
            roster.may_act_as(&wendell, &senator("gemini")),
            Err(RosterError::NotYours { .. })
        ));
        assert!(matches!(
            roster.may_act_as(&wendell, &senator("alice")),
            Err(RosterError::NotYours { .. })
        ));
        assert_eq!(
            roster.may_act_as(&name("bob"), &senator("bob")),
            Err(RosterError::UnknownSenior(name("bob")))
        );
    }

    #[test]
    fn whichever_agent_resolves_by_how_many_are_on_the_committee() {
        let roster = team();
        assert_eq!(
            roster.resolve_address_among(&addr("senator::*"), &[senator("gemini")]),
            Ok(addr("senator::gemini"))
        );
        let err = roster
            .resolve_address_among(&addr("senator::*"), &[senator("claude"), senator("codex")])
            .unwrap_err();
        assert_eq!(
            err,
            ResolveError::Ambiguous {
                candidates: vec![senator("claude"), senator("codex")],
            }
        );
        // The refusal reads as something a sender can act on.
        assert_eq!(
            err.to_string(),
            "several agents could get this; name one of: senator::claude, senator::codex"
        );
        // None on the committee: the person, when the Senate has one.
        let alone = Roster::of_person(name("wendell"));
        assert_eq!(
            alone.resolve_address_among(&addr("senator::*"), &[]),
            Ok(addr("senator::wendell"))
        );
        assert_eq!(
            roster.resolve_address_among(&addr("senator::*"), &[]),
            Err(ResolveError::NoAgent)
        );
    }

    #[test]
    fn resolving_recipients_leaves_concrete_addresses_alone_and_drops_repeats() {
        let roster = team();
        let to = [
            addr("senator::gemini"),
            addr("senator::*"),
            addr("speaker"),
            addr("guest::chat/u_1"),
            Address::Other("role:reviewer".into()),
            // Not seated here, and not this function's business.
            addr("senator::copilot"),
        ];
        let resolved = roster
            .resolve_recipients_among(&to, &[senator("gemini")])
            .unwrap();
        assert_eq!(
            resolved,
            vec![
                addr("senator::gemini"),
                addr("speaker"),
                addr("guest::chat/u_1"),
                Address::Other("role:reviewer".into()),
                addr("senator::copilot"),
            ]
        );
        assert!(matches!(
            roster.resolve_recipients(&[addr("speaker"), addr("senator::*")]),
            Err(ResolveError::Ambiguous { .. })
        ));
    }

    #[test]
    fn a_persons_agents_are_present_while_any_one_is() {
        let roster = team();
        let only_codex = |p: &SenatorName| p == &senator("codex");
        assert_eq!(
            roster.delegation_presence(&name("wendell"), only_codex),
            DelegationPresence::Present
        );
        assert_eq!(
            roster.delegation_presence(&name("alice"), only_codex),
            DelegationPresence::Absent
        );
        assert_eq!(
            roster.delegation_presence(&name("carol"), only_codex),
            DelegationPresence::NoAgents
        );
    }

    #[test]
    fn people_are_found_by_name_and_by_account() {
        let mut alice = Principal::new(name("alice"));
        alice.names = vec!["Alice".into(), "爱丽丝".into()];
        alice.accounts = vec![Account {
            platform: "chat".into(),
            id: "u_1".into(),
        }];
        let roster = Roster::new(vec![alice, Principal::new(name("wendell"))], vec![]).unwrap();
        assert_eq!(roster.resolve("@ALICE").unwrap().handle, name("alice"));
        assert_eq!(roster.resolve("爱丽丝").unwrap().handle, name("alice"));
        assert!(roster.resolve("bob").is_none());
        assert_eq!(
            roster.by_account("chat", "u_1").unwrap().handle,
            name("alice")
        );
        assert!(roster.by_account("other", "u_1").is_none());
    }

    #[test]
    fn the_speaker_is_named_by_id_and_its_desk_is_worked_in_order() {
        // Replaces "a Senate has the built-in Speaker, a named agent or
        // none": the Speaker is any senator named by id; the built-in
        // session and agents work its desk as secretaries.
        let id = |s: &str| SpeakerName::id(s.parse().unwrap());
        let plain = Roster::of_person(name("wendell"));
        assert_eq!(plain.office(), None);

        // A person is the Speaker; claude and the built-in session work
        // the desk, in order.
        let mut desk = plain
            .clone()
            .with_built_in(senator("clerk"))
            .unwrap()
            .with_speaker(id("senator::wendell"))
            .unwrap();
        desk.set_secretaries(
            senator("wendell"),
            vec![senator("claude"), senator("clerk")],
        )
        .unwrap();
        let office = desk.office().unwrap();
        assert_eq!(office.kind, SenatorKind::Person);
        assert_eq!(office.speaker_here(), Some(&senator("wendell")));
        assert_eq!(office.desk, [senator("claude"), senator("clerk")]);
        assert!(office.triages());
        assert_eq!(
            office.agent_workers().collect::<Vec<_>>(),
            [&senator("claude")]
        );
        assert_eq!(desk.owner_of(&senator("clerk")), Some(&name("wendell")));
        assert!(desk.may_act_for_office(&name("wendell"), &senator("wendell")));
        assert!(desk.may_act_for_office(&name("wendell"), &senator("claude")));
        assert!(!desk.may_act_for_office(&name("wendell"), &senator("codex")));

        // An unattended root: an agent is the Speaker and works its own desk.
        let agent = plain.clone().with_speaker(id("senator::claude")).unwrap();
        let office = agent.office().unwrap();
        assert_eq!(office.kind, SenatorKind::Agent);
        assert_eq!(office.desk, [senator("claude")]);
        assert!(!office.triages());
        assert_eq!(
            agent.speaker_seat(false),
            Ok(Some(SpeakerSeat::Senator(senator("claude"))))
        );
        // The built-in session as the Speaker itself.
        let built_in = plain
            .clone()
            .with_built_in(senator("clerk"))
            .unwrap()
            .with_speaker(id("senator::clerk"))
            .unwrap();
        let office = built_in.office().unwrap();
        assert_eq!(office.kind, SenatorKind::BuiltIn);
        assert!(office.triages());
        assert_eq!(built_in.speaker_seat(true), Ok(Some(SpeakerSeat::BuiltIn)));

        // A child's Speaker: the parent's senator, the person of this
        // machine.
        let child = plain
            .clone()
            .with_speaker(id("/acme::senator::wendell"))
            .unwrap();
        let office = child.office().unwrap();
        assert_eq!(office.kind, SenatorKind::Person);
        assert_eq!(office.speaker_here(), None);
        assert!(
            office.desk.is_empty(),
            "no secretary: the desk waits on the Speaker"
        );
        // Its desk is kept here, under the person's handle, and the agents
        // appointed to it work it.
        assert_eq!(office.desk_owner(), Some(&senator("wendell")));
        let mut staffed = child.clone();
        staffed.seat(senator("claude"), &name("wendell")).unwrap();
        staffed
            .set_secretaries(senator("wendell"), vec![senator("claude")])
            .unwrap();
        let office = staffed.office().unwrap();
        assert_eq!(office.desk, [senator("claude")]);
        assert!(staffed.may_act_for_office(&name("wendell"), &senator("claude")));
        // A senator above that is no person here (an agent of the Senate
        // above): its desk is not kept here.
        let above_agent = plain
            .clone()
            .with_speaker(id("/acme::senator::claude-up"))
            .unwrap();
        let office = above_agent.office().unwrap();
        assert_eq!(office.kind, SenatorKind::Agent);
        assert_eq!(office.desk_owner(), None);
        // A senator of this Senate keeps its desk here, whatever its kind.
        assert_eq!(
            agent.office().unwrap().desk_owner(),
            Some(&senator("claude"))
        );

        // A configuration written before Speakers were ids: a bare name.
        let bare: SpeakerName = "claude".parse().unwrap();
        assert!(bare.bare);
        assert_eq!(bare.id, "senator::claude".parse().unwrap());
        assert_eq!(bare.to_string(), "claude");
        // One that names the built-in Speaker only by running it.
        assert_eq!(plain.speaker_seat(true), Ok(Some(SpeakerSeat::BuiltIn)));
        assert_eq!(plain.speaker_seat(false), Ok(None));
    }

    #[test]
    fn a_secretary_is_an_agent_never_its_own_principal() {
        let mut roster = Roster::of_person(name("wendell"));
        assert_eq!(
            roster.set_secretaries(senator("claude"), vec![senator("claude")]),
            Err(RosterError::OwnSecretary(senator("claude")))
        );
        assert_eq!(
            roster.set_secretaries(senator("claude"), vec![senator("wendell")]),
            Err(RosterError::SecretaryIsAPrincipal(senator("wendell")))
        );
        roster
            .set_secretaries(senator("claude"), vec![senator("codex"), senator("gemini")])
            .unwrap();
        assert_eq!(
            roster.secretaries_of(&senator("claude")),
            [senator("codex"), senator("gemini")]
        );
        roster.set_secretaries(senator("claude"), vec![]).unwrap();
        assert!(roster.secretaries_of(&senator("claude")).is_empty());
    }

    #[test]
    fn a_seat_held_by_a_senator_is_held_by_its_person() {
        let roster = team();
        let seat = SpeakerSeat::Senator(senator("claude"));
        assert!(roster.seat_held_by(&seat, &name("wendell")));
        assert!(!roster.seat_held_by(&seat, &name("alice")));
        assert!(!seat.triages(), "a senator-held seat holds no root back");
        assert!(!roster.seat_held_by(&SpeakerSeat::BuiltIn, &name("wendell")));
        assert!(SpeakerSeat::BuiltIn.triages());
        assert_eq!(
            serde_json::to_value(&seat).unwrap(),
            serde_json::json!({ "senator": "claude" })
        );
        assert_eq!(
            serde_json::to_value(SpeakerSeat::BuiltIn).unwrap(),
            serde_json::json!("built_in")
        );
        let back: SpeakerSeat =
            serde_json::from_value(serde_json::json!({ "senator": "claude" })).unwrap();
        assert_eq!(back, seat);
    }

    #[test]
    fn a_roster_is_configured_and_serialized_in_one_checked_form() {
        let json = serde_json::json!({
            "speaker": "claude",
            "principals": [
                {
                    "handle": "wendell",
                    "names": ["Wendell"],
                    "accounts": [
                        { "platform": "chat", "id": "u_1" },
                        { "platform": "chat", "id": "u_2" },
                        { "platform": "sso", "id": "w@example.com" }
                    ]
                },
                { "handle": "alice" }
            ],
            "agents": [{ "name": "claude", "owner": "wendell" }]
        });
        let roster: Roster = serde_json::from_value(json).unwrap();
        assert_eq!(roster.agents(), [senator("claude")]);
        assert_eq!(roster.named_speaker(), Some(&senator("claude")));
        let wendell = roster.principal(&name("wendell")).unwrap();
        assert_eq!(
            wendell.account_ids("chat").collect::<Vec<_>>(),
            ["u_1", "u_2"]
        );
        assert_eq!(roster.by_account("sso", "w@example.com"), Some(wendell));

        let back: Roster = serde_json::from_value(serde_json::to_value(&roster).unwrap()).unwrap();
        assert_eq!(back, roster);
        assert_eq!(Roster::try_from(roster.to_config()), Ok(roster));

        // People alone are a roster; agents are seated as they connect.
        let people: Roster =
            serde_json::from_value(serde_json::json!({ "principals": [{ "handle": "alice" }] }))
                .unwrap();
        assert!(people.agents().is_empty());
    }

    #[test]
    fn a_configured_roster_that_breaks_a_rule_does_not_decode() {
        for bad in [
            serde_json::json!({ "principals": [{ "handle": "alice" }], "agents": [{ "name": "claude", "owner": "wendell" }] }),
            serde_json::json!({ "principals": [{ "handle": "alice" }], "agents": [{ "name": "alice", "owner": "alice" }] }),
            serde_json::json!({ "principals": [{ "handle": "alice" }], "agents": [{ "name": "alice/claude", "owner": "alice" }] }),
            serde_json::json!({ "principals": [{ "handle": "alice" }], "agents": ["alice/claude"] }),
            serde_json::json!({ "principals": [{ "handle": "alice" }, { "handle": "alice" }] }),
            serde_json::json!({ "principals": [{ "handle": "alice" }], "secretaries": { "alice": ["alice"] } }),
            serde_json::json!({
                "principals": [{ "handle": "alice" }, { "handle": "bob" }],
                "secretaries": { "alice": ["bob"] }
            }),
            serde_json::json!({ "principals": [{ "handle": "alice" }], "speaker": "acme::senator::alice" }),
            serde_json::json!({
                "principals": [{ "handle": "alice" }],
                "agents": [
                    { "name": "claude", "owner": "alice" },
                    { "name": "claude", "owner": "alice" }
                ]
            }),
        ] {
            assert!(
                serde_json::from_value::<Roster>(bad.clone()).is_err(),
                "{bad}"
            );
        }
    }
}
