//! Who sits in one Senate.
//!
//! A [`Roster`] lists the people the Senate knows and the agents seated in
//! it. The rules it keeps:
//!
//! - **People are global.** A person is listed once, by handle, and is the
//!   same senator in every Senate.
//! - **No agent without a person.** Every agent's path starts with a listed
//!   person, who is accountable for it.
//! - **One level per Senate.** The top Senate seats agents directly under
//!   their person (`wendell/claude`). A Senate presided over by a senator
//!   seats only agents directly under that senator
//!   (`wendell/claude/reviewer` under `wendell/claude`). An agent's own
//!   subagents are never seated beside it.
//!
//! A person and the agents under their name are their **delegation**: a
//! grouping by path, not a Senate of its own.
//!
//! # Where a roster comes from
//!
//! People come from configuration ([`RosterConfig`], which is also how a
//! [`Roster`] is serialized). Agents may be listed there too, and are
//! otherwise seated as they connect ([`Roster::seat`]): a developer machine
//! authenticates as a person, names the agent it speaks as, and the server
//! seats that agent on first contact if [`Roster::may_act_as`] allows it.
//!
//! # One agent is never assumed
//!
//! A person may have any number of agents here, so "this person's agent" is
//! a lookup with three outcomes ([`Roster::agent_of`]), never a derivation
//! from the handle. [`Roster::resolve_address`] applies it to the wildcard
//! address `senator::<person>/*`.

use serde::{Deserialize, Serialize};

use crate::address::Address;
use crate::ids::Name;
use crate::senator::SenatorPath;

/// A person's identity on an outside platform: a chat platform's user id, an
/// identity provider's subject.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Account {
    pub platform: String,
    pub id: String,
}

/// A person the Senate knows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Senior {
    pub handle: Name,
    /// Other names people use for this person in chat ("Alice", "爱丽丝").
    /// Matched case-insensitively, as is the handle itself.
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub accounts: Vec<Account>,
}

impl Senior {
    pub fn new(handle: Name) -> Self {
        Self {
            handle,
            names: Vec::new(),
            accounts: Vec::new(),
        }
    }

    pub fn path(&self) -> SenatorPath {
        SenatorPath::senior(self.handle.clone())
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

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RosterError {
    #[error("{0} is listed twice")]
    Duplicate(SenatorPath),
    #[error("{0} is a person; only agents are seated")]
    NotAnAgent(SenatorPath),
    #[error("{agent} has no person: {owner} is not listed")]
    NoSenior { agent: SenatorPath, owner: Name },
    #[error("{agent} does not sit in this Senate: {reason}")]
    WrongLevel {
        agent: SenatorPath,
        reason: &'static str,
    },
    #[error("a Senate is presided over by an agent, and {0} is a person")]
    PresiderIsAPerson(SenatorPath),
    #[error("{0} is not a person this Senate knows")]
    UnknownSenior(Name),
    #[error("{person} may not act as {senator}: it is not under their name")]
    NotYours { person: Name, senator: SenatorPath },
}

/// Why `senator::<person>/*` could not be turned into one address.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    /// The person has several agents here. An agent sender is refused with
    /// the candidates and must name one; for a sender that cannot know, the
    /// Speaker picks among them, and asks the person when unsure.
    #[error("{owner} has several agents here; name one of: {}", list(.candidates))]
    Ambiguous {
        owner: Name,
        candidates: Vec<SenatorPath>,
    },
    #[error("{0} is not a person this Senate knows")]
    UnknownSenior(Name),
}

fn list(paths: &[SenatorPath]) -> String {
    paths
        .iter()
        .map(|p| Address::senator(p).to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// A roster as it is written in configuration and on the wire: the people,
/// and optionally agents already seated. It is unchecked;
/// [`Roster::try_from`] applies the rules.
///
/// ```toml
/// agents = ["wendell/claude"]
///
/// [[seniors]]
/// handle = "wendell"
/// names = ["Wendell"]
/// accounts = [{ platform = "chat", id = "u_1" }]
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterConfig {
    /// The senator presiding over this Senate. Absent for the top Senate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presider: Option<SenatorPath>,
    #[serde(default)]
    pub seniors: Vec<Senior>,
    /// Agents seated from the start. The rest are seated as they connect.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<SenatorPath>,
}

/// Which agent a message for "this person's agent" reaches when the sender
/// did not say which one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentOf {
    /// The person has one agent here: it is the one.
    One(SenatorPath),
    /// The person has several. The sender must name one; a sender that
    /// cannot leaves the choice to the Speaker, who asks the person when
    /// unsure.
    Several(Vec<SenatorPath>),
    /// The person has no agent here: the message goes to the person.
    None(SenatorPath),
}

/// Whether a person's delegation can be reached through an agent.
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
    presider: Option<SenatorPath>,
    seniors: Vec<Senior>,
    agents: Vec<SenatorPath>,
}

impl Roster {
    /// The roster of the top Senate, whose Speaker is not a senator.
    pub fn top(seniors: Vec<Senior>, agents: Vec<SenatorPath>) -> Result<Self, RosterError> {
        Self::build(None, seniors, agents)
    }

