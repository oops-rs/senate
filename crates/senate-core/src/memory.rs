//! A committee's memory: what its discussion established, distilled into
//! short points, and the digests that summarise it.
//!
//! Everything here is a fold over the log. The log is the one source of
//! truth; [`MemoryFold`] (with its search index) is rebuilt from any prefix
//! of it and never stored.
//!
//! # Events
//!
//! All of them are `note`s, never pushed (`to` is empty):
//!
//! - **A digest** ([`meta::DIGEST`], a [`DigestPayload`]): the Speaker's
//!   summary of the discussion in a window of the log, at most
//!   [`MAX_DIGEST_POINTS`] one-line points, each citing the seqs it came
//!   from. The body is the points written out for people.
//! - **A memory point** ([`meta::MEMORY`], [`MemoryPayload::Point`]): one
//!   fact worth keeping, from the Speaker. Its identity is its own seq. It
//!   cites the seqs it came from (`refs`) and names the points it replaces
//!   (`supersedes`). The body is the point.
//! - **A correction** ([`MemoryPayload::Correct`]): a person's new wording of
//!   a point. It is a point itself, and it supersedes its target.
//! - **A deletion** ([`MemoryPayload::Delete`]): a person retracts points.
//!
//! Who may write which is the server's rule (the Speaker writes points; a
//! person, or the Speaker on a person's behalf, corrects and deletes); the
//! fold also ignores a point that is not from the Speaker, and a correction
//! or deletion from a guest.
//!
//! # Search
//!
//! The index is terms to points: lowercase words of letters and digits, and
//! character bigrams of CJK text, so Chinese needs no spaces. A query is
//! split the same way; a point matches by how many of the query's terms it
//! has, ties going to the newer point. Points are few (distilled), so an
//! in-memory index is the whole of it.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::address::Address;
use crate::event::{Event, Kind, meta};
use crate::ids::Seq;

/// At most this many points in a digest.
pub const MAX_DIGEST_POINTS: usize = 7;
/// Characters of one digest or memory point kept.
pub const POINT_CHARS: usize = 200;
/// Memory points in force shown in the brief ("What we know").
pub const MAX_BRIEF_POINTS: usize = 15;

/// Why a digest was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DigestTrigger {
    /// The committee's daily time came.
    Daily,
    /// A busy stretch of discussion went quiet.
    Quiet,
}

impl DigestTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Quiet => "quiet",
        }
    }
}

/// One line of a digest, with the seqs it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestPoint {
    pub text: String,
    #[serde(default)]
    pub refs: Vec<Seq>,
}

/// `meta.digest` on the Speaker's digest `note`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestPayload {
    pub trigger: DigestTrigger,
    /// The first seq of the window it covers.
    pub from: Seq,
    /// The last seq of discussion it covers. The next digest covers only
    /// what came after.
    pub through: Seq,
    pub points: Vec<DigestPoint>,
}

impl DigestPayload {
    /// The payload of a digest note: from the Speaker, carrying
    /// [`meta::DIGEST`].
    pub fn of(event: &Event) -> Option<Self> {
        if event.kind != Kind::Note || event.from != Address::Speaker {
            return None;
        }
        serde_json::from_value(event.meta.get(meta::DIGEST)?.clone()).ok()
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// A digest as the log holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Digest {
    pub seq: Seq,
    pub ts: DateTime<Utc>,
    #[serde(flatten)]
    pub payload: DigestPayload,
}

/// `meta.memory` on a memory `note`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum MemoryPayload {
    /// A point distilled by the Speaker; the body is its text.
    Point {
        #[serde(default)]
        refs: Vec<Seq>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        supersedes: Vec<Seq>,
        /// The digest it was distilled with.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        digest: Option<Seq>,
    },
    /// A person's correction of `target`; the body is the corrected text.
    Correct {
        target: Seq,
        #[serde(default)]
        refs: Vec<Seq>,
    },
    /// A person's deletion of points.
    Delete { targets: Vec<Seq> },
}

