//! How people are shown a mention: a name, not an id. `[senator::claude-…]`
//! with the alias `senate-chair` on its committee is `senate-chair`,
//! `[senator::alice]` is `alice`, `[speaker::copilot]` is `copilot
//! (Speaker)`, a seat in a child Senate is `refund-dev（carol-desk）`.
//! The same rules as the Speaker's own rendering (`senate-speaker`'s
//! `SeatNames`): an alias on the committee where it is shown, the alias a
//! child declares for its seat, else the id's name.
//!
//! What it knows comes from the daemon: the people and child Senates with
//! each `status`, the aliases from `aliases`. It is kept here, process-wide,
//! for [`crate::ffi::split_mentions`], which the app calls for every text it
//! draws; [`version`] changes whenever any of it does, so the app can keep
//! its split texts until then.

use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use senate_core::alias::KnownAliases;
use senate_core::{ChildSenate, CommitteeId, Locale, Locator, Mention, People, SenatorName};

/// What the daemon last said about names.
#[derive(Debug, Clone, Default)]
pub struct Names {
    pub people: People,
    pub aliases: Option<KnownAliases>,
    pub children: Vec<ChildSenate>,
    pub locale: Locale,
}

static NAMES: RwLock<Option<Names>> = RwLock::new(None);
static VERSION: AtomicU64 = AtomicU64::new(0);

/// Change what is known; bumps [`version`] only when something changed.
pub(crate) fn update(edit: impl FnOnce(&mut Names)) {
    let Ok(mut guard) = NAMES.write() else { return };
    let names = guard.get_or_insert_with(Names::default);
    let before = format!("{names:?}");
    edit(names);
    if format!("{names:?}") != before {
        VERSION.fetch_add(1, Ordering::Relaxed);
    }
}

/// A snapshot of what is known.
pub(crate) fn current() -> Names {
    NAMES
        .read()
        .ok()
        .and_then(|n| n.clone())
        .unwrap_or_default()
}

/// Changes whenever the names do.
pub fn version() -> u64 {
    VERSION.load(Ordering::Relaxed)
}

impl Names {
    /// How `mention` is shown on `committee` (`None`: no committee in
    /// view, so an alias counts when the agent has only one).
    pub fn display(&self, mention: &Mention, committee: Option<&CommitteeId>) -> String {
        let l = self.locale;
        match mention {
            Mention::Senator(p) => self.senator(p, committee),
            Mention::AnyAgent => l.pick("任一代理", "any agent").to_string(),
            Mention::Speaker => "Speaker".to_string(),
            Mention::SpeakerFor(p) => role(l, &self.senator(p, committee), "Speaker"),
            Mention::Chair => l.pick("主席", "chair").to_string(),
            Mention::Committee => l.pick("委员会", "committee").to_string(),
            Mention::Guest(g) => g.to_string(),
            Mention::Locator(loc) => self.locator(loc),
        }
    }

    fn senator(&self, p: &SenatorName, committee: Option<&CommitteeId>) -> String {
        senate_core::alias::display_name(self.aliases.as_ref(), committee, p)
    }

    fn locator(&self, loc: &Locator) -> String {
        let l = self.locale;
        let committee = loc.committee();
        let inner = match Mention::of(loc.address()) {
            Some(Mention::Senator(p)) => p,
            Some(other) => {
                let shown = self.display(&other, committee);
                return match committee {
                    Some(c) => role(l, &shown, c.as_str()),
                    None => shown,
                };
            }
            None => return loc.to_string(),
        };
        match loc.senate() {
            // A seat in a child Senate: the alias it declares, with the
            // child's name.
            Some(senate) => {
                let child = senate.name();
                let declared = self
                    .children
                    .iter()
                    .find(|c| c.name == *child)
                    .and_then(|c| {
                        c.seats
                            .iter()
                            .find(|s| Some(&s.committee) == committee && s.senator == inner)
                    })
                    .and_then(|s| s.alias.clone());
                role(
                    l,
                    &declared.as_ref().map_or_else(
                        || senate_core::alias::short_name(&inner),
                        ToString::to_string,
                    ),
                    child.as_str(),
                )
            }
            // A seat here, named with its committee.
            None => {
                let name = self.senator(&inner, committee);
                match committee {
                    Some(c) => role(l, &name, c.as_str()),
                    None => name,
                }
            }
        }
    }
}

