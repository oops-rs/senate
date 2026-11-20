//! Senator aliases, each kept on the committee it was given in.
//!
//! An alias is a short name for an agent's primary senator ID (`main` for
//! `claude-6d8b…`). It belongs to one committee, so the same alias can stand
//! for different agents on different committees (`nous::main`,
//! `nous-ios::main`). A person's handle is Senate-wide and is never an alias.
//!
//! [`CommitteeAliases`] is the one table, and [`CommitteeAliases::resolve`]
//! the one resolution, the server and the bridge both use:
//!
//! - `committee::alias` looks on that committee only.
//! - A bare alias is matched on every committee. One match (or several
//!   standing for the same agent) resolves. With several agents the result
//!   is [`AliasMatch::Ambiguous`]: the qualified candidates, and the match
//!   on the writer's own committee if there is one. Each caller applies its
//!   rule: a non-interactive one (the server, MCP tools, `ask`/`tell`)
//!   takes the writer's match and otherwise refuses, listing the candidates
//!   ([`AliasMatch::non_interactive`]); an interactive CLI command lets the
//!   person pick among the candidates, even when one is on their own
//!   committee.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::address::SEPARATOR;
use crate::ids::CommitteeId;
use crate::senator::{SenatorName, SenatorNameError};

/// Every committee's aliases: committee → alias → primary senator ID.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommitteeAliases(BTreeMap<CommitteeId, BTreeMap<SenatorName, SenatorName>>);

/// One alias with the committee it is on and the agent it stands for.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct QualifiedAlias {
    pub committee: CommitteeId,
    pub alias: SenatorName,
    pub primary: SenatorName,
}

impl fmt::Display for QualifiedAlias {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{SEPARATOR}{}", self.committee, self.alias)
    }
}

/// What a name resolves to in [`CommitteeAliases`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasMatch {
    /// No alias by this name: the name stands for itself.
    NotAlias,
    /// The agent this alias stands for.
    One(SenatorName),
    /// Several agents have this alias: the qualified candidates, in
    /// committee order, and the agent it stands for on the writer's own
    /// committee, if it is one there.
    Ambiguous {
        candidates: Vec<QualifiedAlias>,
        own: Option<SenatorName>,
    },
}

impl AliasMatch {
    /// The rule for a caller that cannot ask a person: the resolved
    /// primary, the writer's own committee's match when several agents
    /// have the alias, or the name itself when it is no alias. Otherwise
    /// an error listing the candidates.
    pub fn non_interactive(self, name: &SenatorName) -> Result<SenatorName, AmbiguousAlias> {
        match self {
            Self::NotAlias => Ok(name.clone()),
            Self::One(primary)
            | Self::Ambiguous {
                own: Some(primary), ..
            } => Ok(primary),
            Self::Ambiguous {
                candidates,
                own: None,
            } => Err(AmbiguousAlias {
                alias: name.clone(),
                candidates,
            }),
        }
    }
}

/// A bare alias several agents have, and none on the writer's own committee.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{alias} is an alias on several committees; name one: {}", list(.candidates))]
pub struct AmbiguousAlias {
    pub alias: SenatorName,
    pub candidates: Vec<QualifiedAlias>,
}