    /// The roster of the Senate that `presider` presides over as its Speaker.
    pub fn under(
        presider: SenatorPath,
        seniors: Vec<Senior>,
        agents: Vec<SenatorPath>,
    ) -> Result<Self, RosterError> {
        if presider.is_senior() {
            return Err(RosterError::PresiderIsAPerson(presider));
        }
        Self::build(Some(presider), seniors, agents)
    }

    fn build(
        presider: Option<SenatorPath>,
        seniors: Vec<Senior>,
        agents: Vec<SenatorPath>,
    ) -> Result<Self, RosterError> {
        for (i, senior) in seniors.iter().enumerate() {
            if seniors[..i].iter().any(|s| s.handle == senior.handle) {
                return Err(RosterError::Duplicate(senior.path()));
            }
        }
        if let Some(presider) = &presider
            && !seniors.iter().any(|s| &s.handle == presider.owner())
        {
            return Err(RosterError::NoSenior {
                agent: presider.clone(),
                owner: presider.owner().clone(),
            });
        }
        let mut roster = Self {
            presider,
            seniors,
            agents: Vec::with_capacity(agents.len()),
        };
        for agent in agents {
            if !roster.seat(agent.clone())? {
                return Err(RosterError::Duplicate(agent));
            }
        }
        Ok(roster)
    }

    /// Whether `agent` could sit in this Senate: it is an agent, its person
    /// is listed, and it is at this Senate's level.
    fn check_seat(&self, agent: &SenatorPath) -> Result<(), RosterError> {
        if agent.is_senior() {
            return Err(RosterError::NotAnAgent(agent.clone()));
        }
        if self.senior(agent.owner()).is_none() {
            return Err(RosterError::NoSenior {
                agent: agent.clone(),
                owner: agent.owner().clone(),
            });
        }
        match &self.presider {
            None if agent.level() != 1 => Err(RosterError::WrongLevel {
                agent: agent.clone(),
                reason: "the top Senate seats agents directly under their person",
            }),
            Some(presider) if agent.presider().as_ref() != Some(presider) => {
                Err(RosterError::WrongLevel {
                    agent: agent.clone(),
                    reason: "a nested Senate seats agents directly under its presider",
                })
            }
            _ => Ok(()),
        }
    }

    /// Seat `agent` in this Senate, under the same rules a configured roster
    /// is held to. `Ok(true)` when it was seated now, `Ok(false)` when it
    /// already sat here: seating is what happens on an agent's first
    /// contact, and every later contact asks again.
    pub fn seat(&mut self, agent: SenatorPath) -> Result<bool, RosterError> {
        self.check_seat(&agent)?;
        if self.agents.contains(&agent) {
            return Ok(false);
        }
        self.agents.push(agent);
        Ok(true)
    }

    /// Remove `agent` from this Senate. Returns whether it sat here. Its
    /// person stays: people are not seated, so they are not unseated.
    pub fn unseat(&mut self, agent: &SenatorPath) -> bool {
        let before = self.agents.len();
        self.agents.retain(|a| a != agent);
        self.agents.len() != before
    }