impl MemoryPayload {
    pub fn of(event: &Event) -> Option<Self> {
        if event.kind != Kind::Note {
            return None;
        }
        serde_json::from_value(event.meta.get(meta::MEMORY)?.clone()).ok()
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// Who may write this: a point is the Speaker's alone; a correction or
    /// deletion is a person's (written by them, or by the Speaker on their
    /// behalf). Whether a senator is a person is the roster's to say, so
    /// this answers for the shape only; the server checks the person.
    pub fn by_person(&self) -> bool {
        !matches!(self, Self::Point { .. })
    }
}

/// One memory point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryPoint {
    pub seq: Seq,
    pub ts: DateTime<Utc>,
    pub text: String,
    /// The seqs it came from.
    #[serde(default)]
    pub refs: Vec<Seq>,
    /// The points it replaced.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supersedes: Vec<Seq>,
    /// Who wrote it: the Speaker, or the person who corrected it.
    pub by: Address,
}

impl MemoryPoint {
    /// A person wrote this point (a correction). Only a person may replace
    /// it.
    pub fn by_person(&self) -> bool {
        self.by != Address::Speaker
    }
}

/// How a point stopped being in force.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Retired {
    /// Replaced by the point at this seq.
    Superseded(Seq),
    /// Deleted by a person, by the event at this seq.
    Deleted(Seq),
}

/// A search result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryHit {
    #[serde(flatten)]
    pub point: MemoryPoint,
    /// Set when the point is no longer in force.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired: Option<Retired>,
}

/// Folds a committee's memory events, in seq order, into its points in force,
/// their history, the latest digest, and a search index.
#[derive(Debug, Clone, Default)]
pub struct MemoryFold {
    head: Seq,
    points: BTreeMap<Seq, MemoryPoint>,
    retired: BTreeMap<Seq, Retired>,
    /// Every ref of a point a person deleted: a new point citing one of
    /// these would bring the deleted point back.
    deleted_refs: BTreeSet<Seq>,
    digest: Option<Digest>,
    /// The `through` of every digest, highest last.
    through: Seq,
    index: HashMap<String, Vec<Seq>>,
}

impl MemoryFold {
    pub fn new() -> Self {
        Self::default()
    }

    /// The last seq folded in.
    pub fn head(&self) -> Seq {
        self.head
    }

    /// Fold one event; events at or below [`Self::head`] are ignored.
    pub fn observe(&mut self, event: &Event) {
        if event.seq <= self.head {
            return;
        }
        self.head = event.seq;
        if let Some(digest) = DigestPayload::of(event) {
            self.through = self.through.max(digest.through);
            self.digest = Some(Digest {
                seq: event.seq,
                ts: event.ts,
                payload: digest,
            });
            return;
        }
        let Some(payload) = MemoryPayload::of(event) else {
            return;
        };
        if matches!(event.from, Address::Guest(_)) {
            return;
        }
        let by = event
            .on_behalf_of
            .clone()
            .unwrap_or_else(|| event.from.clone());
        match payload {
            MemoryPayload::Point {
                refs, supersedes, ..
            } => {
                if event.from != Address::Speaker || event.on_behalf_of.is_some() {
                    return;
                }
                self.add(event, refs, supersedes, by);
            }
            MemoryPayload::Correct { target, refs } => {
                if by == Address::Speaker {
                    return;
                }
                // Written without sources (a person typing it): the point it
                // corrects came from the same discussion.
                let refs = if refs.is_empty() {
                    self.points
                        .get(&target)
                        .map(|p| p.refs.clone())
                        .unwrap_or_default()
                } else {
                    refs
                };
                self.add(event, refs, vec![target], by);
            }
            MemoryPayload::Delete { targets } => {
                if by == Address::Speaker {
                    return;
                }
                for target in targets {
                    if let Some(point) = self.points.get(&target) {
                        self.deleted_refs.extend(point.refs.iter().copied());
                    }
                    self.retired.insert(target, Retired::Deleted(event.seq));
                }
            }
        }
    }

