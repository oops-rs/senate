//! The bridge's integration tests, built as one test binary. Each file in
//! this directory is a module of it, and the support code in `../support`
//! is compiled once and shared (`crate::common`, `crate::agy`).
//!
//! One file's tests, then one test:
//!
//! ```sh
//! cargo test -p senate-bridge --test suite hosted::
//! cargo test -p senate-bridge --test suite -- --exact hosted::a_finished_test_leaves_no_host_or_tui_behind
//! ```
//!
//! Every test here shares one process with the others. A test that changes
//! the process itself (its environment, its working directory) belongs in a
//! binary of its own beside this directory, as `../upgrade.rs` is. A test
//! that runs this executable again to get a second process names its
//! helper by its full name, module included (`timeline::agy_writer_child`).

// Built on Unix fixtures (`#!/bin/sh` fakes, PTYs, Unix sockets). Windows runs
// the native Codex path in ../native_codex.rs.
#![cfg(unix)]

#[allow(clippy::unwrap_used)]
#[path = "../support/agy_timeline.rs"]
mod agy;
#[path = "../support/common.rs"]
mod common;

mod approvals;
mod chair_handover;
mod checkpoint;
mod cli;
mod codex;
mod committee_activity;
mod companion;
mod completion;
mod convention;
mod daemon;
mod default_secretary;
mod desk_cli;
mod desk_decision;
mod follow;
mod hosted;
mod hosted_approvals;
mod isolation;
mod join_default;
mod mcp;
mod remote_machines;
mod service;
mod speak;
mod stats;
mod tasks;
mod terminal;
mod timeline;