    /// May the authenticated `person` act as `senator` in this Senate?
    ///
    /// A developer machine authenticates as a person; each thing it then
    /// does as an agent (open a stream, send a heartbeat, write a
    /// checkpoint, append an event) names the agent. This is the check the
    /// server applies to that name and the bridge applies before sending:
    ///
    /// - a listed person may act as themselves;
    /// - a listed person may act as an agent under their own name that
    ///   could sit in this Senate: directly under them in the top Senate,
    ///   directly under the presider in a nested one.
    ///
    /// The agent need not be seated yet. A caller that gets `Ok` for an
    /// agent seats it with [`Roster::seat`], which is how an agent is seated
    /// on first contact.
    pub fn may_act_as(&self, person: &Name, senator: &SenatorPath) -> Result<(), RosterError> {
        if self.senior(person).is_none() {
            return Err(RosterError::UnknownSenior(person.clone()));
        }
        if senator.owner() != person {
            return Err(RosterError::NotYours {
                person: person.clone(),
                senator: senator.clone(),
            });
        }
        if senator.is_senior() {
            return Ok(());
        }
        self.check_seat(senator)
    }

    /// The unchecked form this roster is written as.
    pub fn to_config(&self) -> RosterConfig {
        RosterConfig {
            presider: self.presider.clone(),
            seniors: self.seniors.clone(),
            agents: self.agents.clone(),
        }
    }

    /// The senator presiding over this Senate as its Speaker. `None` in the
    /// top Senate.
    pub fn presider(&self) -> Option<&SenatorPath> {
        self.presider.as_ref()
    }

    pub fn seniors(&self) -> &[Senior] {
        &self.seniors
    }

    /// The agents seated in this Senate.
    pub fn agents(&self) -> &[SenatorPath] {
        &self.agents
    }

    pub fn senior(&self, handle: &Name) -> Option<&Senior> {
        self.seniors.iter().find(|s| &s.handle == handle)
    }

    /// The person behind an account on an outside platform.
    pub fn by_account(&self, platform: &str, id: &str) -> Option<&Senior> {
        self.seniors.iter().find(|s| {
            s.accounts
                .iter()
                .any(|a| a.platform == platform && a.id == id)
        })
    }

    /// A person by handle or any of their names.
    pub fn resolve(&self, name: &str) -> Option<&Senior> {
        let lower = name.trim().trim_start_matches('@').to_lowercase();
        self.seniors.iter().find(|s| {
            s.handle.as_str() == lower || s.names.iter().any(|n| n.to_lowercase() == lower)
        })
    }

    /// Whether `senator` can be addressed in this Senate: a listed person,
    /// or an agent seated here.
    pub fn knows(&self, senator: &SenatorPath) -> bool {
        if senator.is_senior() {
            self.senior(senator.owner()).is_some()
        } else {
            self.agents.contains(senator)
        }
    }

    /// The person's agents seated here, in roster order.
    pub fn agents_of(&self, owner: &Name) -> Vec<&SenatorPath> {
        self.agents.iter().filter(|a| a.owner() == owner).collect()
    }

    /// Who "this person's agent" means when the sender did not say which.
    pub fn agent_of(&self, owner: &Name) -> Result<AgentOf, RosterError> {
        let senior = self
            .senior(owner)
            .ok_or_else(|| RosterError::UnknownSenior(owner.clone()))?;
        let agents = self.agents_of(owner);
        Ok(match agents.as_slice() {
            [] => AgentOf::None(senior.path()),
            [one] => AgentOf::One((*one).clone()),
            _ => AgentOf::Several(agents.into_iter().cloned().collect()),
        })
    }