    fn add(&mut self, event: &Event, refs: Vec<Seq>, supersedes: Vec<Seq>, by: Address) {
        let text = one_line(&event.body);
        if text.is_empty() {
            return;
        }
        for old in &supersedes {
            // A deletion stands: superseding a deleted point does not
            // un-delete it, and the newer point is simply in force.
            self.retired
                .entry(*old)
                .or_insert(Retired::Superseded(event.seq));
        }
        for term in terms(&text) {
            self.index.entry(term).or_default().push(event.seq);
        }
        self.points.insert(
            event.seq,
            MemoryPoint {
                seq: event.seq,
                ts: event.ts,
                text,
                refs,
                supersedes,
                by,
            },
        );
    }

    /// The points in force, oldest first.
    pub fn in_force(&self) -> impl DoubleEndedIterator<Item = &MemoryPoint> {
        self.points
            .values()
            .filter(|p| !self.retired.contains_key(&p.seq))
    }

    /// Every point ever written, oldest first, with how it was retired.
    pub fn history(&self) -> impl DoubleEndedIterator<Item = MemoryHit> + '_ {
        self.points.values().map(|p| self.hit(p))
    }

    pub fn get(&self, seq: Seq) -> Option<MemoryHit> {
        self.points.get(&seq).map(|p| self.hit(p))
    }

    fn hit(&self, point: &MemoryPoint) -> MemoryHit {
        MemoryHit {
            point: point.clone(),
            retired: self.retired.get(&point.seq).copied(),
        }
    }

    pub fn is_in_force(&self, seq: Seq) -> bool {
        self.points.contains_key(&seq) && !self.retired.contains_key(&seq)
    }

    /// Whether `seq` is cited by a point a person deleted.
    pub fn is_deleted_ref(&self, seq: Seq) -> bool {
        self.deleted_refs.contains(&seq)
    }

    /// The latest digest.
    pub fn latest_digest(&self) -> Option<&Digest> {
        self.digest.as_ref()
    }

    /// The highest seq any digest covered: discussion after it is new.
    pub fn digested_through(&self) -> Seq {
        self.through
    }

    /// Up to `limit` points matching `query`, best first. An empty query
    /// lists the newest. Retired points only with `all`.
    pub fn search(&self, query: &str, limit: usize, all: bool) -> Vec<MemoryHit> {
        let wanted = |seq: &Seq| all || !self.retired.contains_key(seq);
        let query_terms: BTreeSet<String> = terms(query).into_iter().collect();
        if query_terms.is_empty() {
            return self
                .points
                .values()
                .rev()
                .filter(|p| wanted(&p.seq))
                .take(limit)
                .map(|p| self.hit(p))
                .collect();
        }
        let mut scores: HashMap<Seq, usize> = HashMap::new();
        for term in &query_terms {
            let Some(seqs) = self.index.get(term) else {
                continue;
            };
            for seq in seqs.iter().filter(|s| wanted(s)) {
                *scores.entry(*seq).or_default() += 1;
            }
        }
        let mut ranked: Vec<(usize, Seq)> = scores.into_iter().map(|(s, n)| (n, s)).collect();
        ranked.sort_by(|a, b| b.cmp(a));
        ranked
            .into_iter()
            .take(limit)
            .filter_map(|(_, seq)| self.points.get(&seq))
            .map(|p| self.hit(p))
            .collect()
    }
}

/// A search term list: lowercase runs of ASCII letters and digits, and for
/// every other alphabetic run (CJK) its character bigrams, or the character
/// itself when it stands alone.
pub fn terms(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut run: Vec<char> = Vec::new();
    let flush_word = |word: &mut String, out: &mut Vec<String>| {
        if !word.is_empty() {
            out.push(std::mem::take(word));
        }
    };
    let flush_run = |run: &mut Vec<char>, out: &mut Vec<String>| {
        match run.len() {
            0 => {}
            1 => out.push(run[0].to_string()),
            _ => out.extend(run.windows(2).map(|w| w.iter().collect())),
        }
        run.clear();
    };
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            flush_run(&mut run, &mut out);
            word.push(c);
        } else if c.is_alphabetic() {
            flush_word(&mut word, &mut out);
            run.push(c);
        } else {
            flush_word(&mut word, &mut out);
            flush_run(&mut run, &mut out);
        }
    }
    flush_word(&mut word, &mut out);
    flush_run(&mut run, &mut out);
    out
}

