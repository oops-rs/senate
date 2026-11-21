//! One conformance suite, run against every [`LogStore`].
//!
//! The Postgres run needs `SENATE_TEST_DATABASE_URL` (for example
//! `postgres://senate:senate@127.0.0.1:5432/senate`, from
//! `deploy/docker-compose.yml`) and skips cleanly without it. Each Postgres
//! test runs in a fresh schema, dropped afterwards, so tests run in parallel
//! and assert exact results.

// The suite's case functions are shared by two `#[tokio::test]` wrappers, so
// clippy does not see them as test code.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::str::FromStr;
use std::sync::Arc;

use senate_core::api::Heartbeat;
use senate_core::hub::{matches_terms, search_terms};
use senate_core::{
    Address, CommitteeId, Event, EventId, Kind, Mirror, MirrorRef, NewEvent, Refs, SenatorName,
    Seq, Source,
};
use senate_log::{CursorOwner, LogError, LogStore, MemoryStore, PgStore, SqliteStore};
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, PgPool};

const PG_URL_ENV: &str = "SENATE_TEST_DATABASE_URL";

struct Harness {
    store: Arc<dyn LogStore>,
    /// Keeps the SQLite file alive for the test's length.
    _dir: Option<tempfile::TempDir>,
    /// `(admin pool, store pool, schema)` for Postgres runs.
    pg: Option<(PgPool, PgPool, String)>,
}

impl Harness {
    fn memory() -> Self {
        Self {
            store: Arc::new(MemoryStore::new()),
            _dir: None,
            pg: None,
        }
    }

    async fn sqlite() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(dir.path().join("senate.db"))
            .await
            .unwrap();
        Self {
            store: Arc::new(store),
            _dir: Some(dir),
            pg: None,
        }
    }

    async fn postgres() -> Option<Self> {
        let Ok(url) = std::env::var(PG_URL_ENV) else {
            eprintln!("skipping Postgres conformance: {PG_URL_ENV} is unset");
            return None;
        };
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect admin pool");
        let schema = format!("conf_{}", EventId::new().to_string().to_lowercase());
        sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .expect("create schema");
        install_pg_trgm(&admin).await;
        // `public` second, for `pg_trgm`'s functions and operator classes.
        let search_path = format!("{schema},public");
        let options = PgConnectOptions::from_str(&url)
            .expect("parse url")
            .options([("search_path", search_path.as_str())]);
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await
            .expect("connect store pool");
        let store = PgStore::from_pool(pool.clone());
        store.migrate().await.expect("migrate");
        // Migrating twice is a no-op.
        store.migrate().await.expect("migrate again");
        Some(Self {
            store: Arc::new(store),
            _dir: None,
            pg: Some((admin, pool, schema)),
        })
    }

    async fn finish(self) {
        if let Some((admin, pool, schema)) = self.pg {
            pool.close().await;
            sqlx::query(AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
                .execute(&admin)
                .await
                .expect("drop schema");
        }
    }
}

/// Install `pg_trgm` in `public` before any test schema migrates. Left to the
/// migration, it would land in the first test schema on the search path and
/// be dropped with it, taking every other schema's trigram index along.
/// Parallel tests race to create it; the loser checks it exists.
async fn install_pg_trgm(admin: &PgPool) {
    if let Err(error) = sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_trgm SCHEMA public")
        .execute(admin)
        .await
    {
        let installed: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pg_trgm')",
        )
        .fetch_one(admin)
        .await
        .expect("check pg_trgm");
        assert!(installed, "install pg_trgm: {error}");
    }
}

fn committee(name: &str) -> CommitteeId {
    CommitteeId::new(name).unwrap()
}

fn path(s: &str) -> SenatorName {
    s.parse().unwrap()
}

fn chat(f: &CommitteeId, body: &str) -> NewEvent {
    NewEvent::new(f.clone(), Kind::Chat, Address::Senator(path("alice")), body)
}

fn seqs(events: &[Event]) -> Vec<u64> {
    events.iter().map(|e| e.seq.0).collect()
}

fn bodies(events: &[Event]) -> Vec<&str> {
    events.iter().map(|e| e.body.as_str()).collect()
}