/// A seat in a child Senate as people are shown it: `refund-dev
/// (carol-desk)` / `refund-dev（carol-desk）`.
pub fn seat_name(l: Locale, name: &str, child: &str) -> String {
    role(l, name, child)
}

/// `name (what)` / `name（what）`.
fn role(l: Locale, name: &str, what: &str) -> String {
    match l {
        Locale::Zh => format!("{name}（{what}）"),
        Locale::En => format!("{name} ({what})"),
    }
}

/// The id a mention stands for, as written without brackets.
pub fn id_of(mention_text: &str) -> String {
    mention_text
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use senate_core::alias::CommitteeAliases;
    use senate_core::{ChildSeat, Name, PresenceState};

    fn committee(c: &str) -> CommitteeId {
        CommitteeId::new(c).unwrap()
    }

    fn names(locale: Locale) -> Names {
        let mut table = CommitteeAliases::new();
        table
            .set(
                &committee("senate"),
                "senate-chair".parse().unwrap(),
                "claude-a956".parse().unwrap(),
            )
            .unwrap();
        table
            .set(
                &committee("other"),
                "chair2".parse().unwrap(),
                "claude-a956".parse().unwrap(),
            )
            .unwrap();
        table
            .set(
                &committee("senate"),
                "windows".parse().unwrap(),
                "claude-b111".parse().unwrap(),
            )
            .unwrap();
        Names {
            people: People::new([Name::new("alice").unwrap()]),
            aliases: Some(KnownAliases::Scoped(table)),
            children: vec![ChildSenate {
                seats: vec![ChildSeat {
                    committee: committee("senate"),
                    senator: "claude-c222".parse().unwrap(),
                    alias: Some("refund-dev".parse().unwrap()),
                    duty: None,
                    presence: PresenceState::Online,
                    paused: false,
                }],
                ..ChildSenate::new(
                    Name::new("carol-desk").unwrap(),
                    "claude-d333".parse().unwrap(),
                )
            }],
            locale,
        }
    }

    fn shown(n: &Names, text: &str, c: Option<&str>) -> String {
        let found = Mention::scan(text).into_iter().next().unwrap();
        n.display(&found.mention, c.map(committee).as_ref())
    }

    #[test]
    fn a_mention_is_shown_by_its_alias_on_the_committee_in_view() {
        let n = names(Locale::En);
        assert_eq!(
            shown(&n, "[senator::claude-a956]", Some("senate")),
            "senate-chair"
        );
        assert_eq!(shown(&n, "[senator::claude-a956]", Some("other")), "chair2");
        assert_eq!(
            shown(&n, "[senator::claude-a956]", Some("third")),
            "claude-a956"
        );
        // No committee in view: only an agent's one alias counts.
        assert_eq!(shown(&n, "[senator::claude-a956]", None), "claude-a956");
        assert_eq!(shown(&n, "[senator::claude-b111]", None), "windows");
        assert_eq!(shown(&n, "[senator::alice]", None), "alice");
    }

    #[test]
    fn roles_and_child_seats_read_as_names() {
        let n = names(Locale::En);
        assert_eq!(
            shown(&n, "[speaker::claude-b111]", Some("senate")),
            "windows (Speaker)"
        );
        assert_eq!(shown(&n, "[speaker]", None), "Speaker");
        assert_eq!(shown(&n, "[senator::*]", None), "any agent");
        assert_eq!(
            shown(&n, "[/carol-desk::senate::senator::claude-c222]", None),
            "refund-dev (carol-desk)"
        );
        let zh = names(Locale::Zh);
        assert_eq!(
            shown(&zh, "[/carol-desk::senate::senator::claude-c222]", None),
            "refund-dev（carol-desk）"
        );
        assert_eq!(
            shown(&zh, "[speaker::claude-b111]", Some("senate")),
            "windows（Speaker）"
        );
    }

    #[test]
    fn without_aliases_a_mention_is_its_name() {
        let n = Names::default();
        assert_eq!(
            shown(&n, "[senator::claude-a956]", Some("senate")),
            "claude-a956"
        );
        assert_eq!(id_of("[senator::claude-a956]"), "senator::claude-a956");
    }
}