    /// Turn an address a sender wrote into one that can be stored.
    ///
    /// Only `senator::<person>/*` changes:
    ///
    /// - the person has one agent here: that agent;
    /// - none: the person;
    /// - several: [`ResolveError::Ambiguous`], carrying the candidates.
    ///
    /// Every other address is returned as it is; whether a named senator
    /// sits here is [`Roster::knows`]'s question, not this one's.
    pub fn resolve_address(&self, address: &Address) -> Result<Address, ResolveError> {
        let Address::AgentOf(owner) = address else {
            return Ok(address.clone());
        };
        match self.agent_of(owner) {
            Ok(AgentOf::One(agent)) => Ok(Address::Senator(agent)),
            Ok(AgentOf::None(person)) => Ok(Address::Senator(person)),
            Ok(AgentOf::Several(candidates)) => Err(ResolveError::Ambiguous {
                owner: owner.clone(),
                candidates,
            }),
            Err(_) => Err(ResolveError::UnknownSenior(owner.clone())),
        }
    }

    /// [`Roster::resolve_address`] over an event's `to`, in order, dropping
    /// an address that resolution made a repeat of an earlier one. The first
    /// wildcard that cannot be resolved is the error.
    pub fn resolve_recipients(&self, to: &[Address]) -> Result<Vec<Address>, ResolveError> {
        let mut out = Vec::with_capacity(to.len());
        for address in to {
            let resolved = self.resolve_address(address)?;
            if !out.contains(&resolved) {
                out.push(resolved);
            }
        }
        Ok(out)
    }

