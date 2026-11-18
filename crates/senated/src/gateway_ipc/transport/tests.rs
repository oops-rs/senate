#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::os::unix::fs::PermissionsExt;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

type Reader = BufReader<OwnedReadHalf>;

async fn next(reader: &mut Reader) -> Value {
    timeout(
        Duration::from_secs(3),
        framing::read(reader, &mut Vec::new()),
    )
    .await
    .unwrap()
    .unwrap()
}

fn hello() -> Value {
    json!({"type":"hello","version":1,"platform":"chat","token":"gateway-token","capabilities":["adopt_group","release_group","post"]})
}

async fn attach<F, Fut>(
    surface: Arc<RemoteSurface>,
    resume: impl Future<Output = Result<()>> + Send + 'static,
    handler: F,
) -> (Reader, OwnedWriteHalf, tokio::task::JoinHandle<Result<()>>)
where
    F: Fn(Inbound) -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    let (server, adapter) = UnixStream::pair().unwrap();
    let task = tokio::spawn(session(server, "gateway-token", surface, resume, handler));
    let (read, mut write) = adapter.into_split();
    let mut read = BufReader::new(read);
    send(&mut write, hello()).await.unwrap();
    assert_eq!(next(&mut read).await, json!({"type":"ready","version":1}));
    (read, write, task)
}

#[tokio::test]
async fn a_reply_and_an_adoption_can_complete_while_the_inbound_waits() {
    let surface = RemoteSurface::new("chat".into());
    let outgoing = surface.clone();
    let (mut read, mut write, session) = attach(surface.clone(), async { Ok(()) }, move |msg| {
        let surface = outgoing.clone();
        async move {
            let adopted = surface.adopt_group(&msg.message_ref).await?;
            assert_eq!(adopted.group_id, msg.group_id);
            let posted = surface
                .post(
                    PostTarget::Reply {
                        message_ref: msg.message_ref,
                        in_thread: true,
                    },
                    "已绑定",
                    "stable-id",
                )
                .await?;
            assert!(posted.duplicate);
            assert_eq!(posted.message_ref, "first-ref");
            Ok(())
        }
    })
    .await;
    send(&mut write, json!({
        "type":"inbound","id":"i7","message":{"group_id":"g","sender":"u","message_ref":"m","addressing":"mention","mentions":[],"text":"/bind tests"}
    })).await.unwrap();
    let adopt = next(&mut read).await;
    assert_eq!(adopt["op"], "adopt_group");
    // Independent obligations can return in a different order.
    let release = tokio::spawn(async move { surface.release_group("old").await });
    let second = next(&mut read).await;
    assert_eq!(second["op"], "release_group");
    send(
        &mut write,
        json!({"type":"result","id":second["id"],"value":true}),
    )
    .await
    .unwrap();
    assert!(release.await.unwrap().unwrap());
    send(
        &mut write,
        json!({"type":"result","id":adopt["id"],"value":{"group_id":"g","already_granted":true}}),
    )
    .await
    .unwrap();
    let post = next(&mut read).await;
    assert_eq!(
        post["args"],
        json!({"target":{"kind":"reply","message_ref":"m","in_thread":true},"markdown":"已绑定","operation_id":"stable-id"})
    );
    send(&mut write, json!({"type":"result","id":post["id"],"value":{"message_ref":"first-ref","thread_ref":"t","duplicate":true}})).await.unwrap();
    assert_eq!(next(&mut read).await, json!({"type":"ack","id":"i7"}));
    drop((read, write));
    assert!(session.await.unwrap().is_err());
}

