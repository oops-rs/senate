//! Shell registration and per-invocation candidates from the same Clap tree.
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use clap::{CommandFactory as _, ValueEnum as _};
use clap_complete::{
    CompleteEnv,
    engine::CompletionCandidate,
    env::{Bash, EnvCompleter, Fish, Zsh},
};
use senate_core::Locale;

use super::Cli;
use crate::Paths;
use crate::config::LaunchProfile;
use crate::doctor::Check;
use crate::ipc::{self, HostedRuntime, Request, SessionsReply};

const ENV: &str = "SENATE_COMPLETE";
const BUDGET: Duration = Duration::from_millis(150);

#[derive(Clone, Copy, clap::ValueEnum)]
pub(super) enum Shell {
    Zsh,
    Bash,
    Fish,
}

impl Shell {
    fn current() -> Option<Self> {
        let path = PathBuf::from(std::env::var_os("SHELL")?);
        Self::from_str(path.file_name()?.to_str()?, false).ok()
    }

    fn adapter(self) -> &'static dyn EnvCompleter {
        match self {
            Self::Zsh => &Zsh,
            Self::Bash => &Bash,
            Self::Fish => &Fish,
        }
    }

    fn enable(self) -> &'static str {
        match self {
            Self::Zsh => {
                "autoload -Uz compinit && compinit && source <(senate setup completions zsh)"
            }
            Self::Bash => "source <(senate setup completions bash)",
            Self::Fish => "senate setup completions fish | source",
        }
    }

    fn startup_files(self) -> Vec<PathBuf> {
        let Some(home) = dirs::home_dir() else {
            return Vec::new();
        };
        match self {
            Self::Zsh => vec![
                std::env::var_os("ZDOTDIR")
                    .map_or(home, PathBuf::from)
                    .join(".zshrc"),
            ],
            Self::Bash => [".bashrc", ".bash_profile", ".bash_login", ".profile"]
                .into_iter()
                .map(|f| home.join(f))
                .collect(),
            Self::Fish => {
                let fish = std::env::var_os("XDG_CONFIG_HOME")
                    .filter(|value| !value.is_empty())
                    .map_or_else(|| home.join(".config"), PathBuf::from)
                    .join("fish");
                vec![
                    fish.join("config.fish"),
                    fish.join("completions/senate.fish"),
                ]
            }
        }
    }
}

pub(super) fn complete() {
    CompleteEnv::with_factory(command)
        .var(ENV)
        .bin("senate")
        .completer("senate")
        .complete();
}

/// The ordinary CLI rewrites its old top-level `restore` spelling before
/// parsing. Let the completion tree recognize the same compatibility path.
fn command() -> clap::Command {
    let cmd = Cli::command();
    let restore = cmd
        .find_subcommand("session")
        .and_then(|session| session.find_subcommand("restore"))
        .cloned();
    match restore {
        Some(restore) => cmd.subcommand(restore.hide(true)),
        None => cmd,
    }
}

pub(super) fn register(shell: Shell) -> anyhow::Result<()> {
    let mut stdout = std::io::stdout().lock();
    shell
        .adapter()
        .write_registration(ENV, "senate", "senate", "senate", &mut stdout)?;
    stdout.flush()?;
    Ok(())
}

pub(super) fn install_hint(locale: Locale) -> String {
    match Shell::current() {
        Some(shell) => locale.pick(
            format!("Shell 补全：{}", shell.enable()),
            format!("Shell completion — {}", shell.enable()),
        ),
        None => locale.pick(
            "Shell 补全请运行 senate setup completions <zsh|bash|fish>。".into(),
            "Shell completion — senate setup completions <zsh|bash|fish>".into(),
        ),
    }
}

pub(super) fn doctor(locale: Locale) -> Option<Check> {
    let shell = Shell::current()?;
    let name = shell.adapter().name();
    let script = format!("senate setup completions {name}");
    let direct = format!("{ENV}={name} senate");
    for path in shell.startup_files() {
        if !path.is_file() {
            continue;
        }
        if let Ok(contents) = std::fs::read_to_string(&path)
            && contents.lines().any(|line| {
                let line = line.trim();
                !line.starts_with('#')
                    && (line.contains(&script)
                        || line.contains(&direct)
                        || line.contains(&format!("{ENV}=\"{name}\"")))
            })
        {
            return Some(Check::ok(
                "setup.completions",
                locale.pick(
                    format!("{name} 补全配置位于 {}", path.display()),
                    format!("{name} completion registration found in {}", path.display()),
                ),
            ));
        }
    }
    Some(Check::warn(
        "setup.completions",
        locale.pick(
            format!("未找到 {name} 的 Senate 补全配置"),
            format!("no Senate completion registration found for {name}"),
        ),
        Some(shell.enable()),
    ))
}

