//! A machine under a parent: the team's Senate `acme` above (its Speaker
//! bob, a person; no built-in Speaker), and alice's machine below running
//! `senated --machine` with `[local]` and the team's `server_url`: it hosts
//! its own Senate, `alice-desk`, and registers it with `acme` as a child,
//! alice its Speaker as a senator of `acme`. Her agents sit in her Senate,
//! never above; the two logs stay apart, and only what one addresses to the
//! other crosses.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use common::*;
use senate_bridge::ipc::{Registration, SayKind};
use senate_core::{Address, CommitteeId, Hub, Kind, Locale, NewEvent, Origin, meta};

fn refund() -> CommitteeId {
    committee()
}

fn alice_desk() -> Address {
    Address::child(name("alice-desk"), refund())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_parent_reaches_alices_desk_and_hears_back_in_its_own_thread() {
    let world = team_above().await;
    let dir = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(
        dir.path(),
        "alice",
        Locale::En,
        &under(&world, "tok-alice", "secretaries = [\"claude\"]\n"),
    );
    desk.start().await;

    // Registered above as a child, alice its Speaker.
    let registration = desk.wait_registration().await;
    assert_eq!(registration.registration, Registration::Registered);
    assert_eq!(registration.parent.unwrap().to_string(), "/acme");
    let children = world.daemon().hub.children();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].name.as_str(), "alice-desk");
    assert_eq!(children[0].speaker.as_str(), "alice");

    // Below: its full path, and alice presiding as a senator of acme.
    let top = desk.senate();
    let (url, token) = (top.url.clone(), top.token.clone());
    let me = me_at(&url, &token).await;
    assert_eq!(me.senate_path().to_string(), "/acme/alice-desk");
    let office = me.office.expect("an office");
    assert_eq!(office.speaker.to_string(), "/acme::senator::alice");
    assert_eq!(office.kind, senate_core::SenatorKind::Person);
    assert_eq!(office.desk, [path("claude")]);

    // claude's session sits in alice's Senate, and works her desk.
    let claude = desk.inbox("s-claude");
    desk.session_start("s-claude", &claude).await.unwrap();
    desk.wait_connected("claude", true).await;
    desk.post("s-claude", "a note that stays below").await;
    let above = speak(
        &world.server_url(),
        "tok-bob",
        &NewEvent::new(
            refund(),
            Kind::Note,
            person("bob"),
            "a note that stays above",
        ),
    )
    .await
    .unwrap();
    assert_eq!(above.committee, refund());
    // Her agents are not seated above.
    let roster = world.hub().roster().await.unwrap();
    assert!(!roster.knows(&path("claude")), "claude sits below only");

    // Bob asks the seat on alice's desk by its qualified name.
    let asked = speak(
        &world.server_url(),
        "tok-bob",
        &NewEvent::new(
            refund(),
            Kind::Question,
            person("bob"),
            "Which retry limit does the refund callback use?",
        )
        .to(Address::Speaker)
        .with_meta(
            meta::INTENDED,
            serde_json::json!("alice-desk::refund-v2::senator::claude"),
        ),
    )
    .await
    .unwrap();
    assert_eq!(asked.to, [alice_desk()]);

    // It lands on alice's desk: a root from the committee to the Speaker,
    // meant for claude, its origin bob's thread above.
    let root = eventually_async("the question on alice's desk", || async {
        desk.events_of(&refund())
            .await
            .into_iter()
            .find(|e| e.body.contains("Which retry limit"))
    })
    .await;
    assert_eq!(root.from, Address::Committee);
    assert_eq!(root.to, [Address::Speaker]);
    assert_eq!(root.thread, None);
    assert_eq!(
        senate_core::locator::intended(&root).unwrap().to_string(),
        "/acme/alice-desk::refund-v2::senator::claude"
    );
    let origin = Origin::of(&root).expect("an origin");
    assert_eq!(origin.locator.to_string(), "/acme::refund-v2::senator::bob");
    assert_eq!(origin.thread, asked.seq);
    let docket = desk.senate().daemon.hub.docket(&refund()).await.unwrap();
    let item = docket
        .items
        .iter()
        .find(|i| i.event.seq == root.seq)
        .expect("on the desk");
    assert_eq!(
        item.intended.as_ref().unwrap().to_string(),
        "/acme/alice-desk::refund-v2::senator::claude"
    );
    // claude, her secretary, is told.
    claude.wait_for("Which retry limit").await;

    // Alice answers there, as herself.
    speak(
        &url,
        &token,
        &NewEvent::new(
            refund(),
            Kind::Answer,
            person("alice"),
            "Three times, see retry.rs",
        )
        .to(Address::Committee)
        .in_thread(root.seq),
    )
    .await
    .unwrap();

    // Back above, in bob's thread, from alice's desk.
    let answer = eventually_async("the answer above", || async {
        world
            .events_of(&refund())
            .await
            .into_iter()
            .find(|e| e.body == "Three times, see retry.rs")
    })
    .await;
    assert_eq!(answer.kind, Kind::Answer);
    assert_eq!(answer.from, alice_desk());
    assert_eq!(answer.from.to_string(), "alice-desk::refund-v2::speaker");
    assert_eq!(answer.thread, Some(asked.seq));
    assert_eq!(answer.to, [person("bob")]);
    assert_eq!(
        answer.meta[meta::ANSWERED_BY],
        serde_json::json!("/acme/alice-desk::refund-v2::senator::alice")
    );

    // Crossing is idempotent: a few more polls store nothing new.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let above = world.events_of(&refund()).await;
    let below = desk.events_of(&refund()).await;
    assert_eq!(
        above
            .iter()
            .filter(|e| e.body == "Three times, see retry.rs")
            .count(),
        1
    );
    assert_eq!(
        below
            .iter()
            .filter(|e| e.body.contains("Which retry limit"))
            .count(),
        1
    );
    // Nothing of either log in the other.
    assert!(above.iter().all(|e| !e.body.contains("stays below")));
    assert!(below.iter().all(|e| !e.body.contains("stays above")));
    assert!(
        above.iter().all(|e| e.from != senator("claude")),
        "claude never writes above"
    );
    desk.stop().await;
    world.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_child_is_refused_unless_its_speaker_is_on_the_roster_and_registers_it() {
    let world = team_above().await;
    let dir = tempfile::tempdir().unwrap();

    // Alice's machine names bob its Speaker, and registers with alice's
    // token: the parent refuses. The machine starts all the same.
    let mut desk = Desk::new(
        dir.path(),
        "alice",
        Locale::En,
        &under(&world, "tok-alice", "speaker = \"/acme::senator::bob\"\n"),
    );
    desk.start().await;
    let registration = desk.wait_registration().await;
    let Registration::Refused { reason } = &registration.registration else {
        panic!("{registration:?}");
    };
    assert!(reason.contains("bob"), "{reason}");
    assert!(world.daemon().hub.children().is_empty());
    let status = desk.machine_status().await;
    let uplink = status.roles.as_ref().unwrap().uplink.as_ref().unwrap();
    assert!(matches!(uplink.registration, Registration::Refused { .. }));
    let view = senate_bridge::status::collect(&desk.paths).await.unwrap();
    let text = senate_bridge::status::render(&view, chrono::Utc::now());
    assert!(
        text.contains(
            "refused to register the child Senate [alice-desk] (Speaker [/acme::senator::bob])"
        ),
        "{text}"
    );
    desk.stop().await;

    // A Speaker not on the parent's roster at all.
    let dir = tempfile::tempdir().unwrap();
    let mut other = Desk::new(
        dir.path(),
        "alice",
        Locale::Zh,
        &under(
            &world,
            "tok-alice",
            "name = \"north-desk\"\nspeaker = \"/acme::senator::dave\"\n",
        ),
    );
    other.start().await;
    let registration = other.wait_registration().await;
    let Registration::Refused { reason } = &registration.registration else {
        panic!("{registration:?}");
    };
    assert!(reason.contains("not on this Senate's roster"), "{reason}");
    let view = senate_bridge::status::collect(&other.paths).await.unwrap();
    let text = senate_bridge::status::render(&view, chrono::Utc::now());
    assert!(
        text.contains("拒绝了子 Senate [north-desk] 的注册（Speaker [/acme::senator::dave]）"),
        "{text}"
    );
    assert!(world.daemon().hub.children().is_empty());
    other.stop().await;
    world.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_name_is_looked_for_nearest_first_and_a_person_above_is_asked_directly() {
    let world = team_above().await;
    // Above: bob runs an agent also named claude on refund-v2, and is on
    // ibot.
    let ibot = CommitteeId::new("ibot").unwrap();
    speak(
        &world.server_url(),
        "tok-bob",
        &NewEvent::new(refund(), Kind::Note, senator("claude"), "claude above"),
    )
    .await
    .unwrap();
    speak(
        &world.server_url(),
        "tok-bob",
        &NewEvent::new(ibot.clone(), Kind::Note, person("bob"), "ibot is here"),
    )
    .await
    .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(
        dir.path(),
        "alice",
        Locale::En,
        &under(&world, "tok-alice", ""),
    );
    desk.start().await;
    assert_eq!(
        desk.wait_registration().await.registration,
        Registration::Registered
    );
    let claude = desk.inbox("s-claude");
    desk.session_start("s-claude", &claude).await.unwrap();
    desk.wait_connected("claude", true).await;
    let socket = desk.paths.socket();

    // refund-v2 is a committee at both levels, and claude sits on both: the
    // machine's own Senate is nearest, and wins.
    let sent = senate_bridge::speak::send(
        &socket,
        refund(),
        SayKind::Question,
        "refund-v2::claude",
        "Nearest first?".into(),
    )
    .await
    .unwrap();
    assert_eq!(sent.posted.to, [senator("claude")]);
    claude.wait_for("Nearest first?").await;
    assert!(
        desk.events_of(&refund())
            .await
            .iter()
            .any(|e| e.body == "Nearest first?" && e.from == person("alice"))
    );
    assert!(
        world
            .events_of(&refund())
            .await
            .iter()
            .all(|e| e.body != "Nearest first?")
    );

    // ibot is only above: alice's question is posted there directly, as
    // her (her token above), by its full name.
    let sent = senate_bridge::speak::send(
        &socket,
        refund(),
        SayKind::Question,
        "/acme::ibot::bob",
        "Is the ibot retry shared?".into(),
    )
    .await
    .unwrap();
    assert_eq!(sent.posted.committee, ibot);
    let asked = world
        .events_of(&ibot)
        .await
        .into_iter()
        .find(|e| e.body == "Is the ibot retry shared?")
        .expect("above, on ibot");
    assert_eq!(asked.from, person("alice"));
    assert_eq!(asked.to, [person("bob")]);
    assert!(
        desk.senate()
            .daemon
            .hub
            .committees()
            .await
            .unwrap()
            .iter()
            .all(|c| c != &ibot),
        "nothing of it below"
    );

    // Names that reach no one are refused, saying why.
    let err = senate_bridge::speak::send(
        &socket,
        refund(),
        SayKind::Question,
        "refund-v2::nobody",
        "Anyone?".into(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("[refund-v2::senator::nobody] cannot be reached"),
        "{err}"
    );
    assert!(err.contains("no senator nobody"), "{err}");
    let err = senate_bridge::speak::send(
        &socket,
        refund(),
        SayKind::Question,
        "nowhere::bob",
        "Anyone?".into(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        err.contains(
            "[nowhere::senator::bob] is not in this Senate, nor in any Senate above it knows of"
        ),
        "{err}"
    );
    desk.stop().await;
    world.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_desk_forwards_an_agents_question_above_and_the_answer_comes_back_down() {
    let world = team_above().await;
    let ibot = CommitteeId::new("ibot").unwrap();
    speak(
        &world.server_url(),
        "tok-bob",
        &NewEvent::new(ibot.clone(), Kind::Note, person("bob"), "ibot is here"),
    )
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(
        dir.path(),
        "alice",
        Locale::En,
        &under(&world, "tok-alice", "secretaries = [\"claude\"]\n"),
    );
    desk.start().await;
    assert_eq!(
        desk.wait_registration().await.registration,
        Registration::Registered
    );
    let claude = desk.inbox("s-claude");
    desk.session_start("s-claude", &claude).await.unwrap();
    let codex = desk.inbox("s-codex");
    desk.session_start_as("s-codex", &codex, "codex")
        .await
        .unwrap();
    desk.wait_connected("claude", true).await;
    desk.wait_connected("codex", true).await;

    // codex, an agent, writes to a seat above: it is put to alice's desk
    // on the way (the boundary rule), meant for bob on ibot above.
    let sent: senate_bridge::ipc::SendReply = desk
        .ipc(senate_bridge::ipc::Request::Send {
            session: Laptop::session("s-codex"),
            kind: SayKind::Question,
            to: Vec::new(),
            at: Some("/acme::ibot::senator::bob".parse().unwrap()),
            body: "Does ibot retry refunds too?".into(),
            refs: None,
            id: None,
        })
        .await;
    assert_eq!(sent.posted.to, [Address::Speaker]);
    assert_eq!(
        sent.intended.as_ref().unwrap().to_string(),
        "/acme::ibot::senator::bob"
    );
    claude.wait_for("Does ibot retry refunds too?").await;

    // claude, working the desk, forwards it above.
    let forwarded: senate_bridge::ipc::Forwarded = desk
        .ipc(senate_bridge::ipc::Request::Forward {
            session: Laptop::session("s-claude"),
            committee: refund(),
            seq: sent.posted.seq,
            id: None,
        })
        .await;
    assert_eq!(forwarded.posted.committee, ibot);
    let up = world
        .events_of(&ibot)
        .await
        .into_iter()
        .find(|e| e.body == "Does ibot retry refunds too?")
        .expect("above");
    assert_eq!(up.from, alice_desk());
    assert_eq!(up.to, [person("bob")]);
    assert_eq!(up.meta[meta::ACTED_BY], serde_json::json!("claude"));
    let origin = Origin::of(&up).unwrap();
    assert_eq!(
        origin.locator.to_string(),
        "/acme/alice-desk::refund-v2::senator::codex"
    );
    assert_eq!(origin.thread, sent.posted.seq);

    // Bob answers above; it comes down into codex's thread.
    speak(
        &world.server_url(),
        "tok-bob",
        &NewEvent::new(ibot.clone(), Kind::Answer, person("bob"), "Yes, twice.")
            .to(alice_desk())
            .in_thread(up.seq),
    )
    .await
    .unwrap();
    let down = eventually_async("the answer below", || async {
        desk.events_of(&refund())
            .await
            .into_iter()
            .find(|e| e.body == "Yes, twice.")
    })
    .await;
    assert_eq!(down.kind, Kind::Answer);
    assert_eq!(down.from, Address::Speaker);
    assert_eq!(down.to, [senator("codex")]);
    assert_eq!(down.thread, Some(sent.posted.seq));
    assert_eq!(
        down.meta[meta::ANSWERED_BY],
        serde_json::json!("/acme::ibot::senator::bob")
    );
    codex.wait_for("Yes, twice.").await;
    // It does not go back up.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert_eq!(
        world
            .events_of(&ibot)
            .await
            .iter()
            .filter(|e| e.body == "Yes, twice.")
            .count(),
        1
    );
    desk.stop().await;
    world.stop().await;
}