fn list(candidates: &[QualifiedAlias]) -> String {
    candidates
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Why an alias cannot be set.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AliasError {
    #[error("{alias} is already the alias of another senator on {committee}")]
    Taken {
        committee: CommitteeId,
        alias: SenatorName,
    },
}

impl CommitteeAliases {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.0.values().all(BTreeMap::is_empty)
    }

    /// Every alias, by committee then alias.
    pub fn iter(&self) -> impl Iterator<Item = QualifiedAlias> + '_ {
        self.0.iter().flat_map(|(committee, aliases)| {
            aliases.iter().map(|(alias, primary)| QualifiedAlias {
                committee: committee.clone(),
                alias: alias.clone(),
                primary: primary.clone(),
            })
        })
    }

    /// The agent `alias` stands for on `committee`.
    pub fn get(&self, committee: &CommitteeId, alias: &SenatorName) -> Option<&SenatorName> {
        self.0.get(committee)?.get(alias)
    }

    /// The alias `primary` has on `committee`.
    pub fn alias_of(&self, committee: &CommitteeId, primary: &SenatorName) -> Option<&SenatorName> {
        self.0
            .get(committee)?
            .iter()
            .find_map(|(alias, target)| (target == primary).then_some(alias))
    }

    /// Whether `name` is an alias on any committee.
    pub fn is_alias(&self, name: &SenatorName) -> bool {
        self.0.values().any(|aliases| aliases.contains_key(name))
    }

    /// Every committee's alias by this name.
    pub fn matches(&self, alias: &SenatorName) -> Vec<QualifiedAlias> {
        self.iter().filter(|q| &q.alias == alias).collect()
    }

    /// Resolve `name`, written from `writer` (the writer's own committee,
    /// when there is one).
    pub fn resolve(&self, name: &AliasName, writer: Option<&CommitteeId>) -> AliasMatch {
        if let Some(committee) = &name.committee {
            return self
                .get(committee, &name.alias)
                .map_or(AliasMatch::NotAlias, |p| AliasMatch::One(p.clone()));
        }
        let matches = self.matches(&name.alias);
        let Some(first) = matches.first() else {
            return AliasMatch::NotAlias;
        };
        if matches.iter().all(|q| q.primary == first.primary) {
            return AliasMatch::One(first.primary.clone());
        }
        AliasMatch::Ambiguous {
            own: writer.and_then(|c| self.get(c, &name.alias)).cloned(),
            candidates: matches,
        }
    }

    /// Resolve a bare `name` written on `writer`'s committee.
    pub fn resolve_bare(&self, name: &SenatorName, writer: Option<&CommitteeId>) -> AliasMatch {
        self.resolve(&AliasName::bare(name.clone()), writer)
    }

    /// The Senate-wide map older clients read (`GET /v1/aliases`): every
    /// alias that stands for one agent, wherever it is. An alias several
    /// agents have is left out, since no flat entry can say which.
    pub fn flat(&self) -> BTreeMap<SenatorName, SenatorName> {
        let mut flat = BTreeMap::new();
        let mut shared = std::collections::BTreeSet::new();
        for q in self.iter() {
            match flat.get(&q.alias) {
                Some(primary) if *primary != q.primary => {
                    shared.insert(q.alias);
                }
                _ => {
                    flat.insert(q.alias, q.primary);
                }
            }
        }
        flat.retain(|alias, _| !shared.contains(alias));
        flat
    }

    /// Give `primary` the alias `alias` on `committee`, replacing the alias
    /// it had there. The caller checks the name is not a person's handle,
    /// a primary ID or session-shaped.
    pub fn set(
        &mut self,
        committee: &CommitteeId,
        alias: SenatorName,
        primary: SenatorName,
    ) -> Result<(), AliasError> {
        let aliases = self.0.entry(committee.clone()).or_default();
        if aliases.get(&alias).is_some_and(|target| *target != primary) {
            return Err(AliasError::Taken {
                committee: committee.clone(),
                alias,
            });
        }
        aliases.retain(|_, target| *target != primary);
        aliases.insert(alias, primary);
        Ok(())
    }

    /// Remove `alias` from `committee`: the agent it stood for.
    pub fn remove(&mut self, committee: &CommitteeId, alias: &SenatorName) -> Option<SenatorName> {
        let aliases = self.0.get_mut(committee)?;
        let primary = aliases.remove(alias);
        if aliases.is_empty() {
            self.0.remove(committee);
        }
        primary
    }

    /// Keep only the aliases whose agent passes `keep`.
    pub fn retain(&mut self, mut keep: impl FnMut(&SenatorName) -> bool) {
        for aliases in self.0.values_mut() {
            aliases.retain(|_, primary| keep(primary));
        }
        self.0.retain(|_, aliases| !aliases.is_empty());
    }

    /// The table from the Senate-wide aliases kept before aliases were per
    /// committee: each alias goes to every committee its agent sits in
    /// (`sits`). The ones whose agent sits nowhere are returned apart, not
    /// carried over.
    pub fn from_global(
        global: &BTreeMap<SenatorName, SenatorName>,
        mut sits: impl FnMut(&SenatorName) -> Vec<CommitteeId>,
    ) -> (Self, Vec<(SenatorName, SenatorName)>) {
        let mut table = Self::new();
        let mut dropped = Vec::new();
        for (alias, primary) in global {
            let committees = sits(primary);
            if committees.is_empty() {
                dropped.push((alias.clone(), primary.clone()));
            }
            for committee in committees {
                // A global map held one alias per agent and one agent per
                // alias, so nothing here can be taken.
                let _ = table.set(&committee, alias.clone(), primary.clone());
            }
        }
        (table, dropped)
    }
}

/// The aliases a bridge reads from one Senate's server: the per-committee
/// table, or, from a server older than per-committee aliases, its
/// Senate-wide map (`GET /v1/aliases`), resolved as it was then: an alias
/// stands for one agent everywhere, and a committee written with it is not
/// checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnownAliases {
    Scoped(CommitteeAliases),
    Unscoped(BTreeMap<SenatorName, SenatorName>),
}

impl KnownAliases {
    /// Whether these came from an older server, which keeps them Senate-wide.
    pub fn is_unscoped(&self) -> bool {
        matches!(self, Self::Unscoped(_))
    }

