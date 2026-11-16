//! `senate`: the command for people and their coding agents on a developer
//! machine. It installs Senate into Claude Code and Codex, is the MCP server
//! and the hooks they run, shows what the daemon is doing and what waits for
//! the person, and writes the local profile (`senate init --local`). The
//! daemon itself is `senated` (`senated --machine` on a developer machine).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use senate_bridge::delivery::{codex, inbox};
use senate_bridge::hook::{self, HookEnv, HookKind, HookTimeouts};
use senate_bridge::ipc::{self, DocketReply, InboxReply, Posted, Request, SessionSelector};
use senate_bridge::session::Runtime;
use senate_bridge::{BridgeConfig, Paths, init, install, mcp, preside, status};
use senate_core::{CommitteeId, EventId, Locale, Name, Seq};

#[derive(Parser)]
#[command(
    name = "senate",
    version,
    about = "Senate on a developer machine: install, status, inbox, take, init, and the MCP server and hooks coding agents run. The daemon is `senated --machine`."
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write the profile for using Senate alone on this machine:
    /// `senate.toml` in the Senate home, with a local top Senate whose
    /// Speaker is one of your agents. Writes nothing else; never overwrites.
    Init {
        /// The local profile (the only one `init` writes).
        #[arg(long)]
        local: bool,
        /// Your handle (lowercase letters, digits, `-`).
        #[arg(long)]
        person: Name,
        /// The agent that presides, relative to you.
        #[arg(long, default_value = "claude")]
        speaker: String,
        /// `en` or `zh`.
        #[arg(long, default_value = "en")]
        locale: Locale,
        /// The loopback address the local Senate serves on.
        #[arg(long, default_value = init::DEFAULT_LOCAL_LISTEN)]
        listen: String,
    },
    /// What waits for you: open task threads on a committee for your agents
    /// or escalated to you, and what awaits the Speaker of every Senate one
    /// of your agents presides over here.
    Inbox {
        /// The committee (default: `.senate/committee` in this repository).
        #[arg(long)]
        committee: Option<CommitteeId>,
        /// Also list threads whose last word is an answer (left out by
        /// default: they wait on no one).
        #[arg(long)]
        all: bool,
    },
    /// Take a thread over as yourself: nothing more of it goes to agents.
    Take {
        /// The thread's root seq.
        thread: u64,
        /// The committee (default: `.senate/committee` in this repository).
        #[arg(long)]
        committee: Option<CommitteeId>,
    },
    /// Stdio MCP server (name `senate`) that Claude Code and Codex start per session.
    Mcp,
    /// Claude Code / Codex hook entry point; reads the hook JSON on stdin. Always exits 0.
    Hook {
        #[arg(value_enum)]
        event: HookKind,
        /// Run as a Codex hook (the session is a Codex thread).
        #[arg(long)]
        codex: bool,
    },
    /// Register the MCP server and hooks with Claude Code, and with Codex
    /// when `codex` is on PATH. Dry run without --yes.
    Install {
        /// Set up Codex even when `codex` is not on PATH.
        #[arg(long, conflicts_with = "no_codex")]
        codex: bool,
        /// Leave Codex alone.
        #[arg(long)]
        no_codex: bool,
        /// Codex home holding config.toml (default: $CODEX_HOME, else ~/.codex).
        #[arg(long, value_name = "DIR")]
        codex_home: Option<PathBuf>,
        /// Write the changes.
        #[arg(long)]
        yes: bool,
        /// Claude config dir (default: $CLAUDE_CONFIG_DIR, else ~/.claude).
        #[arg(long, value_name = "DIR")]
        claude_dir: Option<PathBuf>,
        /// The `claude` CLI to register the MCP server with.
        #[arg(long, value_name = "PATH")]
        claude_bin: Option<PathBuf>,
    },
    /// Show the daemon, the Senate it is connected to, each agent's committees, and registered sessions.
    Status,
    /// Smoke test of the Codex app-server adapter: connect, initialize, read
    /// a thread's status and, with --text, start or steer a turn with it.
    CodexSmoke {
        /// The Codex thread id (a Codex session's id).
        #[arg(long)]
        thread: String,
        /// Push this text: a new turn when the thread is idle, a steer of the
        /// running turn when it is busy. Without it nothing is sent.
        #[arg(long)]
        text: Option<String>,
        /// The app-server socket, `unix:///path` (default: `[codex] listen`
        /// in senate.toml, else `codex app-server proxy`).
        #[arg(long, value_name = "URL")]
        listen: Option<String>,
    },
    /// Manual checks.
    Debug {
        #[command(subcommand)]
        command: DebugCmd,
    },
}

#[derive(Subcommand)]
enum DebugCmd {
    /// Write one user message into a Claude Code inbox socket (smoke test of the framing).
    Push {
        /// The inbox socket (a session's CLAUDE_CODE_MESSAGING_SOCKET).
        #[arg(long)]
        socket: PathBuf,
        /// Auth token (default: $CLAUDE_CODE_MESSAGING_TOKEN).
        #[arg(long)]
        token: Option<String>,
        /// The message text.
        #[arg(long)]
        text: String,
    },
}