fn request<T: serde::de::DeserializeOwned>(request: &Request) -> Option<T> {
    let paths = Paths::from_env().ok()?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(async {
        tokio::time::timeout(
            BUDGET,
            ipc::request(&paths.socket(), request, BUDGET, BUDGET),
        )
        .await
        .ok()?
        .ok()
    })
}

fn candidates(values: impl IntoIterator<Item = String>) -> Vec<CompletionCandidate> {
    values.into_iter().map(CompletionCandidate::new).collect()
}

pub(super) fn sessions() -> Vec<CompletionCandidate> {
    let Some(reply) = request::<SessionsReply>(&Request::Sessions) else {
        return Vec::new();
    };
    candidates(
        reply
            .rows
            .into_iter()
            .map(|row| match (&row.committee, &row.alias) {
                (Some(committee), Some(alias)) => format!("{committee}::{alias}"),
                (_, Some(alias)) => alias.to_string(),
                _ => row.primary.to_string(),
            }),
    )
}

pub(super) fn history() -> Vec<CompletionCandidate> {
    let Some(reply) = request::<ipc::SessionHistoryReply>(&Request::SessionHistory {
        committee: None,
        before: None,
        limit: Some(100),
    }) else {
        return Vec::new();
    };
    candidates(reply.rows.into_iter().flat_map(|row| {
        let label = super::cli_history::label(&row);
        [label, row.session_id]
    }))
}

pub(super) fn committees() -> Vec<CompletionCandidate> {
    let Some(rows) = request::<Vec<ipc::RosterCommittee>>(&Request::Roster) else {
        return Vec::new();
    };
    candidates(rows.into_iter().map(|row| row.committee.to_string()))
}

pub(super) fn profiles() -> Vec<CompletionCandidate> {
    let Some(profiles) = request::<BTreeMap<String, LaunchProfile>>(&Request::Profiles) else {
        return Vec::new();
    };
    candidates(profiles.into_keys())
}

pub(super) fn efforts() -> Vec<CompletionCandidate> {
    // CompleteEnv calls the value callback with the actual command words
    // still in argv, after its registration separator. Parse the prefix so
    // a model or alias named "codex" cannot be mistaken for a runtime.
    let words: Vec<OsString> = std::env::args_os()
        .skip_while(|word| word != "--")
        .skip(1)
        .collect();
    let index = std::env::var("_CLAP_COMPLETE_INDEX")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| words.len().saturating_sub(1));
    let args = command()
        .ignore_errors(true)
        .try_get_matches_from(words.into_iter().take(index))
        .ok();
    let runtime = args.as_ref().and_then(effort_runtime);
    // The unqualified union is useful before a runtime is chosen. Once
    // chosen, use exactly the runtime's own validated choices.
    let values = runtime.map_or_else(
        || {
            let mut union = Vec::new();
            for runtime in HostedRuntime::value_variants() {
                for &effort in runtime.efforts() {
                    if !union.contains(&effort) {
                        union.push(effort);
                    }
                }
            }
            union
        },
        |runtime| runtime.efforts().to_vec(),
    );
    candidates(values.into_iter().map(str::to_owned))
}

fn effort_runtime(args: &clap::ArgMatches) -> Option<HostedRuntime> {
    if let Some(spawn) = args.subcommand_matches("spawn") {
        return spawn
            .get_one::<HostedRuntime>("runtime")
            .copied()
            .or_else(|| {
                let profile = spawn.get_one::<String>("profile")?;
                request::<BTreeMap<String, LaunchProfile>>(&Request::Profiles)?
                    .get(profile)
                    .map(|p| p.runtime)
            });
    }
    let action = args.subcommand_matches("restore").or_else(|| {
        let (name, action) = args.subcommand_matches("session")?.subcommand()?;
        matches!(name, "restore" | "replace").then_some(action)
    })?;
    let name = action.get_one::<senate_core::alias::AliasName>("senator")?;
    let rows = request::<SessionsReply>(&Request::Sessions)?.rows;
    let mut matches = rows.iter().filter(|row| {
        (row.primary == name.alias || row.alias.as_ref() == Some(&name.alias))
            && name
                .committee
                .as_ref()
                .is_none_or(|committee| row.committee.as_ref() == Some(committee))
    });
    let first = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    HostedRuntime::from_str(first.runtime.as_str(), false).ok()
}