async fn appends_assign_gapless_seq_per_committee(store: Arc<dyn LogStore>) {
    let a = committee("refund-v2");
    let b = committee("search");
    assert_eq!(store.head(&a).await.unwrap(), Seq::ZERO);
    for n in 1..=3 {
        let new = chat(&a, &format!("a{n}"));
        let appended = store.append(new.clone()).await.unwrap();
        assert!(!appended.duplicate);
        assert_eq!(appended.event.seq, Seq(n));
        assert_eq!(
            appended.event,
            new.into_event(Seq(n), appended.event.ts),
            "the stored event is the new event plus seq and ts"
        );
    }
    for n in 1..=2 {
        let appended = store.append(chat(&b, "b")).await.unwrap();
        assert_eq!(appended.event.seq, Seq(n));
    }
    assert_eq!(store.head(&a).await.unwrap(), Seq(3));
    assert_eq!(store.head(&b).await.unwrap(), Seq(2));
    let events = store.read(&a, Seq::ZERO, 100).await.unwrap();
    assert!(events.windows(2).all(|w| w[0].ts <= w[1].ts));
}

async fn append_is_idempotent_on_committee_and_id(store: Arc<dyn LogStore>) {
    let f = committee("refund-v2");
    let first = store.append(chat(&f, "original")).await.unwrap();
    let mut retry = chat(&f, "edited on retry").with_id(first.event.id.clone());
    retry.kind = Kind::Note;
    let again = store.append(retry).await.unwrap();
    assert!(again.duplicate);
    assert_eq!(
        again.event, first.event,
        "a duplicate returns the stored event"
    );

    let next = store.append(chat(&f, "next")).await.unwrap();
    assert_eq!(next.event.seq, Seq(2), "a duplicate never consumes a seq");

    let other = committee("search");
    let elsewhere = store
        .append(chat(&other, "same id, other committee").with_id(first.event.id.clone()))
        .await
        .unwrap();
    assert!(!elsewhere.duplicate, "idempotence is per committee");
    assert_eq!(elsewhere.event.seq, Seq(1));
}

