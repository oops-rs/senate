//! The roster as a running server keeps it.
//!
//! [`Seats`] holds the one [`Roster`] of the Senate this server serves, behind
//! a lock. People come from the roster the server was built with (by default
//! the configuration's); agents are seated into it as they connect
//! ([`Seats::act_as`]). Every request that acts as a senator goes through
//! [`Seats::act_as`], which is [`Roster::may_act_as`] followed by
//! [`Roster::seat`], so the check and the seating happen together.

use std::sync::{PoisonError, RwLock};

use senate_core::{Name, Principal, Roster, RosterConfig, RosterError, SenatorName};

pub(crate) struct Seats {
    roster: RwLock<Roster>,
}

impl Seats {
    pub(crate) fn new(roster: Roster) -> Self {
        Self {
            roster: RwLock::new(roster),
        }
    }

    /// The roster as it is now.
    pub(crate) fn snapshot(&self) -> Roster {
        self.roster
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether `person`, authenticated, may act as `senator`; if it is an
    /// agent, seat it under them. `Ok(true)` when it was seated by this
    /// call.
    pub(crate) fn act_as(&self, person: &Name, senator: &SenatorName) -> Result<bool, RosterError> {
        let mut roster = self.roster.write().unwrap_or_else(PoisonError::into_inner);
        roster.may_act_as(person, senator)?;
        if senator == person {
            return Ok(false);
        }
        roster.seat(senator.clone(), person)
    }

    /// Seat an agent the log has already seen, with no person authenticating:
    /// seats are not stored, so a restarted server seats again the agents it
    /// finds in the log.
    pub(crate) fn seat_again(&self, agent: SenatorName) -> Result<bool, RosterError> {
        self.roster
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .seat_again(agent)
    }

    /// Replace the people, keeping the named Speaker and the seated agents
    /// whose person is still listed (or who have none yet). Refused (and
    /// nothing changes) when the new people break the roster's rules.
    pub(crate) fn replace_principals(&self, principals: Vec<Principal>) -> Result<(), RosterError> {
        let mut roster = self.roster.write().unwrap_or_else(PoisonError::into_inner);
        let mut config: RosterConfig = roster.to_config();
        config.agents.retain(|a| match &a.owner {
            Some(owner) => principals.iter().any(|s| &s.handle == owner),
            None => true,
        });
        config.principals = principals;
        *roster = Roster::try_from(config)?;
        Ok(())
    }
}
