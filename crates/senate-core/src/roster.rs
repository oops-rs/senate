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

use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
        for (i, agent) in agents.iter().enumerate() {
            if agent.is_senior() {
                return Err(RosterError::NotAnAgent(agent.clone()));
            }
            if !seniors.iter().any(|s| &s.handle == agent.owner()) {
                return Err(RosterError::NoSenior {
                    agent: agent.clone(),
                    owner: agent.owner().clone(),
                });
            }
            match &presider {
                None if agent.level() != 1 => {
                    return Err(RosterError::WrongLevel {
                        agent: agent.clone(),
                        reason: "the top Senate seats agents directly under their person",
                    });
                }
                Some(presider) if agent.presider().as_ref() != Some(presider) => {
                    return Err(RosterError::WrongLevel {
                        agent: agent.clone(),
                        reason: "a nested Senate seats agents directly under its presider",
                    });
                }
                _ => {}
            }
            if agents[..i].contains(agent) {
                return Err(RosterError::Duplicate(agent.clone()));
            }
        }
        Ok(Self {
            presider,
            seniors,
            agents,
        })
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
}
