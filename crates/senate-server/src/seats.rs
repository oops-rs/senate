//! The roster as a running server keeps it.
//!
//! [`Seats`] holds the one [`Roster`] of the Senate this server serves, behind
//! a lock. People come from the roster the server was built with (by default
//! the configuration's); agents are seated into it as they connect
//! ([`Seats::act_as`]). Every request that acts as a senator goes through
//! [`Seats::act_as`], which is [`Roster::may_act_as`] followed by
//! [`Roster::seat`], so the check and the seating happen together.
//!
//! The daemon's built-in session is a senator of the roster too, but nobody
//! acts as it over HTTP: it writes in process.

use std::sync::{PoisonError, RwLock};

use senate_core::{Name, Principal, Roster, RosterConfig, RosterError, SenatorName};

/// What [`Seats::act_as`] changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Seated {
    /// The agent was seated by this call.
    pub new: bool,
    /// The agent's principal was recorded by this call (a new seat, or an
    /// agent seated again from the log that nobody had acted as yet).
    pub owner_set: bool,
}

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

    /// Change the roster under its lock.
    pub(crate) fn update<R>(&self, change: impl FnOnce(&mut Roster) -> R) -> R {
        let mut roster = self.roster.write().unwrap_or_else(PoisonError::into_inner);
        change(&mut roster)
    }

    /// Whether `person`, authenticated, may act as `senator`; if it is an
    /// agent, seat it under them. Nobody acts as the built-in session.
    pub(crate) fn act_as(
        &self,
        person: &Name,
        senator: &SenatorName,
    ) -> Result<Seated, RosterError> {
        let mut roster = self.roster.write().unwrap_or_else(PoisonError::into_inner);
        if roster.built_in() == Some(senator) {
            return Err(RosterError::NotYours {
                person: person.clone(),
                senator: senator.clone(),
                reason: "it is the built-in session, which writes in process",
            });
        }
        roster.may_act_as(person, senator)?;
        if senator == person {
            return Ok(Seated::default());
        }
        let had_owner = roster.owner_of(senator).is_some();
        let new = roster.seat(senator.clone(), person)?;
        Ok(Seated {
            new,
            owner_set: !had_owner,
        })
    }

    /// Seat an agent the log has already seen, with no person authenticating:
    /// a restarted server seats again the agents it finds in the log (their
    /// principals come from the store's registry, seated before this).
    pub(crate) fn seat_again(&self, agent: SenatorName) -> Result<bool, RosterError> {
        self.roster
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .seat_again(agent)
    }

    /// Replace the people, keeping the Speaker, the built-in session, the
    /// secretaries and the seated agents whose person is still listed (or
    /// who have none yet). Refused (and nothing changes) when the new
    /// people break the roster's rules.
    pub(crate) fn replace_principals(&self, principals: Vec<Principal>) -> Result<(), RosterError> {
        let mut roster = self.roster.write().unwrap_or_else(PoisonError::into_inner);
        let mut config: RosterConfig = roster.to_config();
        config.agents.retain(|a| match &a.owner {
            Some(owner) => principals.iter().any(|s| &s.handle == owner),
            None => true,
        });
        config.principals = principals;
        *roster = rebuild(config, roster.built_in().cloned())?;
        Ok(())
    }
}

/// A roster from its configured form plus the built-in session, in the
/// order its rules need: a Speaker that is a principal is named before the
/// built-in session is seated (which it then runs); one that is the
/// built-in session is named after.
pub(crate) fn rebuild(
    mut config: RosterConfig,
    built_in: Option<SenatorName>,
) -> Result<Roster, RosterError> {
    let speaker = config.speaker.take();
    let names_built_in = speaker
        .as_ref()
        .is_some_and(|s| s.id.senate().is_none() && Some(s.id.name()) == built_in.as_ref());
    let mut roster = Roster::try_from(config)?;
    if !names_built_in {
        roster.name_speaker(speaker.clone())?;
    }
    if let Some(built_in) = built_in {
        roster = roster.with_built_in(built_in)?;
    }
    if names_built_in {
        roster.name_speaker(speaker)?;
    }
    Ok(roster)
}