fn init_logging(default: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    // A second init (never expected) is harmless to ignore.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

fn runtime(multi: bool) -> anyhow::Result<tokio::runtime::Runtime> {
    let mut b = if multi {
        tokio::runtime::Builder::new_multi_thread()
    } else {
        tokio::runtime::Builder::new_current_thread()
    };
    Ok(b.enable_all().build()?)
}

/// Read stdin to the end, giving up after `timeout` (Claude closes it, but a
/// hook must never hang on a stdin that stays open).
fn read_stdin(timeout: Duration) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        let r = std::io::stdin().read_to_string(&mut s).map(|_| s);
        // The receiver is gone if we already timed out; nothing to do then.
        let _ = tx.send(r);
    });
    rx.recv_timeout(timeout).ok()?.ok()
}

/// Hooks never fail and never print anything but their one JSON line.
fn run_hook(kind: HookKind, codex: bool) {
    let Some(stdin) = read_stdin(Duration::from_secs(1)) else {
        return;
    };
    let Ok(paths) = Paths::from_env() else {
        return;
    };
    let Ok(rt) = runtime(false) else {
        return;
    };
    let env = HookEnv::from_process();
    let runtime = codex.then_some(Runtime::Codex);
    let out = rt.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(4),
            hook::run_as(
                kind,
                &paths.socket(),
                &stdin,
                &env,
                runtime,
                HookTimeouts::default(),
            ),
        )
        .await
        .ok()
        .flatten()
    });
    if let Some(line) = out {
        let mut stdout = std::io::stdout().lock();
        // A closed stdout means Claude stopped listening; exit 0 regardless.
        let _ = writeln!(stdout, "{line}");
        let _ = stdout.flush();
    }
}

async fn codex_smoke(
    transport: &codex::Transport,
    thread: &str,
    text: Option<&str>,
) -> anyhow::Result<()> {
    const STEP: Duration = Duration::from_secs(10);
    let step = |what: &'static str| move |_| anyhow::anyhow!("{what}: no answer in {STEP:?}");
    println!("connecting: {transport}");
    let mut server = tokio::time::timeout(STEP, codex::AppServer::connect(transport))
        .await
        .map_err(step("connect"))??;
    println!("initialized");
    let state = tokio::time::timeout(STEP, server.thread_state(thread))
        .await
        .map_err(step("thread/read"))??;
    println!("thread {thread}: {state:?}");
    if matches!(state, codex::ThreadState::Active { .. }) {
        let turn = tokio::time::timeout(STEP, server.active_turn(thread))
            .await
            .map_err(step("thread/turns/list"))??;
        println!("active turn: {}", turn.as_deref().unwrap_or("none"));
    }
    let Some(text) = text else {
        println!("no --text: nothing sent");
        return Ok(());
    };
    let pushed = tokio::time::timeout(STEP, server.push(thread, text))
        .await
        .map_err(step("push"))??;
    match pushed {
        codex::Pushed::Started { turn } => {
            println!(
                "turn/start: new turn {turn} with {} chars",
                text.chars().count()
            );
        }
        codex::Pushed::Steered { turn } => {
            println!(
                "turn/steer: {} chars into turn {turn}",
                text.chars().count()
            );
        }
    }
    Ok(())
}

/// The committee named, else the one in this repository's
/// `.senate/committee`.
async fn committee_here(named: Option<CommitteeId>) -> anyhow::Result<CommitteeId> {
    if let Some(committee) = named {
        return Ok(committee);
    }
    let cwd = std::env::current_dir()?;
    let facts = senate_bridge::session::gather_git_facts(&cwd, Duration::from_secs(2)).await;
    facts
        .committee_file
        .as_deref()
        .and_then(|text| {
            text.lines()
                .map(str::trim)
                .find(|l| !l.is_empty() && !l.starts_with('#'))
        })
        .and_then(|line| CommitteeId::new(line).ok())
        .context("name the committee with --committee (no .senate/committee here)")
}

async fn daemon_call<T: serde::de::DeserializeOwned>(
    paths: &Paths,
    request: &Request,
) -> anyhow::Result<T> {
    Ok(ipc::request(
        &paths.socket(),
        request,
        Duration::from_secs(1),
        Duration::from_secs(20),
    )
    .await?)
}

async fn inbox(
    paths: &Paths,
    committee: Option<CommitteeId>,
    all: bool,
    locale: Locale,
) -> anyhow::Result<String> {
    let committee = committee_here(committee).await?;
    let threads: InboxReply = daemon_call(
        paths,
        &Request::Inbox {
            session: SessionSelector::default(),
            committee: Some(committee),
            agent: None,
            all,
        },
    )
    .await?;
    let mut out = mcp::render_inbox(&threads, locale, mcp::ShowAll::Flag);
    let docket: DocketReply = daemon_call(
        paths,
        &Request::Docket {
            session: SessionSelector::default(),
            committee: None,
        },
    )
    .await?;
    if !docket.senates.is_empty() {
        out.push_str("\n\n");
        out.push_str(&preside::render_docket(&docket, locale));
    }
    Ok(out)
}

