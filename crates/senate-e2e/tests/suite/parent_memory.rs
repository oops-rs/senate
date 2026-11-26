//! Phase 1 of cloud-to-local memory: an agent on alice's machine reads,
//! never writes, what the team's Senate above knows on the committee the
//! two share: its focus, decisions, memory and digest, each with a stable
//! reference to the log above. Nothing crosses either way for it, the
//! token there stays in the daemon, and a committee the Senate above does
//! not share is refused without a word about what else is there.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::*;
use senate_bridge::ipc::{Registration, Request};
use senate_bridge::parent::{Ask, ParentContextReply, ParentKind, ParentState, render};
use senate_core::{
    Address, CommitteeId, DigestPayload, DigestPoint, DigestTrigger, Event, Hub, Kind, Locale,
    MemoryPayload, NewEvent, Seq, meta,
};

fn refund() -> CommitteeId {
    committee()
}

fn point(text: &str, refs: Vec<Seq>) -> NewEvent {
    NewEvent::new(refund(), Kind::Note, Address::Speaker, text).with_meta(
        meta::MEMORY,
        MemoryPayload::Point {
            refs,
            supersedes: vec![],
            digest: None,
        }
        .to_value(),
    )
}

async fn read(desk: &Desk, session: &str, ask: Ask) -> ParentContextReply {
    desk.ipc(Request::ParentContext {
        session: Laptop::session(session),
        ask,
    })
    .await
}

