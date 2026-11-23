#![allow(clippy::unwrap_used)]
#[path = "support/common.rs"]
mod common;

use common::{Harness, path};
use senate_bridge::{
    agent::AgentSource,
    ipc::{self, DeskDecision, DeskItemDetail, DeskOutcome, Request},
    session::Runtime,
    state::SessionUpsert,
};
use senate_core::{Address, CommitteeId, EventId, Kind, NewEvent, meta};
use serde_json::json;
use std::{path::Path, time::Duration};

const WAIT: Duration = Duration::from_secs(6);
const SOCKET_ENV: &str = "SENATE_DESK_AGENT_TEST_SOCKET";
const GO_ENV: &str = "SENATE_DESK_AGENT_TEST_READY";

async fn call<T: serde::de::DeserializeOwned>(
    socket: &Path,
    request: &Request,
) -> Result<T, ipc::ClientError> {
    ipc::request(socket, request, WAIT, WAIT).await
}

#[tokio::test]
async fn the_original_request_gets_one_person_attributed_delivery_receipt_on_retry() {
    let mut h = Harness::new().await;
    h.server
        .state
        .inner
        .lock()
        .unwrap()
        .roster
        .name_speaker(Some("senator::alice".parse().unwrap()))
        .unwrap();
    let make_item = || {
        h.server.append(
            NewEvent::new(
                common::committee(),
                Kind::Request,
                Address::senior(&common::name("bob")),
                "A full request",
            )
            .to(Address::Speaker)
            .with_meta(meta::INTENDED, json!("other::senator::alice")),
        )
    };
    let original = make_item();
    let rejected = make_item();
    h.start().await;
    let socket = h.socket();
    let detail: DeskItemDetail = call(
        &socket,
        &Request::DeskItem {
            committee: original.committee.clone(),
            seq: original.seq,
        },
    )
    .await
    .unwrap();
    assert_eq!(detail.item.body, "A full request");
    assert_eq!(detail.events.len(), 1);
    assert_eq!(detail.destination.as_deref(), Some("other::senator::alice"));
    assert_eq!(detail.blocked, None);

    let stale = Request::DeskDecide {
        committee: original.committee.clone(),
        seq: original.seq,
        item_id: EventId::new(),
        id: EventId::new(),
        decision: DeskDecision::Forward,
    };
    assert!(
        call::<DeskOutcome>(&socket, &stale)
            .await
            .unwrap_err()
            .to_string()
            .contains("has changed")
    );
    assert!(h.server.events("other").is_empty());

    let forward = Request::DeskDecide {
        committee: original.committee.clone(),
        seq: original.seq,
        item_id: original.id.clone(),
        id: EventId::new(),
        decision: DeskDecision::Forward,
    };
    // The destination is already durable when the audit append fails.
    // Retrying the same decision must reuse its original delivery ID.
    h.server
        .state
        .inner
        .lock()
        .unwrap()
        .reject_append_containing = Some("approved forwarding".into());
    let failed_audit = call::<DeskOutcome>(&socket, &forward)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        failed_audit.contains("Delivered to other::senator::alice #1"),
        "{failed_audit}"
    );
    assert_eq!(h.server.events("other").len(), 1);
    h.server
        .state
        .inner
        .lock()
        .unwrap()
        .reject_append_containing = None;
    let receipt: DeskOutcome = call(&socket, &forward).await.unwrap();
    let retry: DeskOutcome = call(&socket, &forward).await.unwrap();
    assert_eq!(retry, receipt);
    let DeskOutcome::Delivered { seq, committee, .. } = receipt else {
        panic!("delivery receipt expected")
    };
    let destination = h.server.events(committee.as_str());
    assert_eq!(destination.len(), 1);
    assert_eq!(destination[0].seq, seq);
    assert_eq!(
        senate_core::Origin::of(&destination[0]).unwrap().thread,
        original.seq
    );
    assert_eq!(destination[0].meta[meta::DESK_PERSON], "alice");
    let audit: Vec<_> = h
        .server
        .events(original.committee.as_str())
        .into_iter()
        .filter(|event| event.thread == Some(original.seq))
        .collect();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].meta[meta::ACTED_BY], "alice");
    assert_eq!(audit[0].meta[meta::DESK_OUTCOME]["seq"], seq.0);

    let refused = Request::DeskDecide {
        committee: original.committee.clone(),
        seq: rejected.seq,
        item_id: rejected.id.clone(),
        id: EventId::new(),
        decision: DeskDecision::Refuse {
            reason: "Outside policy".into(),
        },
    };
    let receipt: DeskOutcome = call(&socket, &refused).await.unwrap();
    assert!(matches!(receipt, DeskOutcome::Refused { .. }));
    assert_eq!(
        call::<DeskOutcome>(&socket, &refused).await.unwrap(),
        receipt
    );
    assert_eq!(h.server.events("other").len(), 1);
    h.stop().await;
}

#[tokio::test]
async fn an_agent_peer_is_refused_even_when_its_person_is_speaker() {
    if let (Some(socket), Some(ready)) = (std::env::var_os(SOCKET_ENV), std::env::var_os(GO_ENV)) {
        tokio::time::timeout(WAIT, async {
            while !Path::new(&ready).exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let err = call::<DeskOutcome>(
            Path::new(&socket),
            &Request::DeskDecide {
                committee: CommitteeId::new("refund-v2").unwrap(),
                seq: senate_core::Seq(1),
                item_id: EventId::new(),
                id: EventId::new(),
                decision: DeskDecision::Forward,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ipc::ClientError::Remote(error)
            if error.code == ipc::codes::NOT_A_PERSON && error.message.contains("senator::claude")));
        return;
    }
    let mut h = Harness::new().await;
    h.server
        .state
        .inner
        .lock()
        .unwrap()
        .roster
        .name_speaker(Some("senator::alice".parse().unwrap()))
        .unwrap();
    h.start().await;
    let ready = h.dir.path().join("peer-ready");
    let mut agent = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "an_agent_peer_is_refused_even_when_its_person_is_speaker",
            "--nocapture",
        ])
        .env(SOCKET_ENV, h.socket())
        .env(GO_ENV, &ready)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    h.store()
        .upsert_session(&SessionUpsert {
            session_id: "desk-guard-peer".into(),
            cwd: h.dir.path().into(),
            git_root: None,
            branch: None,
            inferred_committee: None,
            socket: None,
            token: None,
            claude_pid: agent.id(),
            transcript_path: None,
            ancestor_pids: Vec::new(),
            runtime: Runtime::Claude,
            agent: path("claude"),
            agent_source: AgentSource::Session,
        })
        .unwrap();
    std::fs::write(ready, "").unwrap();
    assert!(
        tokio::time::timeout(WAIT, agent.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    h.stop().await;
}