#[tokio::test]
async fn a_rejected_input_does_not_block_the_next_input() {
    use crate::gateway_ipc::bind::RejectedInput;
    use tokio::io::AsyncReadExt;

    let (mut reader, mut writer, task) = attach(
        RemoteSurface::new("chat".into()),
        async { Ok(()) },
        |input| async move {
            match input.message_ref.as_str() {
                "conflict" => Err(RejectedInput("conflicting replay").into()),
                "unavailable" => Err(anyhow!("store temporarily unavailable")),
                _ => Ok(()),
            }
        },
    )
    .await;
    let inbound = |id: &str| {
        json!({
            "type":"inbound","id":id,
            "message":{"group_id":"g","sender":"u","message_ref":id,"addressing":"ambient","mentions":[],"text":"hello"}
        })
    };
    send(&mut writer, inbound("conflict")).await.unwrap();
    assert_eq!(
        next(&mut reader).await,
        json!({"type":"ack","id":"conflict","error":{"kind":"rejected","detail":"conflicting replay"}})
    );
    send(&mut writer, inbound("valid")).await.unwrap();
    assert_eq!(next(&mut reader).await, json!({"type":"ack","id":"valid"}));

    send(&mut writer, inbound("unavailable")).await.unwrap();
    assert!(
        timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    let mut tail = Vec::new();
    reader.read_to_end(&mut tail).await.unwrap();
    assert!(
        tail.is_empty(),
        "transient failure must leave the input unacknowledged"
    );
}

#[tokio::test]
async fn disconnect_releases_calls_and_preserves_the_error_class_after_reconnect() {
    let surface = RemoteSurface::new("chat".into());
    let (mut read, write, session) =
        attach(surface.clone(), async { Ok(()) }, |_| async { Ok(()) }).await;
    let caller = surface.clone();
    let pending = tokio::spawn(async move { caller.adopt_group("m").await });
    let _ = next(&mut read).await;
    drop((read, write));
    assert!(session.await.unwrap().is_err());
    assert!(matches!(
        pending.await.unwrap(),
        Err(SurfaceError::Unavailable(_))
    ));

    let (mut read, mut write, task) =
        attach(surface.clone(), async { Ok(()) }, |_| async { Ok(()) }).await;
    for kind in ["unavailable", "unsupported", "rejected"] {
        let caller = surface.clone();
        let pending = tokio::spawn(async move { caller.release_group("g").await });
        let call = next(&mut read).await;
        send(
            &mut write,
            json!({"type":"result","id":call["id"],"error":{"kind":kind,"detail":"reason"}}),
        )
        .await
        .unwrap();
        let err = pending.await.unwrap().unwrap_err();
        assert!(matches!(
            (kind, err),
            ("unavailable", SurfaceError::Unavailable(_))
                | ("unsupported", SurfaceError::Unsupported(_))
                | ("rejected", SurfaceError::Rejected(_))
        ));
    }
    send(
        &mut write,
        json!({"type":"result","id":"unrelated","value":true}),
    )
    .await
    .unwrap();
    assert!(task.await.unwrap().is_err());
    assert!(surface.peer().is_none());
}

#[tokio::test]
async fn an_unverified_hello_cannot_attach_a_surface() {
    for (key, value) in [
        ("token", json!("wrong")),
        ("version", json!(2)),
        ("platform", json!("other")),
        ("capabilities", json!(["adopt_group", "release_group"])),
    ] {
        let surface = RemoteSurface::new("chat".into());
        let (server, mut adapter) = UnixStream::pair().unwrap();
        let task = tokio::spawn(session(
            server,
            "gateway-token",
            surface.clone(),
            async { Ok(()) },
            |_| async { Ok(()) },
        ));
        let mut frame = hello();
        frame[key] = value;
        send(&mut adapter, frame).await.unwrap();
        let result = timeout(Duration::from_secs(2), task)
            .await
            .expect("an invalid hello should close the connection")
            .unwrap();
        assert!(result.is_err());
        assert!(surface.peer().is_none());
    }
}

#[tokio::test]
async fn recovery_posts_before_a_fresh_inbound_can_be_acknowledged() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let surface = RemoteSurface::new("chat".into());
    let recovery_surface = surface.clone();
    let recovered = Arc::new(AtomicBool::new(false));
    let flag = recovered.clone();
    let resume = async move {
        let receipt = recovery_surface
            .post(
                PostTarget::Reply {
                    message_ref: "original".into(),
                    in_thread: true,
                },
                "bound",
                "original-op",
            )
            .await?;
        assert!(receipt.duplicate);
        flag.store(true, Ordering::SeqCst);
        Ok(())
    };
    let (mut reader, mut writer, task) = attach(surface, resume, move |_| {
        let flag = recovered.clone();
        async move {
            ensure!(flag.load(Ordering::SeqCst), "overtook recovery");
            Ok(())
        }
    })
    .await;
    send(&mut writer, json!({"type":"inbound","id":"fresh","message":{"group_id":"g","sender":"u","message_ref":"new","addressing":"ambient","mentions":[],"text":"hello"}})).await.unwrap();
    let retried = next(&mut reader).await;
    assert_eq!(retried["args"]["operation_id"], "original-op");
    send(&mut writer, json!({"type":"result","id":retried["id"],"value":{"duplicate":true,"message_ref":"original-reply"}})).await.unwrap();
    assert_eq!(next(&mut reader).await, json!({"type":"ack","id":"fresh"}));
    drop((reader, writer));
    assert!(task.await.unwrap().is_err());
}

#[test]
fn a_result_has_exactly_one_outcome() {
    for frame in [
        json!({"type":"result","id":"s1","value":true}),
        json!({"type":"result","id":"s1","error":{"kind":"unavailable","detail":"x"}}),
    ] {
        assert!(serde_json::from_value::<AdapterFrame>(frame).is_ok());
    }
    for frame in [
        json!({"type":"result","id":"s1"}),
        json!({"type":"result","id":"s1","value":true,"error":{"kind":"unavailable","detail":"x"}}),
    ] {
        assert!(serde_json::from_value::<AdapterFrame>(frame).is_err());
    }
}

#[tokio::test]
async fn only_complete_bounded_lines_are_parsed() {
    let valid = format!("1{}\n", " ".repeat(framing::MAX_FRAME - 1));
    assert_eq!(
        framing::read::<_, Value>(&mut valid.as_bytes(), &mut Vec::new())
            .await
            .unwrap(),
        1
    );
    let too_big = format!(" {}", valid);
    assert!(
        framing::read::<_, Value>(&mut too_big.as_bytes(), &mut Vec::new())
            .await
            .is_err()
    );
    assert!(
        framing::read::<_, Value>(&mut &b"1"[..], &mut Vec::new())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn binding_recovers_abandoned_sockets_and_preserves_live_endpoints_and_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gateway.sock");
    drop(tokio::net::UnixListener::bind(&path).unwrap());
    let bound = socket::BoundSocket::bind(&path).await.unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(socket::BoundSocket::bind(&path).await.is_err());
    drop(bound);
    assert!(!path.exists());
    std::fs::write(&path, "keep").unwrap();
    assert!(socket::BoundSocket::bind(&path).await.is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "keep");
}
