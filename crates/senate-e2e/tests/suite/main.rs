//! The end-to-end tests, built as one test binary. Each file in this
//! directory is a module of it, and the world in `../support` is compiled
//! once and shared (`crate::common`).
//!
//! One file's tests, then one test:
//!
//! ```sh
//! cargo test -p senate-e2e --test suite chat_sim::
//! cargo test -p senate-e2e --test suite -- --exact sqlite::a_restart_keeps_the_log_and_seats_the_agents_again
//! ```
//!
//! Every test here shares one process with the others. A test that changes
//! the process itself (its environment, its working directory) belongs in a
//! binary of its own beside this directory.

#[path = "../support/common.rs"]
mod common;

mod agent_mode;
mod alone;
mod boundary;
mod chat_sim;
mod child_offline;
mod cleanup;
mod continuity;
mod criteria;
mod decisions;
mod delegations;
mod desk_change;
mod english;
mod example_configs;
mod guests;
mod membership;
mod memory;
mod nested;
mod no_chat;
mod parent_child;
mod parent_memory;
mod qualified;
mod release_version;
mod secretary_agents;
mod secretary_team;
mod session_replace;
mod sqlite;
mod supervision;
mod tasks;
mod two_levels;
mod work;