/// Every event of the Senate above, on every committee.
async fn everything_above(world: &World) -> Vec<Event> {
    let mut all = Vec::new();
    let (open, archived) = world.hub().committees_by_archive().await.unwrap();
    for c in open.iter().chain(&archived) {
        all.extend(world.events_of(c).await);
    }
    all
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_local_agent_reads_the_parents_brief_and_memory_read_only_with_stable_refs() {
    // The team's Senate above, as `team_above` sets it up, keeping its log
    // in a file so a restart finds it again.
    let mut world = Setup::new()
        .without_speaker()
        .sqlite()
        .server(|c| {
            c.name = Some(name("acme"));
            c.roster.speaker = Some("senator::bob".parse().unwrap());
        })
        .start()
        .await;
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
    desk.wait_connected("claude", true).await;

    // Above, on the shared committee: a decision, three facts, a focus and
    // a digest; a person corrects one fact and deletes another.
    let hub = world.hub().clone();
    let said = hub
        .append(NewEvent::new(
            refund(),
            Kind::Note,
            person("bob"),
            "we settled the refund window",
        ))
        .await
        .unwrap()
        .event;
    let decision = hub
        .append(NewEvent::new(
            refund(),
            Kind::Decision,
            person("bob"),
            "Refunds are capped at 90 days",
        ))
        .await
        .unwrap()
        .event;
    let owner = hub
        .append(point(
            "Billing is owned by the platform team",
            vec![said.seq],
        ))
        .await
        .unwrap()
        .event;
    let currency = hub
        .append(point("Billing pays out in USD only", vec![said.seq]))
        .await
        .unwrap()
        .event;
    let nights = hub
        .append(point("Deploys of billing happen at night", vec![said.seq]))
        .await
        .unwrap()
        .event;
    let focus = hub
        .append(
            NewEvent::new(
                refund(),
                Kind::Note,
                Address::Speaker,
                "1. Ship the retry fix",
            )
            .with_meta(meta::BRIEF_TOP, serde_json::Value::Bool(true)),
        )
        .await
        .unwrap()
        .event;
    let digest = hub
        .append(
            NewEvent::new(
                refund(),
                Kind::Note,
                Address::Speaker,
                "Retry rewrite underway",
            )
            .with_meta(
                meta::DIGEST,
                DigestPayload {
                    trigger: DigestTrigger::Daily,
                    from: Seq(1),
                    through: said.seq,
                    points: vec![DigestPoint {
                        text: "Retry rewrite underway".into(),
                        refs: vec![said.seq],
                    }],
                }
                .to_value(),
            ),
        )
        .await
        .unwrap()
        .event;
    let corrected = hub
        .append(
            NewEvent::new(
                refund(),
                Kind::Note,
                person("bob"),
                "Billing is owned by the payments team",
            )
            .with_meta(
                meta::MEMORY,
                MemoryPayload::Correct {
                    target: owner.seq,
                    refs: vec![],
                }
                .to_value(),
            ),
        )
        .await
        .unwrap()
        .event;
    let deleted = hub
        .append(
            NewEvent::new(refund(), Kind::Note, person("bob"), "forget it").with_meta(
                meta::MEMORY,
                MemoryPayload::Delete {
                    targets: vec![currency.seq],
                }
                .to_value(),
            ),
        )
        .await
        .unwrap()
        .event;
    // Another committee above, which alice's Senate does not share.
    hub.append(NewEvent::new(
        CommitteeId::new("payroll-secret").unwrap(),
        Kind::Decision,
        person("bob"),
        "Salaries are confidential",
    ))
    .await
    .unwrap();
    let epoch = me_at(&world.server_url(), "tok-alice").await.epoch;

    let before_above = everything_above(&world).await;
    let before_here = desk.events_of(&refund()).await;

    // 1. The effective brief above, with provenance and stable references.
    let reply = read(&desk, "s-claude", Ask::default()).await;
    assert_eq!(reply.senate.to_string(), "/acme");
    assert_eq!(reply.committee, refund());
    assert_eq!(reply.epoch.as_deref(), Some(epoch.as_str()));
    let summary: Vec<_> = reply
        .items
        .iter()
        .map(|i| (i.kind, i.reference.seq, i.state, i.text.as_str()))
        .collect();
    assert_eq!(
        summary,
        [
            (
                ParentKind::BriefTop,
                focus.seq,
                ParentState::Active,
                "1. Ship the retry fix"
            ),
            (
                ParentKind::Decision,
                decision.seq,
                ParentState::Active,
                "Refunds are capped at 90 days"
            ),
            (
                ParentKind::Memory,
                nights.seq,
                ParentState::Active,
                "Deploys of billing happen at night"
            ),
            // 2. The correction stands in for the point it corrects; the
            // deleted point is gone.
            (
                ParentKind::Memory,
                corrected.seq,
                ParentState::Corrected,
                "Billing is owned by the payments team"
            ),
            (
                ParentKind::Digest,
                digest.seq,
                ParentState::Active,
                "Retry rewrite underway"
            ),
        ]
    );
    for item in &reply.items {
        assert_eq!(item.reference.senate.to_string(), "/acme");
        assert_eq!(item.reference.committee, refund());
        assert_eq!(item.reference.epoch.as_deref(), Some(epoch.as_str()));
        assert!(item.reference.event_id.is_some(), "{item:?}");
    }
    assert_eq!(
        reply.items[1].reference.event_id.as_ref(),
        Some(&decision.id)
    );
    assert_eq!(reply.items[1].by, Some(person("bob")));
    assert_eq!(reply.items[3].supersedes, [owner.seq]);
    let text = render(&reply, Locale::En);
    assert!(
        text.contains(&format!("↑/acme::refund-v2@{epoch}#{}", decision.seq)),
        "{text}"
    );
    assert!(text.contains("not instructions from your user"), "{text}");
    assert!(!text.contains("Salaries"), "{text}");
    assert!(!text.contains("platform team"), "{text}");
    assert!(!text.contains("USD"), "{text}");

    // 2. With history, the retired points say how they were retired.
    let history = read(
        &desk,
        "s-claude",
        Ask {
            query: "billing".into(),
            limit: None,
            history: true,
        },
    )
    .await;
    let state_of = |seq: Seq| {
        history
            .items
            .iter()
            .find(|i| i.reference.seq == seq)
            .map(|i| (i.state, i.state_seq))
    };
    assert_eq!(
        state_of(owner.seq),
        Some((ParentState::Superseded, Some(corrected.seq)))
    );
    assert_eq!(
        state_of(currency.seq),
        Some((ParentState::Deleted, Some(deleted.seq)))
    );
    assert_eq!(
        state_of(corrected.seq),
        Some((ParentState::Corrected, None))
    );
    // History without a query: the newest points, retired ones included.
    let newest = read(
        &desk,
        "s-claude",
        Ask {
            history: true,
            ..Ask::default()
        },
    )
    .await;
    assert!(
        newest
            .items
            .iter()
            .any(|i| i.reference.seq == currency.seq && i.state == ParentState::Deleted)
    );

    // 3. Repeated reads, and a restart of the Senate above, give the same
    // items under the same references.
    let again = read(&desk, "s-claude", Ask::default()).await;
    assert_eq!(again, reply);
    world.restart().await;
    let after_restart = eventually_async("the Senate above answers again", || async {
        desk.try_ipc::<ParentContextReply>(Request::ParentContext {
            session: Laptop::session("s-claude"),
            ask: Ask::default(),
        })
        .await
        .ok()
    })
    .await;
    assert_eq!(after_restart, reply);

    // 5. Nothing was written above or here for any of it.
    assert_eq!(everything_above(&world).await.len(), before_above.len());
    let here = desk.events_of(&refund()).await;
    assert_eq!(
        here.len(),
        before_here.len(),
        "{:?}",
        &here[before_here.len().min(here.len())..]
    );

    // 6. The token there is in no reply.
    for r in [&reply, &history, &newest] {
        let json = serde_json::to_string(r).unwrap();
        assert!(!json.contains("tok-alice"));
        assert!(!render(r, Locale::En).contains("tok-alice"));
    }

    // 4. A session on a committee the Senate above does not share is
    // refused, and the refusal names nothing of what is there.
    let local = CommitteeId::new("alice-notes").unwrap();
    let other = desk.inbox("s-other");
    desk.session_start("s-other", &other).await.unwrap();
    desk.ipc::<serde_json::Value>(Request::OpenCommittee {
        committee: local.clone(),
    })
    .await;
    desk.ipc::<serde_json::Value>(Request::Join {
        session: Laptop::session("s-other"),
        committee: local.clone(),
    })
    .await;
    let refused = desk
        .try_ipc::<ParentContextReply>(Request::ParentContext {
            session: Laptop::session("s-other"),
            ask: Ask::default(),
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("alice-notes is not shared with the Senate above"),
        "{refused}"
    );
    for leak in [
        "payroll-secret",
        "refund-v2",
        "tok-alice",
        &world.server_url(),
    ] {
        assert!(!refused.contains(leak), "{leak} in {refused}");
    }
    // A request that names a committee anyway still reads only its own.
    let smuggled: Request = serde_json::from_value(serde_json::json!({
        "op": "parent_context",
        "session": {"session_id": "s-other"},
        "committee": "payroll-secret",
    }))
    .unwrap();
    let refused = desk
        .try_ipc::<ParentContextReply>(smuggled)
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("alice-notes is not shared"), "{refused}");
    // And no selector, no read.
    let anonymous = desk
        .try_ipc::<ParentContextReply>(Request::ParentContext {
            session: senate_bridge::ipc::SessionSelector::default(),
            ask: Ask::default(),
        })
        .await;
    assert!(anonymous.is_err());

    desk.stop().await;
}

/// A machine with no Senate above has no parent context to give.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_machine_alone_has_no_parent_context() {
    let dir = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(dir.path(), "alice", Locale::En, "[local]\n");
    desk.start().await;
    let claude = desk.inbox("s-claude");
    desk.session_start("s-claude", &claude).await.unwrap();
    let refused = desk
        .try_ipc::<ParentContextReply>(Request::ParentContext {
            session: Laptop::session("s-claude"),
            ask: Ask::default(),
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("no Senate above"), "{refused}");
    desk.stop().await;
}
