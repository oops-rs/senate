//! Keeping a test's processes away from the session that runs the tests.
//!
//! A test suite is often run from inside a coding session, whose
//! environment names its own inbox socket and token
//! (`CLAUDE_CODE_MESSAGING_SOCKET`, `CLAUDE_CODE_MESSAGING_TOKEN`), its
//! Senate home, its agent pin and its Claude Code and Codex homes. A
//! `senate` or `senated` a test spawns inherits all of it unless told
//! otherwise, and a hook that registers that socket gets the test's
//! messages pushed into the real session.
//!
//! Two guards, so neither has to be remembered alone:
//!
//! - [`isolate`] is how every test spawns one of these binaries: it removes
//!   [`INHERITED`] and sets [`CONFINE_ENV`] to the test's temp directory
//!   (a convention test checks every spawn goes through it).
//! - A binary started with [`CONFINE_ENV`] set uses no inbox socket outside
//!   that directory ([`confined`]): the hook drops one from its
//!   environment, and delivery never connects to one. A variable set again
//!   after [`isolate`], or a socket registered some other way, still
//!   cannot reach outside the test.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The directory a test's processes are confined to.
pub const CONFINE_ENV: &str = "SENATE_CONFINE_TO";

/// What a spawned binary must never inherit from the process running the
/// tests.
pub const INHERITED: [&str; 14] = [
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_PID",
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "CODEX_THREAD_ID",
    "SENATE_HOME",
    "SENATE_AGENT",
    "SENATE_AGY_BIN",
    crate::delivery::agy::PROOF_ENV,
    "SENATED_CONFIG",
    "RUST_LOG",
    CONFINE_ENV,
];

/// Prepare `cmd` (a `senate` or `senated` a test spawns): nothing in
/// [`INHERITED`] reaches it, and it is confined to `root`, the test's temp
/// directory. Set what the test means to give it after this.
pub fn isolate<'a>(cmd: &'a mut Command, root: &Path) -> &'a mut Command {
    for var in INHERITED {
        cmd.env_remove(var);
    }
    cmd.env(CONFINE_ENV, root)
}

/// [`isolate`] for a `tokio` command.
pub fn isolate_async<'a>(
    cmd: &'a mut tokio::process::Command,
    root: &Path,
) -> &'a mut tokio::process::Command {
    isolate(cmd.as_std_mut(), root);
    cmd
}

/// Whether this process may use the inbox socket `socket`: always, unless
/// it was started confined ([`CONFINE_ENV`]), and then only one inside that
/// directory.
pub fn confined(socket: &Path) -> bool {
    match std::env::var_os(CONFINE_ENV).filter(|v| !v.is_empty()) {
        None => true,
        Some(root) => within(socket, Path::new(&root)),
    }
}

fn within(path: &Path, root: &Path) -> bool {
    path.is_absolute() && resolved(path).starts_with(resolved(root))
}

/// `path` with its deepest existing ancestor made canonical (the socket,
/// and its directory, may not exist yet), and `..` refused by keeping it.
fn resolved(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut at = path;
    loop {
        if let Ok(canonical) = at.canonicalize() {
            return rest.iter().rev().fold(canonical, |p, part| p.join(part));
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                at = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_socket_is_within_its_root_only() {
        let dir = tempfile::tempdir().unwrap();
        let inside = dir.path().join("s.sock");
        assert!(within(&inside, dir.path()));
        assert!(within(&dir.path().join("a/b.sock"), dir.path()));
        assert!(!within(Path::new("/tmp/elsewhere.sock"), dir.path()));
        assert!(!within(Path::new("relative.sock"), dir.path()));
        assert!(!within(&dir.path().join("../x.sock"), dir.path()));
    }
}