async fn read_and_get_page_in_order(store: Arc<dyn LogStore>) {
    let f = committee("refund-v2");
    for n in 1..=10 {
        store.append(chat(&f, &format!("m{n}"))).await.unwrap();
    }
    let page = store.read(&f, Seq::ZERO, 3).await.unwrap();
    assert_eq!(
        page.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![Seq(1), Seq(2), Seq(3)]
    );
    let rest = store.read(&f, Seq(3), 100).await.unwrap();
    assert_eq!(rest.len(), 7);
    assert_eq!(rest.first().unwrap().seq, Seq(4));
    assert_eq!(rest.last().unwrap().body, "m10");
    assert!(store.read(&f, Seq(10), 100).await.unwrap().is_empty());
    assert!(
        store
            .read(&f, Seq(u64::MAX / 2), 1)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(store.read(&f, Seq::ZERO, 0).await.unwrap().is_empty());
    assert!(
        store
            .read(&committee("nope"), Seq::ZERO, 10)
            .await
            .unwrap()
            .is_empty()
    );

    assert_eq!(store.get(&f, Seq(4)).await.unwrap().unwrap().body, "m4");
    assert!(store.get(&f, Seq::ZERO).await.unwrap().is_none());
    assert!(store.get(&f, Seq(11)).await.unwrap().is_none());
    assert!(store.get(&f, Seq(u64::MAX)).await.unwrap().is_none());
    assert!(
        store
            .get(&committee("nope"), Seq(1))
            .await
            .unwrap()
            .is_none()
    );
}

async fn every_envelope_field_round_trips(store: Arc<dyn LogStore>) {
    let f = committee("refund-v2");
    let root = store.append(chat(&f, "root")).await.unwrap().event;
    let new = NewEvent::new(
        f.clone(),
        Kind::Other("hologram".into()),
        Address::Speaker,
        "网关的幂等键用哪个 header？",
    )
    .with_to(vec![Address::Senator(path("claude")), Address::Committee])
    .in_thread(root.seq)
    .on_behalf_of(Address::Senator(path("pm")))
    .with_source(Source {
        platform: "chat".into(),
        group_id: "backend".into(),
        group_name: Some("退款 v2 · 后端群".into()),
        simulated: false,
    })
    .with_refs(Refs {
        commit: Some("a41f9c2".into()),
        path: Some("gateway/src/idem.rs".into()),
        branch: None,
    })
    .with_mirror(Mirror {
        platform: "chat".into(),
        message_ref: "mref_1".into(),
        thread_ref: Some("thr_1".into()),
    })
    .with_meta("addressing", json!("mention"))
    .with_meta(
        "nested",
        json!({"z": [1, 2.5, null, true], "a": {"deep": "值"}, "big": u64::MAX}),
    );
    let appended = store.append(new).await.unwrap().event;
    let read = store.get(&f, appended.seq).await.unwrap().unwrap();
    assert_eq!(read, appended);
    let paged = store.read(&f, root.seq, 10).await.unwrap();
    assert_eq!(paged, vec![appended]);
}

async fn committees_lists_only_committees_with_events_sorted(store: Arc<dyn LogStore>) {
    assert!(store.committees().await.unwrap().is_empty());
    for name in ["b-x", "ab", "a", "a-b"] {
        store.append(chat(&committee(name), "hi")).await.unwrap();
    }
    // Byte order, not locale collation: '-' sorts before letters.
    assert_eq!(
        store.committees().await.unwrap(),
        vec![
            committee("a"),
            committee("a-b"),
            committee("ab"),
            committee("b-x")
        ]
    );
}

async fn concurrent_appends_stay_gapless(store: Arc<dyn LogStore>) {
    let f = committee("refund-v2");
    let tasks: Vec<_> = (0..40)
        .map(|n| {
            let store = store.clone();
            let f = f.clone();
            tokio::spawn(async move { store.append(chat(&f, &format!("c{n}"))).await })
        })
        .collect();
    let mut seqs = BTreeSet::new();
    for task in tasks {
        let appended = task.await.unwrap().unwrap();
        assert!(!appended.duplicate);
        assert!(seqs.insert(appended.event.seq), "seq assigned twice");
    }
    assert_eq!(seqs, (1..=40).map(Seq).collect::<BTreeSet<_>>());
    assert_eq!(store.head(&f).await.unwrap(), Seq(40));
    let events = store.read(&f, Seq::ZERO, 100).await.unwrap();
    assert!(
        events
            .iter()
            .enumerate()
            .all(|(i, e)| e.seq == Seq(i as u64 + 1))
    );
}

async fn concurrent_duplicates_store_once(store: Arc<dyn LogStore>) {
    let f = committee("refund-v2");
    store.append(chat(&f, "before")).await.unwrap();
    let new = chat(&f, "raced");
    let tasks: Vec<_> = (0..20)
        .map(|_| {
            let store = store.clone();
            let new = new.clone();
            tokio::spawn(async move { store.append(new).await })
        })
        .collect();
    let mut originals = 0;
    for task in tasks {
        let appended = task.await.unwrap().unwrap();
        assert_eq!(appended.event.seq, Seq(2));
        assert_eq!(appended.event.id, new.id);
        if !appended.duplicate {
            originals += 1;
        }
    }
    assert_eq!(originals, 1, "exactly one append stores the event");
    assert_eq!(store.head(&f).await.unwrap(), Seq(2));
    let after = store.append(chat(&f, "after")).await.unwrap();
    assert_eq!(after.event.seq, Seq(3), "raced duplicates consumed no seq");
}

async fn cursors_overwrite_and_are_scoped(store: Arc<dyn LogStore>) {
    let f = committee("refund-v2");
    let g = committee("search");
    let alice = CursorOwner::from(path("copilot"));
    let bob = CursorOwner::from(path("claude"));
    assert_eq!(store.cursor(&f, &alice, "delivered").await.unwrap(), None);
    store
        .set_cursor(&f, &alice, "delivered", Seq(5))
        .await
        .unwrap();
    assert_eq!(
        store.cursor(&f, &alice, "delivered").await.unwrap(),
        Some(Seq(5))
    );
    store
        .set_cursor(&f, &alice, "delivered", Seq(3))
        .await
        .unwrap();
    assert_eq!(
        store.cursor(&f, &alice, "delivered").await.unwrap(),
        Some(Seq(3)),
        "cursors overwrite; a consumer may rewind"
    );
    store
        .set_cursor(&f, &alice, "delivered", Seq::ZERO)
        .await
        .unwrap();
    assert_eq!(
        store.cursor(&f, &alice, "delivered").await.unwrap(),
        Some(Seq::ZERO)
    );
    assert_eq!(store.cursor(&f, &alice, "read").await.unwrap(), None);
    assert_eq!(store.cursor(&f, &bob, "delivered").await.unwrap(), None);
    assert_eq!(store.cursor(&g, &alice, "delivered").await.unwrap(), None);
    store
        .set_cursor(&f, &CursorOwner::Hub, "speaker", Seq(9))
        .await
        .unwrap();
    assert_eq!(
        store
            .cursor(&f, &CursorOwner::Hub, "speaker")
            .await
            .unwrap(),
        Some(Seq(9))
    );
    assert!(
        store.committees().await.unwrap().is_empty(),
        "cursors are not events"
    );
}

async fn two_agents_keep_separate_cursors(store: Arc<dyn LogStore>) {
    let f = committee("refund-v2");
    let claude = CursorOwner::from(path("copilot"));
    let codex = CursorOwner::from(path("codex"));
    let person = CursorOwner::from(path("alice"));
    store
        .set_cursor(&f, &claude, "delivered", Seq(7))
        .await
        .unwrap();
    store
        .set_cursor(&f, &codex, "delivered", Seq(2))
        .await
        .unwrap();
    assert_eq!(
        store.cursor(&f, &claude, "delivered").await.unwrap(),
        Some(Seq(7))
    );
    assert_eq!(
        store.cursor(&f, &codex, "delivered").await.unwrap(),
        Some(Seq(2))
    );
    assert_eq!(
        store.cursor(&f, &person, "delivered").await.unwrap(),
        None,
        "the person's own cursor is a third one"
    );
    // A third agent, never set, has none.
    let third = CursorOwner::from(path("reviewer"));
    assert_eq!(store.cursor(&f, &third, "delivered").await.unwrap(), None);
}

async fn unresolved_events_are_refused(store: Arc<dyn LogStore>) {
    let f = committee("refund-v2");
    let wildcard: Address = "senator::*".parse().unwrap();
    let to_wildcard = chat(&f, "to any").to(wildcard.clone());
    let from_wildcard = NewEvent::new(f.clone(), Kind::Chat, wildcard.clone(), "from any");
    let behalf = chat(&f, "behalf").on_behalf_of(wildcard);
    for event in [to_wildcard, from_wildcard, behalf] {
        assert!(matches!(
            store.append(event).await,
            Err(LogError::Invalid(_))
        ));
    }
    assert_eq!(store.head(&f).await.unwrap(), Seq::ZERO, "nothing stored");
    assert!(store.committees().await.unwrap().is_empty());
    let concrete = chat(&f, "to one").to("senator::claude".parse().unwrap());
    assert_eq!(store.append(concrete).await.unwrap().event.seq, Seq(1));
}

async fn mirrors_are_first_write_wins_and_indexed_by_message(store: Arc<dyn LogStore>) {
    let f = committee("refund-v2");
    let mirror = |seq: u64, platform: &str, message_ref: &str| MirrorRef {
        committee: f.clone(),
        seq: Seq(seq),
        platform: platform.into(),
        target: "backend".into(),
        message_ref: message_ref.into(),
        thread_ref: Some(format!("thr_{seq}")),
    };
    assert!(
        store
            .mirrors_of_seq(&f, Seq(120), "chat")
            .await
            .unwrap()
            .is_empty()
    );
    store
        .put_mirror(mirror(120, "chat", "om_root"))
        .await
        .unwrap();
    store
        .put_mirror(mirror(120, "chat", "om_root"))
        .await
        .unwrap();
    store
        .put_mirror(mirror(120, "chat", "om_other"))
        .await
        .unwrap();
    assert_eq!(
        store.mirrors_of_seq(&f, Seq(120), "chat").await.unwrap(),
        vec![mirror(120, "chat", "om_root")],
        "first write wins"
    );
    assert!(
        store
            .mirrors_of_seq(&f, Seq(120), "other")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.mirror_of_message("chat", "om_other").await.unwrap(),
        None
    );

    // Two log events mirrored onto one platform message: the lower seq wins.
    store
        .put_mirror(mirror(130, "chat", "om_shared"))
        .await
        .unwrap();
    store
        .put_mirror(mirror(125, "chat", "om_shared"))
        .await
        .unwrap();
    assert_eq!(
        store
            .mirror_of_message("chat", "om_shared")
            .await
            .unwrap()
            .map(|m| m.seq),
        Some(Seq(125))
    );
    assert_eq!(
        store.mirror_of_message("chat", "om_root").await.unwrap(),
        Some(mirror(120, "chat", "om_root"))
    );
    assert_eq!(
        store.mirror_of_message("other", "om_root").await.unwrap(),
        None
    );
    let no_thread = MirrorRef {
        thread_ref: None,
        ..mirror(140, "chat", "om_flat")
    };
    store.put_mirror(no_thread.clone()).await.unwrap();
    assert_eq!(
        store.mirrors_of_seq(&f, Seq(140), "chat").await.unwrap(),
        vec![no_thread]
    );
}

async fn one_event_may_have_several_copies(store: Arc<dyn LogStore>) {
    let f = committee("refund-v2");
    let copy = |seq: u64, target: &str, message_ref: &str| MirrorRef {
        committee: f.clone(),
        seq: Seq(seq),
        platform: "chat".into(),
        target: target.into(),
        message_ref: message_ref.into(),
        thread_ref: None,
    };
    // Written out of target order; read back in byte order of target.
    store
        .put_mirror(copy(120, "refund-backend", "om_question"))
        .await
        .unwrap();
    store
        .put_mirror(copy(120, "pm-group", "om_human"))
        .await
        .unwrap();
    store
        .put_mirror(copy(120, "refund-backend#ack", "om_ack"))
        .await
        .unwrap();
    // First write wins per target, not per event.
    store
        .put_mirror(copy(120, "pm-group", "om_retry"))
        .await
        .unwrap();
    assert_eq!(
        store.mirrors_of_seq(&f, Seq(120), "chat").await.unwrap(),
        vec![
            copy(120, "pm-group", "om_human"),
            copy(120, "refund-backend", "om_question"),
            copy(120, "refund-backend#ack", "om_ack"),
        ]
    );
    assert!(
        store
            .mirrors_of_seq(&f, Seq(121), "chat")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .mirrors_of_seq(&committee("search"), Seq(120), "chat")
            .await
            .unwrap()
            .is_empty()
    );
    for (message_ref, target) in [
        ("om_human", "pm-group"),
        ("om_question", "refund-backend"),
        ("om_ack", "refund-backend#ack"),
    ] {
        assert_eq!(
            store
                .mirror_of_message("chat", message_ref)
                .await
                .unwrap()
                .map(|m| m.target),
            Some(target.to_string())
        );
    }
    assert_eq!(
        store.mirror_of_message("chat", "om_retry").await.unwrap(),
        None
    );

    // Several copies claim one message: lowest seq, then lowest target.
    store.put_mirror(copy(131, "0", "om_shared")).await.unwrap();
    store.put_mirror(copy(130, "b", "om_shared")).await.unwrap();
    store.put_mirror(copy(130, "a", "om_shared")).await.unwrap();
    assert_eq!(
        store.mirror_of_message("chat", "om_shared").await.unwrap(),
        Some(copy(130, "a", "om_shared"))
    );
}

async fn epoch_is_stable(store: Arc<dyn LogStore>) {
    let epoch = store.epoch().await.unwrap();
    assert_eq!(epoch.len(), 26, "a ULID: {epoch:?}");
    assert!(epoch.bytes().all(|b| b.is_ascii_alphanumeric()));
    store
        .append(chat(&committee("refund-v2"), "x"))
        .await
        .unwrap();
    assert_eq!(
        store.epoch().await.unwrap(),
        epoch,
        "appends keep the epoch"
    );
}

async fn thread_returns_the_root_then_its_replies(store: Arc<dyn LogStore>) {
    let f = committee("refund-v2");
    let g = committee("search");
    let reply = |root: Seq, body: &str| chat(&f, body).in_thread(root);
    let root = store.append(chat(&f, "root")).await.unwrap().event;
    let other = store.append(chat(&f, "other root")).await.unwrap().event;
    store.append(reply(root.seq, "r1")).await.unwrap();
    store.append(reply(other.seq, "elsewhere")).await.unwrap();
    store.append(reply(root.seq, "r2")).await.unwrap();
    store.append(chat(&f, "unthreaded")).await.unwrap();
    store.append(reply(root.seq, "r3")).await.unwrap();
    // The same root seq in another committee.
    store
        .append(chat(&g, "g1").in_thread(root.seq))
        .await
        .unwrap();

    let thread = store.thread(&f, root.seq).await.unwrap();
    assert_eq!(bodies(&thread), vec!["root", "r1", "r2", "r3"]);
    assert_eq!(thread[0], root);
    assert_eq!(seqs(&thread), vec![1, 3, 5, 7]);
    assert_eq!(
        bodies(&store.thread(&f, other.seq).await.unwrap()),
        vec!["other root", "elsewhere"]
    );
    assert_eq!(
        seqs(&store.thread(&f, Seq(6)).await.unwrap()),
        vec![6],
        "a leaf is a thread of one"
    );

    // An event naming a root at or after its own seq is not a reply: the
    // thread is the root first, then what follows it.
    let early = store.append(reply(Seq(10), "names seq 10")).await.unwrap();
    assert_eq!(early.event.seq, Seq(8));
    assert!(store.thread(&f, Seq(10)).await.unwrap().is_empty());
    store.append(chat(&f, "9")).await.unwrap();
    let root_10 = store.append(reply(Seq(10), "is seq 10")).await.unwrap();
    assert_eq!(root_10.event.seq, Seq(10));
    store.append(reply(Seq(10), "r 10")).await.unwrap();
    assert_eq!(
        seqs(&store.thread(&f, Seq(10)).await.unwrap()),
        vec![10, 11]
    );

    // Replies to a root that is not stored still come back.
    let orphan = store.append(reply(Seq::ZERO, "orphan")).await.unwrap();
    assert_eq!(
        seqs(&store.thread(&f, Seq::ZERO).await.unwrap()),
        vec![orphan.event.seq.0]
    );

    assert!(store.thread(&f, Seq(500)).await.unwrap().is_empty());
    assert!(
        store
            .thread(&committee("nope"), root.seq)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(store.thread(&f, Seq(u64::MAX)).await.unwrap().is_empty());
}

async fn bindings_list_every_committees_binding_events(store: Arc<dyn LogStore>) {
    assert!(store.bindings().await.unwrap().is_empty());
    let binding = |f: &CommitteeId, body: &str| {
        NewEvent::new(f.clone(), Kind::Binding, Address::Speaker, body)
    };
    let a = committee("a-b");
    let ab = committee("ab");
    let b = committee("b-x");
    store.append(chat(&b, "chat")).await.unwrap();
    store.append(binding(&b, "b bind")).await.unwrap();
    store.append(binding(&ab, "ab bind")).await.unwrap();
    store.append(binding(&a, "a bind")).await.unwrap();
    store.append(chat(&a, "chat")).await.unwrap();
    store.append(binding(&a, "a unbind")).await.unwrap();
    store.append(binding(&b, "b unbind")).await.unwrap();
    store
        .append(NewEvent::new(
            b.clone(),
            Kind::Other("binding-ish".into()),
            Address::Speaker,
            "not a binding",
        ))
        .await
        .unwrap();
    let bindings = store.bindings().await.unwrap();
    assert!(bindings.iter().all(|e| e.kind == Kind::Binding));
    assert_eq!(
        bindings
            .iter()
            .map(|e| (e.committee.as_str(), e.seq.0, e.body.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("a-b", 1, "a bind"),
            ("a-b", 3, "a unbind"),
            ("ab", 1, "ab bind"),
            ("b-x", 2, "b bind"),
            ("b-x", 3, "b unbind"),
        ],
        "committees in byte order, then seq"
    );
}

/// `store.search`, checked against the contract: the first hits are exactly
/// `exact` (every term matches, newest first); anything after them is a near
/// match, which only a store with a text index returns, and which never
/// matches every term. Every hit is of `f`, and there are at most `limit`.
async fn assert_search(
    store: &Arc<dyn LogStore>,
    f: &CommitteeId,
    query: &str,
    limit: usize,
    exact: &[u64],
) -> Vec<Event> {
    let hits = store.search(f, query, limit).await.unwrap();
    let terms = search_terms(query);
    let split = exact.len().min(hits.len());
    assert_eq!(seqs(&hits[..split]), exact, "exact matches of {query:?}");
    assert!(
        hits[split..]
            .iter()
            .all(|e| !matches_terms(&e.body, &terms)),
        "near matches of {query:?} follow the exact ones: {:?}",
        bodies(&hits)
    );
    assert!(hits.len() <= limit, "{query:?} is limited to {limit}");
    assert!(hits.iter().all(|e| e.committee == *f), "only {f}'s events");
    hits
}

async fn search_matches_every_term_newest_first(store: Arc<dyn LogStore>) {
    let f = committee("refund-v2");
    for body in [
        "Refund limit is 30 days",
        "the REFUND window",
        "unrelated",
        "100% refunded",
        "refund_policy.md updated",
        "退款上限是 30 天",
        "path C:\\refund\\x",
        "退款 Limit 改为 14 天",
    ] {
        store.append(chat(&f, body)).await.unwrap();
    }
    let other = committee("search");
    store
        .append(chat(&other, "refund limit elsewhere"))
        .await
        .unwrap();
    let store = &store;

    assert_search(store, &f, "refund", 100, &[7, 5, 4, 2, 1]).await;
    let limited = assert_search(store, &f, "ReFuNd", 2, &[7, 5]).await;
    assert_eq!(seqs(&limited), [7, 5], "newest first, limited");
    assert_search(store, &f, "refund", 0, &[]).await;
    assert_search(store, &f, "%", 100, &[4]).await;
    assert_search(store, &f, "d_p", 100, &[5]).await;
    assert_search(store, &f, "0_d", 100, &[]).await;
    assert_search(store, &f, "1%r", 100, &[]).await;
    assert_search(store, &f, "\\refund\\", 100, &[7]).await;

    // Every term must match, in any order, case-insensitively.
    assert_search(store, &f, "limit refund", 100, &[1]).await;
    assert_search(store, &f, "  LIMIT\t30 ", 100, &[1]).await;
    assert_search(store, &f, "refund nothing", 100, &[]).await;

    // Chinese needs no tokenizer: a substring is a term.
    assert_search(store, &f, "上限", 100, &[6]).await;
    assert_search(store, &f, "退款 天", 100, &[8, 6]).await;
    assert_search(store, &f, "退款 limit", 100, &[8]).await;

    // No terms match nothing.
    for blank in ["", "   ", "\t\n"] {
        assert!(store.search(&f, blank, 10).await.unwrap().is_empty());
    }

    // Another committee's events never show up, and are found in their own.
    let elsewhere = assert_search(store, &other, "refund limit", 10, &[1]).await;
    assert_eq!(seqs(&elsewhere), [1]);
    assert!(
        store
            .search(&committee("nope"), "refund", 10)
            .await
            .unwrap()
            .is_empty()
    );
}

async fn heartbeats_keep_the_latest_per_agent(store: Arc<dyn LogStore>) {
    let f = committee("refund-v2");
    let claude = path("copilot");
    let codex = path("codex");
    assert!(store.last_heartbeat(&f, &claude).await.unwrap().is_none());
    let first = senate_log::now();
    store
        .record_heartbeat(
            &f,
            Heartbeat {
                agent: claude.clone(),
                sessions: 1,
                cursor: Seq(4),
                left: false,
            },
            first,
        )
        .await
        .unwrap();
    let second = senate_log::now();
    let beat = Heartbeat {
        agent: claude.clone(),
        sessions: 2,
        cursor: Seq(9),
        left: false,
    };
    store
        .record_heartbeat(&f, beat.clone(), second)
        .await
        .unwrap();
    let record = store.last_heartbeat(&f, &claude).await.unwrap().unwrap();
    assert_eq!(record.heartbeat, beat);
    assert_eq!(record.at, second);
    assert_eq!(record.committee, f);
    assert_eq!(record.agent, claude);

    // Another agent of the same person has its own, and the person has none.
    assert!(store.last_heartbeat(&f, &codex).await.unwrap().is_none());
    assert!(
        store
            .last_heartbeat(&f, &path("alice"))
            .await
            .unwrap()
            .is_none()
    );
    let other = Heartbeat {
        agent: codex.clone(),
        sessions: 1,
        cursor: Seq(1),
        left: false,
    };
    store
        .record_heartbeat(&f, other.clone(), second)
        .await
        .unwrap();
    assert_eq!(
        store
            .last_heartbeat(&f, &codex)
            .await
            .unwrap()
            .unwrap()
            .heartbeat,
        other
    );
    assert_eq!(
        store
            .last_heartbeat(&f, &claude)
            .await
            .unwrap()
            .unwrap()
            .heartbeat,
        beat,
        "one agent's beat does not touch the other's"
    );
    assert!(
        store
            .last_heartbeat(&committee("search"), &claude)
            .await
            .unwrap()
            .is_none()
    );

    // All of a committee's, by agent path.
    let all = store.heartbeats(&f).await.unwrap();
    assert_eq!(
        all.iter().map(|r| r.agent.to_string()).collect::<Vec<_>>(),
        ["codex", "copilot"]
    );
    assert_eq!(all[1].heartbeat, beat);
    assert!(
        store
            .heartbeats(&committee("search"))
            .await
            .unwrap()
            .is_empty()
    );
}

async fn registry_reads_one_exact_key(store: Arc<dyn LogStore>) {
    let key = "gateway:test:%_";
    let other = format!("{key}:pending");
    store
        .put_registry(&other, Some(json!("other")))
        .await
        .unwrap();
    assert_eq!(store.registry_value(key).await.unwrap(), None);
    store
        .put_registry(key, Some(json!({"state": "done"})))
        .await
        .unwrap();
    assert_eq!(
        store.registry_value(key).await.unwrap(),
        Some(json!({"state": "done"}))
    );
    store.put_registry(key, None).await.unwrap();
    assert_eq!(store.registry_value(key).await.unwrap(), None);
    assert_eq!(
        store.registry_value(&other).await.unwrap(),
        Some(json!("other"))
    );
}

macro_rules! conformance {
    ($($case:ident),* $(,)?) => {
        mod memory {
            $(
                #[tokio::test]
                async fn $case() {
                    let harness = super::Harness::memory();
                    super::$case(harness.store.clone()).await;
                    harness.finish().await;
                }
            )*
        }

        mod sqlite {
            $(
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                async fn $case() {
                    let harness = super::Harness::sqlite().await;
                    super::$case(harness.store.clone()).await;
                    harness.finish().await;
                }
            )*
        }

        mod postgres {
            $(
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                async fn $case() {
                    let Some(harness) = super::Harness::postgres().await else {
                        return;
                    };
                    super::$case(harness.store.clone()).await;
                    harness.finish().await;
                }
            )*
        }
    };
}

conformance!(
    registry_reads_one_exact_key,
    appends_assign_gapless_seq_per_committee,
    append_is_idempotent_on_committee_and_id,
    read_and_get_page_in_order,
    every_envelope_field_round_trips,
    committees_lists_only_committees_with_events_sorted,
    concurrent_appends_stay_gapless,
    concurrent_duplicates_store_once,
    cursors_overwrite_and_are_scoped,
    two_agents_keep_separate_cursors,
    unresolved_events_are_refused,
    mirrors_are_first_write_wins_and_indexed_by_message,
    one_event_may_have_several_copies,
    epoch_is_stable,
    thread_returns_the_root_then_its_replies,
    bindings_list_every_committees_binding_events,
    search_matches_every_term_newest_first,
    heartbeats_keep_the_latest_per_agent,
);

#[tokio::test]
async fn every_memory_store_is_a_new_epoch() {
    let a = MemoryStore::new().epoch().await.unwrap();
    let b = MemoryStore::new().epoch().await.unwrap();
    assert_ne!(a, b);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_postgres_log_keeps_its_epoch_and_a_new_one_gets_its_own() {
    let Some(first) = Harness::postgres().await else {
        return;
    };
    let Some(second) = Harness::postgres().await else {
        return;
    };
    let epoch = first.store.epoch().await.unwrap();
    assert_ne!(epoch, second.store.epoch().await.unwrap());

    // Reopening (and re-migrating) the same database keeps its epoch.
    let (_, pool, _) = first.pg.as_ref().unwrap();
    let reopened = PgStore::from_pool(pool.clone());
    reopened.migrate().await.unwrap();
    assert_eq!(reopened.epoch().await.unwrap(), epoch);

    // The documented reset: delete the row, and the next migration mints a
    // new epoch.
    sqlx::query("DELETE FROM log_meta")
        .execute(pool)
        .await
        .unwrap();
    assert!(matches!(
        reopened.epoch().await,
        Err(senate_log::LogError::Corrupt(_))
    ));
    reopened.migrate().await.unwrap();
    let minted = reopened.epoch().await.unwrap();
    assert_ne!(minted, epoch);
    assert_eq!(first.store.epoch().await.unwrap(), minted);

    first.finish().await;
    second.finish().await;
}

/// Near matches are the Postgres store's own: after the exact ones, by
/// similarity, never past `limit`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_search_appends_near_matches() {
    let Some(harness) = Harness::postgres().await else {
        return;
    };
    let store = harness.store.clone();
    let f = committee("refund-v2");
    for body in [
        "the refund window",
        "refnud policy draft",
        "unrelated",
        "refund limit",
    ] {
        store.append(chat(&f, body)).await.unwrap();
    }
    let hits = store.search(&f, "refund", 10).await.unwrap();
    assert_eq!(seqs(&hits), [4, 1, 2], "exact newest first, then the typo");
    let hits = store.search(&f, "refund", 2).await.unwrap();
    assert_eq!(seqs(&hits), [4, 1], "exact matches fill the limit first");
    let hits = store.search(&f, "refnud", 10).await.unwrap();
    assert_eq!(hits[0].seq, Seq(2), "the exact match leads");
    assert!(seqs(&hits).contains(&1) && seqs(&hits).contains(&4));
    assert!(!seqs(&hits).contains(&3), "dissimilar bodies stay out");
    harness.finish().await;
}

#[tokio::test]
async fn connect_selects_the_store_by_url() {
    let store = senate_log::connect(senate_log::MEMORY_URL).await.unwrap();
    assert!(store.committees().await.unwrap().is_empty());
    assert!(matches!(
        senate_log::connect("mysql://nope").await,
        Err(LogError::Invalid(_))
    ));

    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}", dir.path().join("log.db").display());
    let store = senate_log::connect(&url).await.unwrap();
    let f = committee("refund-v2");
    store.append(chat(&f, "kept")).await.unwrap();
    let epoch = store.epoch().await.unwrap();
    drop(store);

    // Reopening the file finds the same log and the same epoch.
    let reopened = senate_log::connect(&url).await.unwrap();
    assert_eq!(reopened.epoch().await.unwrap(), epoch);
    assert_eq!(reopened.head(&f).await.unwrap(), Seq(1));
    assert_eq!(
        reopened.read(&f, Seq::ZERO, 10).await.unwrap()[0].body,
        "kept"
    );
}

#[tokio::test]
async fn every_sqlite_file_is_a_new_epoch() {
    let a = Harness::sqlite().await;
    let b = Harness::sqlite().await;
    assert_ne!(
        a.store.epoch().await.unwrap(),
        b.store.epoch().await.unwrap()
    );
}
