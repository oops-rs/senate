//! The person-facing reads on a real bridge socket and an isolated log.
#![allow(clippy::unwrap_used)]

use crate::common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::{Harness, committee, note, question, senator};
use senate_bridge::ipc::{self, ActivityBatch, CommitteeThreadPage, Reply, Request};
use senate_core::{Address, Kind, NewEvent, Origin, Seq, meta, task::StatusPayload};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

const TIMEOUT: Duration = Duration::from_secs(5);

async fn watch(harness: &Harness, request: Request) -> BufReader<UnixStream> {
    let mut stream = UnixStream::connect(harness.socket()).await.unwrap();
    let mut line = serde_json::to_vec(&request).unwrap();
    line.push(b'\n');
    stream.write_all(&line).await.unwrap();
    BufReader::new(stream)
}

async fn read(stream: &mut BufReader<UnixStream>) -> String {
    let mut line = String::new();
    tokio::time::timeout(TIMEOUT, stream.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    line
}

async fn batch(stream: &mut BufReader<UnixStream>) -> ActivityBatch {
    serde_json::from_str(&read(stream).await).unwrap()
}

fn activity(after: Option<Seq>) -> Request {
    Request::ActivityWatch {
        committees: Some(vec![committee()]),
        after: after
            .map(|after| BTreeMap::from([(committee(), after)]))
            .unwrap_or_default(),
    }
}

#[tokio::test]
async fn activity_start_at_head_shared_feed_replay_no_duplicates_and_coalescing() {
    let mut h = Harness::new().await;
    h.start().await;
    let root = h
        .server
        .append(question(senator("bob"), senator("alice"), "Check this?"));
    assert_eq!(h.server.active_streams("refund-v2"), 0);

    let mut quiet = watch(&h, activity(None)).await;
    let ready = batch(&mut quiet).await;
    assert!(ready.events.is_empty());
    assert_eq!(ready.cursors[&committee()], root.seq);
    let mut replay = watch(&h, activity(Some(Seq::ZERO))).await;
    assert!(batch(&mut replay).await.events.is_empty());
    assert_eq!(batch(&mut replay).await.events[0].seq, root.seq);

    h.wait_stream("refund-v2").await;
    assert_eq!(h.server.active_streams("refund-v2"), 1);
    assert_eq!(h.server.active_streams("other"), 0);
    h.server.append(NewEvent::new(
        "other".parse().unwrap(),
        Kind::Note,
        senator("bob"),
        "other committee",
    ));
    let first = h.server.append(
        note(
            senator("bob"),
            &format!("{}\nnot the preview", "图".repeat(100)),
        )
        .in_thread(root.seq),
    );
    let second = h
        .server
        .append(note(senator("alice"), "second").in_thread(root.seq));
    for stream in [&mut quiet, &mut replay] {
        let updates = batch(stream).await;
        assert!(!updates.reset);
        assert_eq!(
            updates.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [first.seq, second.seq]
        );
        assert_eq!(updates.events[0].line, "图".repeat(80));
        assert_eq!(updates.events[1].thread, Some(root.seq));
    }

    // Lose one SSE update: the next seq forces bounded canonical replay.
    let missing = h
        .server
        .append_silently(note(senator("alice"), "gap").in_thread(root.seq));
    let next = h
        .server
        .append(note(senator("bob"), "next").in_thread(root.seq));
    assert_eq!(batch(&mut quiet).await.events[0].seq, missing.seq);
    assert_eq!(batch(&mut quiet).await.events[0].seq, next.seq);

    drop(quiet);
    drop(replay);
    common::eventually("last watcher releases passive feed", TIMEOUT, || {
        (h.server.active_streams("refund-v2") == 0).then_some(())
    })
    .await;
}

#[tokio::test]
async fn activity_reuses_a_running_agent_feed_and_refuses_a_foreign_committee() {
    let mut h = Harness::with_committees(&["refund-v2"]).await;
    h.start().await;
    let mut denied = watch(
        &h,
        Request::ActivityWatch {
            committees: Some(vec!["foreign".parse().unwrap()]),
            after: BTreeMap::new(),
        },
    )
    .await;
    let reply: Reply = serde_json::from_str(&read(&mut denied).await).unwrap();
    assert!(!reply.ok);
    assert_eq!(h.server.active_streams("foreign"), 0);

    let cwd = h.workdir("activity-reuse", Some("refund-v2"));
    h.hook(
        senate_bridge::hook::HookKind::SessionStart,
        "activity-reuse",
        &cwd,
        None,
    )
    .await;
    h.wait_stream_of("refund-v2", "claude").await;
    let mut reader = watch(&h, activity(None)).await;
    assert!(batch(&mut reader).await.events.is_empty());
    let event = h.server.append(note(senator("alice"), "existing stream"));
    assert_eq!(batch(&mut reader).await.events[0].seq, event.seq);
    assert_eq!(h.server.active_streams("refund-v2"), 1);
    let mut ahead = watch(&h, activity(Some(Seq(9999)))).await;
    assert!(batch(&mut ahead).await.events.is_empty());
    assert!(batch(&mut ahead).await.reset);
}

#[tokio::test]
async fn committee_threads_list_both_copies_and_keep_inbox_filtering_separate() {
    let mut h = Harness::new().await;
    h.start().await;
    let other = "other".parse().unwrap();
    let local = h.server.append(
        note(senator("alice"), "Cross-committee discussion")
            .with_meta(meta::INTENDED, "other::chair".into())
            .with_meta(
                meta::CHAIR_DISCUSSION,
                "/senate::other::senator::codex".into(),
            ),
    );
    let origin = Origin {
        locator: "/senate::refund-v2::senator::alice".parse().unwrap(),
        thread: local.seq,
    };
    let copy = h.server.append(
        NewEvent::new(
            other,
            Kind::Note,
            Address::Committee,
            "Cross-committee discussion",
        )
        .on_behalf_of(senator("alice"))
        .to(senator("codex"))
        .with_meta(meta::CHAIR_DISCUSSION, true.into())
        .with_meta(meta::ORIGIN, origin.value()),
    );
    let reply = h
        .server
        .append(note(senator("bob"), "Reply").in_thread(local.seq));
    let archived = h
        .server
        .append(question(senator("bob"), senator("alice"), "Archived"));
    h.server.append(
        NewEvent::new(committee(), Kind::Status, senator("alice"), "Closed")
            .in_thread(archived.seq)
            .with_meta(
                meta::STATUS,
                StatusPayload::new(senate_core::StatusState::Archived).value(),
            ),
    );

    let page = |committee, include_archived, cursor| {
        let op = Request::CommitteeThreads {
            committee,
            include_archived,
            limit: Some(1),
            cursor,
        };
        let socket = h.socket();
        async move {
            ipc::request::<CommitteeThreadPage>(&socket, &op, TIMEOUT, TIMEOUT)
                .await
                .unwrap()
        }
    };
    let local_rows = page(committee(), false, None).await;
    assert_eq!(local_rows.rows.len(), 1);
    let row = &local_rows.rows[0];
    assert_eq!(row.root, local.seq);
    assert_eq!(row.last_seq, reply.seq);
    assert_eq!(row.last_author.as_deref(), Some("senator::bob"));
    assert_eq!(row.message_count, Some(2));
    assert_eq!(
        row.other_committee
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("other")
    );
    let other_rows = page("other".parse().unwrap(), false, None).await;
    assert_eq!(other_rows.rows[0].root, copy.seq);
    assert_eq!(other_rows.rows[0].other_committee, Some(committee()));
    assert_eq!(other_rows.rows[0].opener, origin.locator.to_string());

    let all = page(committee(), true, None).await;
    assert_eq!(all.rows[0].root, archived.seq);
    assert_eq!(
        page(committee(), true, all.next_cursor).await.rows[0].root,
        local.seq
    );
}

#[tokio::test]
async fn an_existing_watch_hands_off_to_a_newly_seated_stream() {
    let mut h = Harness::with_committees(&["refund-v2"]).await;
    h.start().await;
    let mut reader = watch(&h, activity(None)).await;
    assert!(batch(&mut reader).await.events.is_empty());
    h.wait_stream("refund-v2").await;
    assert_eq!(h.server.active_streams_of("refund-v2", "alice"), 1);

    let cwd = h.workdir("activity-handoff", Some("refund-v2"));
    h.hook(
        senate_bridge::hook::HookKind::SessionStart,
        "activity-handoff",
        &cwd,
        None,
    )
    .await;
    h.wait_stream_of("refund-v2", "claude").await;
    let first = h.server.append(note(senator("bob"), "Switch feeds"));
    assert_eq!(batch(&mut reader).await.events[0].seq, first.seq);
    common::eventually(
        "the seated feed replaces the passive stream",
        TIMEOUT,
        || (h.server.active_streams("refund-v2") == 1).then_some(()),
    )
    .await;
    let second = h
        .server
        .append(note(senator("alice"), "Now only the seated stream"));
    assert_eq!(batch(&mut reader).await.events[0].seq, second.seq);
    let member = senate_core::CommitteeSeatChange {
        senator: "reviewer".parse().unwrap(),
        action: senate_core::CommitteeSeatAction::Join,
        by: "alice".parse().unwrap(),
    };
    let joined = h
        .server
        .append(member.event(committee(), senate_core::EventId::new()));
    let update = batch(&mut reader).await;
    assert_eq!(update.events[0].seq, joined.seq);
    assert_eq!(update.events[0].kind, "joined");
    assert_eq!(update.events[0].from, "senator::reviewer");
}