    /// A delegation is reachable while any one of its agents is present.
    pub fn delegation_presence(
        &self,
        owner: &Name,
        is_present: impl Fn(&SenatorPath) -> bool,
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
}

impl TryFrom<RosterConfig> for Roster {
    type Error = RosterError;
    fn try_from(config: RosterConfig) -> Result<Self, Self::Error> {
        match config.presider {
            None => Self::top(config.seniors, config.agents),
            Some(presider) => Self::under(presider, config.seniors, config.agents),
        }
    }
}

impl From<Roster> for RosterConfig {
    fn from(roster: Roster) -> Self {
        Self {
            presider: roster.presider,
            seniors: roster.seniors,
            agents: roster.agents,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    fn path(s: &str) -> SenatorPath {
        s.parse().unwrap()
    }

    fn seniors(handles: &[&str]) -> Vec<Senior> {
        handles.iter().map(|h| Senior::new(name(h))).collect()
    }

    fn top() -> Roster {
        Roster::top(
            seniors(&["wendell", "alice", "carol"]),
            vec![
                path("wendell/claude"),
                path("wendell/codex"),
                path("alice/claude"),
            ],
        )
        .unwrap()
    }

    #[test]
    fn the_top_senate_seats_agents_directly_under_their_person() {
        let roster = top();
        assert_eq!(roster.presider(), None);
        assert!(roster.knows(&path("wendell")));
        assert!(roster.knows(&path("wendell/claude")));
        assert!(!roster.knows(&path("wendell/claude/reviewer")));
        assert!(!roster.knows(&path("bob")));

        let err = Roster::top(seniors(&["wendell"]), vec![path("wendell/claude/reviewer")]);
        assert!(matches!(err, Err(RosterError::WrongLevel { .. })));
    }

    #[test]
    fn a_nested_senate_seats_only_agents_directly_under_its_presider() {
        let roster = Roster::under(
            path("wendell/claude"),
            seniors(&["wendell"]),
            vec![
                path("wendell/claude/reviewer"),
                path("wendell/claude/codex"),
            ],
        )
        .unwrap();
        assert_eq!(roster.presider(), Some(&path("wendell/claude")));
        // The person is the same senator here as in the Senate above.
        assert!(roster.knows(&path("wendell")));
        assert!(roster.knows(&path("wendell/claude/reviewer")));

        for stray in ["wendell/codex", "wendell/codex/x", "wendell/claude/a/b"] {
            let err = Roster::under(
                path("wendell/claude"),
                seniors(&["wendell"]),
                vec![path(stray)],
            );
            assert!(
                matches!(err, Err(RosterError::WrongLevel { .. })),
                "{stray}"
            );
        }
    }

    #[test]
    fn a_person_cannot_preside_and_a_presider_needs_its_person() {
        assert_eq!(
            Roster::under(path("wendell"), seniors(&["wendell"]), vec![]),
            Err(RosterError::PresiderIsAPerson(path("wendell")))
        );
        assert!(matches!(
            Roster::under(path("wendell/claude"), seniors(&["alice"]), vec![]),
            Err(RosterError::NoSenior { .. })
        ));
    }

    #[test]
    fn no_agent_without_a_person_and_no_duplicates() {
        assert!(matches!(
            Roster::top(seniors(&["alice"]), vec![path("wendell/claude")]),
            Err(RosterError::NoSenior { .. })
        ));
        assert_eq!(
            Roster::top(seniors(&["wendell"]), vec![path("wendell")]),
            Err(RosterError::NotAnAgent(path("wendell")))
        );
        assert_eq!(
            Roster::top(
                seniors(&["wendell"]),
                vec![path("wendell/claude"), path("wendell/claude")]
            ),
            Err(RosterError::Duplicate(path("wendell/claude")))
        );
        assert_eq!(
            Roster::top(seniors(&["wendell", "wendell"]), vec![]),
            Err(RosterError::Duplicate(path("wendell")))
        );
    }

    #[test]
    fn an_unnamed_agent_resolves_by_how_many_the_person_has() {
        let roster = top();
        assert_eq!(
            roster.agent_of(&name("alice")),
            Ok(AgentOf::One(path("alice/claude")))
        );
        assert_eq!(
            roster.agent_of(&name("wendell")),
            Ok(AgentOf::Several(vec![
                path("wendell/claude"),
                path("wendell/codex")
            ]))
        );
        assert_eq!(
            roster.agent_of(&name("carol")),
            Ok(AgentOf::None(path("carol")))
        );
        assert_eq!(
            roster.agent_of(&name("bob")),
            Err(RosterError::UnknownSenior(name("bob")))
        );
    }

    #[test]
    fn a_delegation_is_present_while_any_agent_is() {
        let roster = top();
        let only_codex = |p: &SenatorPath| p == &path("wendell/codex");
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
        let mut alice = Senior::new(name("alice"));
        alice.names = vec!["Alice".into(), "爱丽丝".into()];
        alice.accounts = vec![Account {
            platform: "chat".into(),
            id: "u_1".into(),
        }];
        let roster = Roster::top(vec![alice, Senior::new(name("wendell"))], vec![]).unwrap();

        assert_eq!(roster.resolve("@ALICE").unwrap().handle, name("alice"));
        assert_eq!(roster.resolve("爱丽丝").unwrap().handle, name("alice"));
        assert!(roster.resolve("bob").is_none());
        assert_eq!(
            roster.by_account("chat", "u_1").unwrap().handle,
            name("alice")
        );
        assert!(roster.by_account("other", "u_1").is_none());
    }

    fn addr(s: &str) -> Address {
        s.parse().unwrap()
    }

    #[test]
    fn whichever_agent_resolves_to_the_one_agent() {
        let roster = top();
        assert_eq!(
            roster.resolve_address(&addr("senator::alice/*")),
            Ok(addr("senator::alice/claude"))
        );
    }

    #[test]
    fn whichever_agent_of_a_person_with_none_resolves_to_the_person() {
        let roster = top();
        assert_eq!(
            roster.resolve_address(&addr("senator::carol/*")),
            Ok(addr("senator::carol"))
        );
    }

    #[test]
    fn whichever_agent_of_a_person_with_several_is_refused_with_the_candidates() {
        let roster = top();
        let err = roster
            .resolve_address(&addr("senator::wendell/*"))
            .unwrap_err();
        assert_eq!(
            err,
            ResolveError::Ambiguous {
                owner: name("wendell"),
                candidates: vec![path("wendell/claude"), path("wendell/codex")],
            }
        );
        // The refusal reads as something a sender can act on.
        assert_eq!(
            err.to_string(),
            "wendell has several agents here; name one of: senator::wendell/claude, senator::wendell/codex"
        );
        assert_eq!(
            roster.resolve_address(&addr("senator::bob/*")),
            Err(ResolveError::UnknownSenior(name("bob")))
        );
    }

    #[test]
    fn resolving_recipients_leaves_concrete_addresses_alone_and_drops_repeats() {
        let roster = top();
        let to = [
            addr("senator::alice/claude"),
            addr("senator::alice/*"),
            addr("senator::carol/*"),
            addr("speaker"),
            addr("guest::chat/u_1"),
            Address::Other("role:reviewer".into()),
            // Not seated here, and not this function's business.
            addr("senator::wendell/gemini"),
        ];
        let resolved = roster.resolve_recipients(&to).unwrap();
        assert_eq!(
            resolved,
            vec![
                addr("senator::alice/claude"),
                addr("senator::carol"),
                addr("speaker"),
                addr("guest::chat/u_1"),
                Address::Other("role:reviewer".into()),
                addr("senator::wendell/gemini"),
            ]
        );
        assert!(resolved.iter().all(|a| !a.is_wildcard()));
        assert!(matches!(
            roster.resolve_recipients(&[addr("speaker"), addr("senator::wendell/*")]),
            Err(ResolveError::Ambiguous { .. })
        ));
    }

    #[test]
    fn in_a_nested_senate_whichever_agent_means_the_agents_seated_there() {
        let roster = Roster::under(
            path("wendell/claude"),
            seniors(&["wendell", "alice"]),
            vec![path("wendell/claude/reviewer")],
        )
        .unwrap();
        assert_eq!(
            roster.resolve_address(&addr("senator::wendell/*")),
            Ok(addr("senator::wendell/claude/reviewer"))
        );
        assert_eq!(
            roster.resolve_address(&addr("senator::alice/*")),
            Ok(addr("senator::alice"))
        );
    }

    #[test]
    fn seating_holds_an_agent_to_the_roster_rules() {
        let mut roster = Roster::top(seniors(&["wendell"]), vec![]).unwrap();
        assert_eq!(roster.seat(path("wendell/claude")), Ok(true));
        // First contact seats; every later contact finds it seated.
        assert_eq!(roster.seat(path("wendell/claude")), Ok(false));
        assert_eq!(roster.seat(path("wendell/codex")), Ok(true));
        assert_eq!(
            roster.agents(),
            [path("wendell/claude"), path("wendell/codex")]
        );

        assert_eq!(
            roster.seat(path("wendell")),
            Err(RosterError::NotAnAgent(path("wendell")))
        );
        assert!(matches!(
            roster.seat(path("alice/claude")),
            Err(RosterError::NoSenior { .. })
        ));
        assert!(matches!(
            roster.seat(path("wendell/claude/reviewer")),
            Err(RosterError::WrongLevel { .. })
        ));
        assert_eq!(roster.agents().len(), 2);

        let mut nested =
            Roster::under(path("wendell/claude"), seniors(&["wendell"]), vec![]).unwrap();
        assert_eq!(nested.seat(path("wendell/claude/reviewer")), Ok(true));
        assert!(matches!(
            nested.seat(path("wendell/codex")),
            Err(RosterError::WrongLevel { .. })
        ));
    }

    #[test]
    fn unseating_removes_the_agent_and_keeps_the_person() {
        let mut roster = top();
        assert!(roster.unseat(&path("wendell/codex")));
        assert!(!roster.unseat(&path("wendell/codex")));
        assert!(!roster.knows(&path("wendell/codex")));
        assert!(roster.knows(&path("wendell")));
        // With one agent left, "whichever" is that one again.
        assert_eq!(
            roster.agent_of(&name("wendell")),
            Ok(AgentOf::One(path("wendell/claude")))
        );
        assert!(!roster.unseat(&path("wendell")));
        assert!(roster.knows(&path("wendell")));
    }

    #[test]
    fn a_person_acts_as_themselves_and_as_agents_under_their_own_name() {
        let roster = top();
        let wendell = name("wendell");
        assert_eq!(roster.may_act_as(&wendell, &path("wendell")), Ok(()));
        assert_eq!(roster.may_act_as(&wendell, &path("wendell/claude")), Ok(()));
        // Not seated yet: allowed, and seated on first contact.
        assert!(!roster.knows(&path("wendell/gemini")));
        assert_eq!(roster.may_act_as(&wendell, &path("wendell/gemini")), Ok(()));

        assert_eq!(
            roster.may_act_as(&wendell, &path("alice/claude")),
            Err(RosterError::NotYours {
                person: wendell.clone(),
                senator: path("alice/claude"),
            })
        );
        assert!(matches!(
            roster.may_act_as(&wendell, &path("alice")),
            Err(RosterError::NotYours { .. })
        ));
        // The top Senate seats level 1 only.
        assert!(matches!(
            roster.may_act_as(&wendell, &path("wendell/claude/reviewer")),
            Err(RosterError::WrongLevel { .. })
        ));
        assert_eq!(
            roster.may_act_as(&name("bob"), &path("bob/claude")),
            Err(RosterError::UnknownSenior(name("bob")))
        );
    }

    #[test]
    fn in_a_nested_senate_a_person_acts_as_agents_under_its_presider() {
        let roster = Roster::under(
            path("wendell/claude"),
            seniors(&["wendell", "alice"]),
            vec![],
        )
        .unwrap();
        let wendell = name("wendell");
        assert_eq!(
            roster.may_act_as(&wendell, &path("wendell/claude/reviewer")),
            Ok(())
        );
        assert_eq!(roster.may_act_as(&wendell, &path("wendell")), Ok(()));
        assert!(matches!(
            roster.may_act_as(&wendell, &path("wendell/codex")),
            Err(RosterError::WrongLevel { .. })
        ));
        // Alice is known here, and has no agents of her own in wendell's Senate.
        assert_eq!(roster.may_act_as(&name("alice"), &path("alice")), Ok(()));
        assert!(matches!(
            roster.may_act_as(&name("alice"), &path("alice/claude")),
            Err(RosterError::WrongLevel { .. })
        ));
        assert!(matches!(
            roster.may_act_as(&name("alice"), &path("wendell/claude/reviewer")),
            Err(RosterError::NotYours { .. })
        ));
    }

    #[test]
    fn a_roster_is_configured_and_serialized_in_one_checked_form() {
        let json = serde_json::json!({
            "seniors": [
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
            "agents": ["wendell/claude"]
        });
        let roster: Roster = serde_json::from_value(json).unwrap();
        assert_eq!(roster.presider(), None);
        assert_eq!(roster.agents(), [path("wendell/claude")]);
        let wendell = roster.senior(&name("wendell")).unwrap();
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
            serde_json::from_value(serde_json::json!({ "seniors": [{ "handle": "alice" }] }))
                .unwrap();
        assert!(people.agents().is_empty());

        let nested: Roster = serde_json::from_value(serde_json::json!({
            "presider": "wendell/claude",
            "seniors": [{ "handle": "wendell" }],
            "agents": ["wendell/claude/reviewer"]
        }))
        .unwrap();
        assert_eq!(nested.presider(), Some(&path("wendell/claude")));
        assert_eq!(
            serde_json::to_value(&nested).unwrap()["presider"],
            "wendell/claude"
        );
    }

    #[test]
    fn a_configured_roster_that_breaks_a_rule_does_not_decode() {
        for bad in [
            serde_json::json!({ "seniors": [{ "handle": "alice" }], "agents": ["wendell/claude"] }),
            serde_json::json!({ "seniors": [{ "handle": "alice" }], "agents": ["alice"] }),
            serde_json::json!({ "seniors": [{ "handle": "alice" }], "agents": ["alice/a/b"] }),
            serde_json::json!({ "seniors": [{ "handle": "alice" }, { "handle": "alice" }] }),
            serde_json::json!({ "presider": "alice", "seniors": [{ "handle": "alice" }] }),
            serde_json::json!({
                "seniors": [{ "handle": "alice" }],
                "agents": ["alice/claude", "alice/claude"]
            }),
        ] {
            assert!(
                serde_json::from_value::<Roster>(bad.clone()).is_err(),
                "{bad}"
            );
        }
    }
}