fn run(command: Cmd) -> anyhow::Result<()> {
    match command {
        Cmd::Hook { .. } => Ok(()),
        Cmd::Init {
            local,
            person,
            speaker,
            locale,
            listen,
        } => {
            if !local {
                anyhow::bail!(
                    "`senate init` writes the local profile only: run `senate init --local`. \
                     Under a team Senate, write senate.toml from config/senate.example.toml."
                );
            }
            let paths = Paths::from_env()?;
            let profile = init::LocalProfile {
                person,
                speaker: speaker.parse().map_err(anyhow::Error::msg)?,
                locale,
                listen,
            };
            let path = init::init_local(&paths, &profile)?;
            println!("{}", path.display());
            Ok(())
        }
        Cmd::Inbox { committee, all } => {
            let paths = Paths::from_env()?;
            let locale = BridgeConfig::locale_at(&paths.config());
            let text = runtime(false)?.block_on(inbox(&paths, committee, all, locale))?;
            println!("{text}");
            Ok(())
        }
        Cmd::Take { thread, committee } => {
            let paths = Paths::from_env()?;
            let locale = BridgeConfig::locale_at(&paths.config());
            let posted: Posted = runtime(false)?.block_on(async {
                let committee = committee_here(committee).await?;
                daemon_call(
                    &paths,
                    &Request::Take {
                        session: SessionSelector::default(),
                        thread: Seq(thread),
                        id: Some(EventId::new()),
                        committee: Some(committee),
                    },
                )
                .await
            })?;
            println!(
                "{}",
                match locale {
                    Locale::Zh => format!(
                        "你接管了 {} 上的 thread {thread}（seq {}）。",
                        posted.committee, posted.seq
                    ),
                    Locale::En => format!(
                        "You took thread {thread} on {} over (seq {}).",
                        posted.committee, posted.seq
                    ),
                }
            );
            Ok(())
        }
        Cmd::Mcp => {
            init_logging("senate_bridge=warn");
            let paths = Paths::from_env()?;
            let locale = BridgeConfig::locale_at(&paths.config());
            runtime(false)?.block_on(mcp::serve_stdio(
                paths.socket(),
                mcp::selector_from_env(),
                locale,
            ))
        }
        Cmd::Install {
            codex,
            no_codex,
            codex_home,
            yes,
            claude_dir,
            claude_bin,
        } => {
            let exe = std::env::current_exe()
                .and_then(|p| p.canonicalize())
                .context("locating this binary")?;
            let choice = if no_codex {
                install::CodexChoice::No
            } else if codex {
                install::CodexChoice::Yes
            } else {
                install::CodexChoice::Auto
            };
            // The report is in the bridge's language; before senate.toml
            // exists, the default.
            let locale = Paths::from_env()
                .map(|p| BridgeConfig::locale_at(&p.config()))
                .unwrap_or_default();
            let opts =
                install::InstallOptions::resolve(exe, claude_dir, claude_bin, choice, codex_home)?
                    .with_locale(locale);
            let plan = install::plan(&opts)?;
            let apply = yes && !plan.is_noop();
            if apply {
                install::apply(&plan, &opts)?;
            }
            print!("{}", install::render(&plan, &opts, apply));
            Ok(())
        }
        Cmd::CodexSmoke {
            thread,
            text,
            listen,
        } => {
            let paths = Paths::from_env()?;
            let mut config =
                codex::CodexConfig::load(&paths.config()).map_err(anyhow::Error::msg)?;
            if listen.is_some() {
                config.listen = listen;
            }
            let transport = config.transport().map_err(anyhow::Error::msg)?;
            runtime(false)?.block_on(codex_smoke(&transport, &thread, text.as_deref()))
        }
        Cmd::Status => {
            let paths = Paths::from_env()?;
            let view = runtime(false)?.block_on(status::collect(&paths))?;
            print!("{}", status::render(&view, chrono::Utc::now()));
            Ok(())
        }
        Cmd::Debug {
            command:
                DebugCmd::Push {
                    socket,
                    token,
                    text,
                },
        } => {
            let token = token
                .or_else(|| std::env::var("CLAUDE_CODE_MESSAGING_TOKEN").ok())
                .filter(|t| !t.is_empty())
                .context("no token: pass --token or set CLAUDE_CODE_MESSAGING_TOKEN")?;
            let options = inbox::PushOptions {
                timeout: Duration::from_secs(5),
                linger: Duration::from_secs(2),
            };
            let reply =
                runtime(false)?.block_on(inbox::push(&socket, Some(&token), &text, &options))?;
            println!(
                "pushed {} chars to {}",
                text.chars().count(),
                socket.display()
            );
            match reply {
                Some(r) => println!("socket replied: {r}"),
                None => println!("socket sent nothing back"),
            }
            Ok(())
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Cmd::Hook { event, codex } = cli.command {
        run_hook(event, codex);
        return ExitCode::SUCCESS;
    }
    match run(cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("senate: {e:#}");
            ExitCode::FAILURE
        }
    }
}
