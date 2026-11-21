//! A fake daemon with canned threads for `claude` and `codex`, a Codex
//! thread, an approval, and the docket of the one Senate (`alice-desk`),
//! where `claude` presides, for working on the native app without a Senate
//! server:
//!
//! ```sh
//! mkdir -p /tmp/sh && chmod 700 /tmp/sh
//! SENATE_HOME=/tmp/sh cargo run -p senate-app-core --example fake_daemon &
//! SENATE_HOME=/tmp/sh SENATE_APP_SHOW=1 apps/macos/build/Build/Products/Debug/Senate.app/Contents/MacOS/Senate
//! ```
//!
//! Every 20 s it asks for one more approval. Ctrl-C stops it. It only ever
//! writes under `SENATE_HOME`, which this example requires to be set.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../tests/support/common.rs"]
mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use senate_bridge::Paths;
use senate_bridge::ipc::WatchEvent;

#[tokio::main]
async fn main() {
    assert!(
        std::env::var_os(senate_bridge::paths::HOME_ENV).is_some_and(|v| !v.is_empty()),
        "set SENATE_HOME to a scratch directory: this fake must not run in the real home"
    );
    let paths = Paths::from_env().expect("a Senate home");
    paths.ensure_home().expect("creating the Senate home");
    let socket = paths.socket();
    let mut w = common::World::new();
    w.sessions = common::hosted_sessions();
    let world = Arc::new(Mutex::new(w));
    let daemon = common::FakeDaemon::start(&socket, Arc::clone(&world));
    println!("fake daemon on {}", socket.display());
    let mut n = 8;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = tokio::time::sleep(Duration::from_secs(20)) => {
                let a = common::approval(&format!("019a-thread:{n}"));
                n += 1;
                world.lock().unwrap().approvals.push(a.clone());
                daemon.emit(WatchEvent::ApprovalRequested(Box::new(a)));
            }
        }
    }
    for r in daemon.requests() {
        println!("{}", serde_json::to_string(&r).unwrap());
    }
}