    /// See [`CommitteeAliases::resolve`].
    pub fn resolve(&self, name: &AliasName, writer: Option<&CommitteeId>) -> AliasMatch {
        match self {
            Self::Scoped(table) => table.resolve(name, writer),
            Self::Unscoped(map) => map
                .get(&name.alias)
                .map_or(AliasMatch::NotAlias, |p| AliasMatch::One(p.clone())),
        }
    }

    /// The alias `primary` goes by on `committee` (on an older server, its
    /// one alias).
    pub fn alias_of(&self, committee: &CommitteeId, primary: &SenatorName) -> Option<&SenatorName> {
        match self {
            Self::Scoped(table) => table.alias_of(committee, primary),
            Self::Unscoped(map) => map
                .iter()
                .find_map(|(alias, target)| (target == primary).then_some(alias)),
        }
    }
}

/// A name as written where an alias may be: `alias` or `committee::alias`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasName {
    pub committee: Option<CommitteeId>,
    pub alias: SenatorName,
}

impl AliasName {
    pub fn bare(alias: SenatorName) -> Self {
        Self {
            committee: None,
            alias,
        }
    }
}

impl From<SenatorName> for AliasName {
    fn from(alias: SenatorName) -> Self {
        Self::bare(alias)
    }
}

impl fmt::Display for AliasName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.committee {
            Some(committee) => write!(f, "{committee}{SEPARATOR}{}", self.alias),
            None => write!(f, "{}", self.alias),
        }
    }
}