/// `text` on one line, cut to [`POINT_CHARS`].
pub fn one_line(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(POINT_CHARS) {
        None => line,
        Some((i, _)) => format!("{}…", &line[..i]),
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::ids::{CommitteeId, EventId};

    fn committee() -> CommitteeId {
        CommitteeId::new("orchard").unwrap()
    }

    fn note(seq: u64, from: Address, body: &str, key: &str, payload: Value) -> Event {
        let mut meta = serde_json::Map::new();
        meta.insert(key.to_string(), payload);
        Event {
            seq: Seq(seq),
            committee: committee(),
            id: EventId::new(),
            ts: Utc.timestamp_opt(1_700_000_000 + seq as i64, 0).unwrap(),
            kind: Kind::Note,
            from,
            to: Vec::new(),
            thread: None,
            on_behalf_of: None,
            source: None,
            body: body.to_string(),
            refs: None,
            mirror: None,
            meta,
        }
    }

    fn point(seq: u64, body: &str, refs: &[u64], supersedes: &[u64]) -> Event {
        let payload = MemoryPayload::Point {
            refs: refs.iter().copied().map(Seq).collect(),
            supersedes: supersedes.iter().copied().map(Seq).collect(),
            digest: None,
        };
        note(
            seq,
            Address::Speaker,
            body,
            meta::MEMORY,
            payload.to_value(),
        )
    }

    fn person() -> Address {
        "senator::ana".parse().unwrap()
    }

    fn fold(events: &[Event]) -> MemoryFold {
        let mut fold = MemoryFold::new();
        for e in events {
            fold.observe(e);
        }
        fold
    }

    fn texts(fold: &MemoryFold) -> Vec<String> {
        fold.in_force().map(|p| p.text.clone()).collect()
    }

    #[test]
    fn a_newer_point_supersedes_an_older_one() {
        let fold = fold(&[
            point(10, "The release ships on Friday", &[3, 4], &[]),
            point(11, "Staging lives on the second cluster", &[5], &[]),
            point(20, "The release ships on Monday", &[15], &[10]),
        ]);
        assert_eq!(
            texts(&fold),
            [
                "Staging lives on the second cluster",
                "The release ships on Monday"
            ]
        );
        assert_eq!(
            fold.get(Seq(10)).unwrap().retired,
            Some(Retired::Superseded(Seq(20)))
        );
        assert_eq!(fold.history().count(), 3);
    }

    #[test]
    fn a_person_corrects_and_deletes() {
        let correct = note(
            30,
            person(),
            "The release ships on Tuesday",
            meta::MEMORY,
            MemoryPayload::Correct {
                target: Seq(20),
                refs: vec![Seq(15), Seq(29)],
            }
            .to_value(),
        );
        let delete = note(
            31,
            person(),
            "",
            meta::MEMORY,
            MemoryPayload::Delete {
                targets: vec![Seq(11)],
            }
            .to_value(),
        );
        let fold = fold(&[
            point(11, "Staging lives on the second cluster", &[5], &[]),
            point(20, "The release ships on Monday", &[15], &[]),
            correct,
            delete,
        ]);
        let in_force: Vec<_> = fold.in_force().collect();
        assert_eq!(in_force.len(), 1);
        assert_eq!(in_force[0].text, "The release ships on Tuesday");
        assert!(in_force[0].by_person());
        assert_eq!(
            fold.get(Seq(11)).unwrap().retired,
            Some(Retired::Deleted(Seq(31)))
        );
        assert!(fold.is_deleted_ref(Seq(5)));
        assert!(!fold.is_deleted_ref(Seq(15)));
    }

    #[test]
    fn a_correction_typed_without_sources_keeps_its_targets() {
        let typed = note(
            12,
            person(),
            "Deploys happen at night",
            meta::MEMORY,
            MemoryPayload::Correct {
                target: Seq(10),
                refs: vec![],
            }
            .to_value(),
        );
        let fold = fold(&[point(10, "Deploys happen at noon", &[3, 4], &[]), typed]);
        let point = fold.get(Seq(12)).unwrap().point;
        assert_eq!(point.refs, [Seq(3), Seq(4)]);
        assert_eq!(point.supersedes, [Seq(10)]);
    }

    #[test]
    fn only_the_speaker_writes_points_and_nobody_deletes_as_the_speaker() {
        let by_agent = note(
            5,
            "senator::bot".parse().unwrap(),
            "An agent's idea",
            meta::MEMORY,
            MemoryPayload::Point {
                refs: vec![],
                supersedes: vec![],
                digest: None,
            }
            .to_value(),
        );
        let speaker_delete = note(
            6,
            Address::Speaker,
            "",
            meta::MEMORY,
            MemoryPayload::Delete {
                targets: vec![Seq(4)],
            }
            .to_value(),
        );
        let fold = fold(&[point(4, "Kept", &[1], &[]), by_agent, speaker_delete]);
        assert_eq!(texts(&fold), ["Kept"]);
    }

    #[test]
    fn digests_are_tracked_by_their_window() {
        let digest = DigestPayload {
            trigger: DigestTrigger::Quiet,
            from: Seq(3),
            through: Seq(40),
            points: vec![DigestPoint {
                text: "Agreed on the schema".into(),
                refs: vec![Seq(7)],
            }],
        };
        let fold = fold(&[note(
            41,
            Address::Speaker,
            "- Agreed on the schema",
            meta::DIGEST,
            digest.to_value(),
        )]);
        assert_eq!(fold.digested_through(), Seq(40));
        assert_eq!(fold.latest_digest().unwrap().payload, digest);
    }

    #[test]
    fn search_ranks_by_matched_terms_and_handles_cjk() {
        let fold = fold(&[
            point(1, "发布日期定在周五", &[1], &[]),
            point(2, "Staging cluster is shared", &[1], &[]),
            point(3, "The staging release cluster moved", &[1], &[]),
        ]);
        let hits = fold.search("发布", 5, false);
        assert_eq!(hits[0].point.seq, Seq(1));
        let hits = fold.search("staging cluster release", 5, false);
        assert_eq!(
            hits.iter().map(|h| h.point.seq).collect::<Vec<_>>(),
            [Seq(3), Seq(2)]
        );
        assert_eq!(fold.search("", 1, false)[0].point.seq, Seq(3));
        assert!(fold.search("nothing-like-it", 5, false).is_empty());
    }

    #[test]
    fn search_leaves_retired_points_out_unless_asked() {
        let fold = fold(&[
            point(1, "Deploys happen at noon", &[1], &[]),
            point(2, "Deploys happen at night", &[2], &[1]),
        ]);
        assert_eq!(fold.search("deploys", 5, false).len(), 1);
        assert_eq!(fold.search("deploys", 5, true).len(), 2);
    }

    #[test]
    fn refolding_from_the_log_gives_the_same_memory() {
        let events = [
            point(1, "Deploys happen at noon", &[1], &[]),
            point(2, "Deploys happen at night", &[2], &[1]),
            point(3, "发布日期定在周五", &[1], &[]),
        ];
        let whole = fold(&events);
        let mut incremental = fold(&events[..1]);
        for e in &events {
            incremental.observe(e); // overlapping pages are ignored
        }
        assert_eq!(
            whole.search("deploys 发布", 10, true),
            incremental.search("deploys 发布", 10, true)
        );
        assert_eq!(whole.head(), incremental.head());
    }

    #[test]
    fn terms_split_words_and_cjk_bigrams() {
        assert_eq!(terms("Ship v2 发布日"), ["ship", "v2", "发布", "布日"]);
        assert_eq!(terms("单"), ["单"]);
    }
}
