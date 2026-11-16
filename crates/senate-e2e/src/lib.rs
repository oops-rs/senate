//! End-to-end tests of Senate: the real server, the real Speaker (with a
//! scripted brain) and real bridge daemons, assembled as `senated`
//! assembles them, with the fake chat platform of `senated::testing` and
//! the fake runner and forge of `senate_work::testing`; and developer
//! machines running `senated --machine` in process, hosting their own
//! Senates (alone on one machine, or one level down under a team's). The
//! tests live in `tests/`; this crate has no library code.