impl Serialize for AliasName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// A bare alias reads as a plain senator name does, so a field that held a
/// [`SenatorName`] can hold an [`AliasName`] with no change on the wire.
impl<'de> Deserialize<'de> for AliasName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AliasNameError {
    #[error("{0:?} is not a committee id")]
    Committee(String),
    #[error(transparent)]
    Name(#[from] SenatorNameError),
}

impl FromStr for AliasName {
    type Err = AliasNameError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.split_once(SEPARATOR) {
            Some((committee, alias)) => Ok(Self {
                committee: Some(
                    CommitteeId::new(committee)
                        .map_err(|_| AliasNameError::Committee(committee.to_string()))?,
                ),
                alias: alias.parse()?,
            }),
            None => Ok(Self::bare(s.parse()?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> CommitteeId {
        CommitteeId::new(s).unwrap()
    }
    fn n(s: &str) -> SenatorName {
        s.parse().unwrap()
    }
    fn table(rows: &[(&str, &str, &str)]) -> CommitteeAliases {
        let mut t = CommitteeAliases::new();
        for (committee, alias, primary) in rows {
            t.set(&c(committee), n(alias), n(primary)).unwrap();
        }
        t
    }

    #[test]
    fn the_same_alias_lives_on_several_committees() {
        let t = table(&[
            ("nous", "main", "claude-a"),
            ("nous-ios", "main", "claude-b"),
        ]);
        assert_eq!(t.get(&c("nous"), &n("main")), Some(&n("claude-a")));
        assert_eq!(t.get(&c("nous-ios"), &n("main")), Some(&n("claude-b")));
        assert!(t.is_alias(&n("main")));
    }

    #[test]
    fn an_alias_is_unique_on_its_committee_and_replaces_the_agents_old_one() {
        let mut t = table(&[("nous", "main", "claude-a")]);
        assert_eq!(
            t.set(&c("nous"), n("main"), n("claude-b")),
            Err(AliasError::Taken {
                committee: c("nous"),
                alias: n("main")
            })
        );
        t.set(&c("nous"), n("lead"), n("claude-a")).unwrap();
        assert_eq!(t.get(&c("nous"), &n("main")), None);
        assert_eq!(t.alias_of(&c("nous"), &n("claude-a")), Some(&n("lead")));
    }

    #[test]
    fn a_single_match_resolves_from_any_committee() {
        let t = table(&[("nous", "main", "claude-a")]);
        let main = AliasName::bare(n("main"));
        assert_eq!(t.resolve(&main, None), AliasMatch::One(n("claude-a")));
        assert_eq!(
            t.resolve(&main, Some(&c("elsewhere"))),
            AliasMatch::One(n("claude-a"))
        );
        assert_eq!(t.resolve_bare(&n("other"), None), AliasMatch::NotAlias);
    }

    #[test]
    fn matches_of_one_agent_count_as_one() {
        let t = table(&[
            ("nous", "main", "claude-a"),
            ("nous-ios", "main", "claude-a"),
        ]);
        assert_eq!(
            t.resolve_bare(&n("main"), None),
            AliasMatch::One(n("claude-a"))
        );
    }

    #[test]
    fn several_agents_give_candidates_and_the_writers_own_match() {
        let t = table(&[
            ("nous", "main", "claude-a"),
            ("nous-ios", "main", "claude-b"),
        ]);
        let from_ios = t.resolve_bare(&n("main"), Some(&c("nous-ios")));
        let AliasMatch::Ambiguous { candidates, own } = from_ios.clone() else {
            panic!("expected candidates");
        };
        let shown: Vec<String> = candidates.iter().map(ToString::to_string).collect();
        assert_eq!(shown, ["nous::main", "nous-ios::main"]);
        assert_eq!(own, Some(n("claude-b")));
        // A caller that cannot ask takes the writer's committee's match.
        assert_eq!(from_ios.non_interactive(&n("main")), Ok(n("claude-b")));
        // From a committee with no match, it is refused with the candidates.
        let err = t
            .resolve_bare(&n("main"), Some(&c("senate")))
            .non_interactive(&n("main"))
            .unwrap_err();
        assert!(
            err.to_string().contains("nous::main, nous-ios::main"),
            "{err}"
        );
        assert_eq!(
            t.resolve_bare(&n("other"), None)
                .non_interactive(&n("other")),
            Ok(n("other"))
        );
    }

    #[test]
    fn a_qualified_name_looks_on_its_committee_only() {
        let t = table(&[
            ("nous", "main", "claude-a"),
            ("nous-ios", "main", "claude-b"),
        ]);
        let q: AliasName = "nous-ios::main".parse().unwrap();
        assert_eq!(
            t.resolve(&q, Some(&c("nous"))),
            AliasMatch::One(n("claude-b"))
        );
        let missing: AliasName = "senate::main".parse().unwrap();
        assert_eq!(t.resolve(&missing, None), AliasMatch::NotAlias);
        assert!("Bad::main".parse::<AliasName>().is_err());
        assert_eq!(q.to_string(), "nous-ios::main");
        assert_eq!(serde_json::to_value(&q).unwrap(), "nous-ios::main");
        let bare: AliasName = serde_json::from_value("main".into()).unwrap();
        assert_eq!(bare, AliasName::bare(n("main")));
    }

    #[test]
    fn the_flat_view_leaves_out_aliases_of_several_agents() {
        let t = table(&[
            ("nous", "main", "claude-a"),
            ("nous-ios", "main", "claude-b"),
            ("nous", "lead", "claude-c"),
            ("senate", "lead", "claude-c"),
        ]);
        assert_eq!(t.flat(), BTreeMap::from([(n("lead"), n("claude-c"))]));
    }

    #[test]
    fn an_older_servers_map_resolves_senate_wide() {
        let known = KnownAliases::Unscoped(BTreeMap::from([(n("main"), n("claude-a"))]));
        let qualified: AliasName = "nous::main".parse().unwrap();
        assert_eq!(
            known.resolve(&qualified, None),
            AliasMatch::One(n("claude-a"))
        );
        assert_eq!(known.resolve_bare_for_test("other"), AliasMatch::NotAlias);
        assert_eq!(known.alias_of(&c("x"), &n("claude-a")), Some(&n("main")));
        assert!(known.is_unscoped());
    }

    impl KnownAliases {
        fn resolve_bare_for_test(&self, name: &str) -> AliasMatch {
            self.resolve(&AliasName::bare(n(name)), None)
        }
    }

    #[test]
    fn removing_the_last_alias_drops_the_committee() {
        let mut t = table(&[("nous", "main", "claude-a")]);
        assert_eq!(t.remove(&c("nous"), &n("main")), Some(n("claude-a")));
        assert!(t.is_empty());
        assert_eq!(t, CommitteeAliases::new());
    }

    #[test]
    fn global_aliases_go_to_every_committee_their_agent_sits_in() {
        let global = BTreeMap::from([
            (n("main"), n("claude-a")),
            (n("review"), n("codex-b")),
            (n("smoke"), n("claude-gone")),
        ]);
        let (t, dropped) =
            CommitteeAliases::from_global(&global, |primary| match primary.as_str() {
                "claude-a" => vec![c("nous"), c("nous-ios")],
                "codex-b" => vec![c("senate")],
                _ => vec![],
            });
        assert_eq!(
            t,
            table(&[
                ("nous", "main", "claude-a"),
                ("nous-ios", "main", "claude-a"),
                ("senate", "review", "codex-b"),
            ])
        );
        assert_eq!(dropped, [(n("smoke"), n("claude-gone"))]);
    }

    #[test]
    fn the_table_round_trips_as_nested_maps() {
        let t = table(&[("nous", "main", "claude-a")]);
        let json = serde_json::to_value(&t).unwrap();
        assert_eq!(json, serde_json::json!({"nous": {"main": "claude-a"}}));
        assert_eq!(serde_json::from_value::<CommitteeAliases>(json).unwrap(), t);
    }
}
