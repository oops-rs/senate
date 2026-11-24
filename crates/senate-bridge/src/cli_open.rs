//! `senate open`: show a place in the Senate app. The link (or a route such
//! as `committee/senate`) is canonicalised by the one v1 grammar
//! ([`senate_observe_protocol::link`]) and handed to the platform launcher
//! as one argument, never through a shell. A link only navigates.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::bail;
use clap::Args;
use clap_complete::engine::{ArgValueCandidates, CompletionCandidate};
use senate_observe_protocol::link::AppLink;

#[derive(Args)]
pub(super) struct Open {
    /// A `senate://open/v1/…` link, or a route such as `committee/senate`,
    /// `thread/senate/42`, `session/senate::claude`, `desk` or `inbox`.
    #[arg(add = ArgValueCandidates::new(routes))]
    target: String,
    /// Print the canonical link instead of opening it.
    #[arg(long)]
    print: bool,
}

/// The macOS launcher.
const OPEN: &str = "/usr/bin/open";

pub(super) fn run(args: Open) -> anyhow::Result<()> {
    let link = canonical(&args.target)?;
    if args.print {
        println!("{link}");
        return Ok(());
    }
    if cfg!(target_os = "macos") {
        launch(Path::new(OPEN), &link)
    } else {
        bail!("opening Senate links needs the Senate app, which is for macOS; the link is {link}")
    }
}

/// The canonical link for a full link or a route; a route's values are
/// written decoded (`session/senate::claude`).
fn canonical(target: &str) -> anyhow::Result<AppLink> {
    let target = target.trim();
    let link = if target.contains("://") {
        target.parse()
    } else {
        AppLink::from_route(target)
    };
    link.map_err(|_| {
        anyhow::anyhow!(
            "not a Senate app link or route: {target}\n\
             Try a link such as senate://open/v1/inbox, or a route such as \
             committee/<name>, thread/<committee>/<seq>, session/<committee>::<alias>, \
             senator/<name>, desk, desk/<committee>/<seq>, approvals, inbox, history, \
             history/<session-id> or profile."
        )
    })
}

/// Hand the link to `launcher` as its one argument.
fn launch(launcher: &Path, link: &AppLink) -> anyhow::Result<()> {
    let output = Command::new(launcher)
        .arg(link.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    match output {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            let said = String::from_utf8_lossy(&output.stderr);
            let said = said.trim();
            if said.is_empty() || said.contains("No application") || said.contains("-10814") {
                bail!(
                    "no app is registered for senate:// links: install the Senate app \
                     and open it once, then try again ({link})"
                )
            }
            bail!("could not open {link}: {said}")
        }
        Err(error) => bail!("could not run {}: {error}", launcher.display()),
    }
}

/// The route words, then each committee's page.
fn routes() -> Vec<CompletionCandidate> {
    let words = [
        "inbox",
        "approvals",
        "desk",
        "history",
        "profile",
        "committee/",
        "thread/",
        "session/",
        "senator/",
        "desk/",
        "history/",
    ];
    words
        .into_iter()
        .map(str::to_owned)
        .chain(
            super::completion::committees()
                .into_iter()
                .map(|c| format!("committee/{}", c.get_value().to_string_lossy())),
        )
        .map(CompletionCandidate::new)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_and_links_canonicalise_by_the_one_grammar() {
        for (input, expected) in [
            ("committee/senate", "senate://open/v1/committee/senate"),
            ("thread/senate/42", "senate://open/v1/thread/senate/42"),
            (
                "session/senate::claude",
                "senate://open/v1/session/senate%3A%3Aclaude",
            ),
            ("inbox", "senate://open/v1/inbox"),
            (" desk ", "senate://open/v1/desk"),
            (
                "senate://open/v1/session/senate%3a%3aclaude?senate=%2fteam%2flocal",
                "senate://open/v1/session/senate%3A%3Aclaude?senate=%2Fteam%2Flocal",
            ),
        ] {
            assert_eq!(canonical(input).unwrap().to_string(), expected, "{input}");
        }
        for input in [
            "senate://intent/v1/reply/senate/42",
            "senate://evil/v1/inbox",
            "https://example.com",
            "approve/42",
            "thread/senate/0",
            "committee/a b",
            "inbox; rm -rf ~",
            "$(touch x)",
            "",
        ] {
            assert!(canonical(input).is_err(), "{input}");
        }
    }

    #[test]
    fn the_launcher_gets_the_link_as_one_argument_and_failures_are_explained() {
        let link = canonical("thread/senate/42").unwrap();
        launch(Path::new("/usr/bin/true"), &link).unwrap();
        let refused = launch(Path::new("/usr/bin/false"), &link)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("no app is registered"), "{refused}");
        let missing = launch(Path::new("/nonexistent/open"), &link)
            .unwrap_err()
            .to_string();
        assert!(missing.contains("could not run"), "{missing}");
    }
}
