//! Child processes: bounded in time, with secrets scrubbed from anything
//! that leaves this crate as text.

use std::process::{Output, Stdio};
use std::time::Duration;

use tokio::process::Command;

/// Replaces known secret values with `***` in text that may be shown or logged.
#[derive(Debug, Clone, Default)]
pub(crate) struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    pub(crate) fn with(mut self, secret: &str) -> Self {
        let s = secret.trim();
        if s.len() >= 4 {
            self.secrets.push(s.to_owned());
        }
        self
    }

    pub(crate) fn redact(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for s in &self.secrets {
            out = out.replace(s.as_str(), "***");
        }
        out
    }
}

/// Run to completion within `limit`, capturing stdout and stderr. The child
/// is killed if the limit passes.
pub(crate) async fn output(cmd: &mut Command, limit: Duration) -> Result<Output, String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match tokio::time::timeout(limit, cmd.output()).await {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(e)) => Err(format!(
            "could not start {:?}: {e}",
            cmd.as_std().get_program()
        )),
        Err(_) => Err(format!(
            "{:?} took longer than {}s",
            cmd.as_std().get_program(),
            limit.as_secs()
        )),
    }
}

/// The last few non-empty lines of a child's stderr, for a failure reason.
pub(crate) fn tail(bytes: &[u8], lines: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut kept: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .rev()
        .take(lines)
        .collect();
    kept.reverse();
    crate::stream::clip(&kept.join(" | "), 400)
}

/// Quote for a POSIX shell: `'…'` with embedded quotes escaped.
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_and_quotes() {
        let r = Redactor::default().with("sk-abcdef").with("");
        assert_eq!(r.redact("key sk-abcdef!"), "key ***!");
        assert_eq!(sh_quote("a'b c"), r"'a'\''b c'");
        assert_eq!(tail(b"a\n\nb\nc\n", 2), "b | c");
    }
}
