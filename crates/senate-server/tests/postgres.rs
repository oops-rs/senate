//! The server over [`PgStore`]. Needs `SENATE_TEST_DATABASE_URL` and skips
//! cleanly without it; runs in a fresh schema, dropped afterwards.

#![allow(clippy::unwrap_used)]

#[path = "support/common.rs"]
mod common;

use std::str::FromStr;
use std::sync::Arc;

use common::*;
use reqwest::{Method, StatusCode};
use senate_core::api::{
    AppendResponse, Cursor, EventsPage, Heartbeat, Me, SearchResults, brief_path, cursor_path,
    events_path, heartbeat_path, search_path,
};
use senate_core::{Brief, EventId, Seq};
use senate_log::{LogStore, PgStore};
use sqlx::AssertSqlSafe;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_over_postgres() {
    let Ok(url) = std::env::var("SENATE_TEST_DATABASE_URL") else {
        eprintln!("skipping: SENATE_TEST_DATABASE_URL is unset");
        return;
    };
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let schema = format!("srv_{}", EventId::new().to_string().to_lowercase());
    sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    // `pg_trgm` in `public`, not in the schema dropped below (which would
    // drop it for every other test schema too); `public` on the path for it.
    if let Err(error) = sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_trgm SCHEMA public")
        .execute(&admin)
        .await
    {
        // A parallel test won the race to create it.
        let installed: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pg_trgm')",
        )
        .fetch_one(&admin)
        .await
        .unwrap();
        assert!(installed, "install pg_trgm: {error}");
    }
    let schemas = format!("{schema},public");
    let options = PgConnectOptions::from_str(&url)
        .unwrap()
        .options([("search_path", schemas.as_str())]);
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await
        .unwrap();
    let store = PgStore::from_pool(pool.clone());
    store.migrate().await.unwrap();
    let epoch = store.epoch().await.unwrap();

    let server = TestServer::start_with(Arc::new(store), None).await;
    for n in 1..=3 {
        assert_eq!(server.append(&format!("m{n}")).await.seq, Seq(n));
    }

    // The database's epoch is the server's, on every response.
    let me = server
        .request(Method::GET, "/v1/me", BOB)
        .send()
        .await
        .unwrap();
    assert_eq!(epoch_of(&me), epoch);
    assert_eq!(me.json::<Me>().await.unwrap().epoch, epoch);
    let ahead = server
        .request(
            Method::GET,
            &format!("{}?after=4", events_path(&committee())),
            BOB,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(epoch_of(&ahead), epoch);
    api_error(ahead, StatusCode::CONFLICT, "cursor_ahead").await;
    let event = note("retried");
    let first: AppendResponse = server.post_event(ALICE, &event).await.json().await.unwrap();
    let again: AppendResponse = server.post_event(ALICE, &event).await.json().await.unwrap();
    assert!(again.duplicate);
    assert_eq!(again.event, first.event);

    let page: EventsPage = server
        .request(
            Method::GET,
            &format!("{}?after=1&limit=2", events_path(&committee())),
            BOB,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(seqs(&page.events), vec![2, 3]);
    assert!(page.has_more);

    let mut stream = server.stream(BOB, None, Some(2)).await;
    assert_eq!(stream.seqs(2).await, vec![3, 4]);
    server.append("live").await;
    assert_eq!(stream.seqs(1).await, vec![5]);

    server
        .request(Method::PUT, &cursor_path(&committee(), "delivered"), BOB)
        .json(&Cursor { seq: Seq(5) })
        .send()
        .await
        .unwrap();
    let cursor: Cursor = server
        .request(Method::GET, &cursor_path(&committee(), "delivered"), BOB)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cursor.seq, Seq(5));
    let beat = server
        .request(Method::POST, &heartbeat_path(&committee()), BOB)
        .json(&Heartbeat {
            paused: false,
            agent: path("claude"),
            sessions: 1,
            cursor: Seq(5),
            left: false,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(beat.status(), StatusCode::NO_CONTENT);

    let found: SearchResults = server
        .request(
            Method::GET,
            &format!("{}?q=M3", search_path(&committee())),
            BOB,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(found.events[0].seq, Seq(3), "the exact match first");
    let brief: Brief = server
        .request(Method::GET, &brief_path(&committee()), BOB)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(brief.head, Seq(5));

    drop(stream);
    server.running.shutdown().await.unwrap();
    pool.close().await;
    sqlx::query(AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
}
