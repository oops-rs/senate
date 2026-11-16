//! `senate-bridge`: the developer-machine side of Senate. One daemon per
//! machine holds a person's token and seats one agent per kind of coding
//! session (`<person>/claude`, `<person>/codex`) on the committees they work
//! on.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use senate_bridge::delivery::{codex, inbox};
use senate_bridge::hook::{self, HookEnv, HookKind, HookTimeouts};
use senate_bridge::session::Runtime;
use senate_bridge::{BridgeConfig, DaemonOptions, Paths, Timing, daemon, install, mcp, status};

#[derive(Parser)]
#[command(
    name = "senate-bridge",
    version,
    about = "Seats this machine's coding-agent sessions, as their person's agents, on Senate committee logs."
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the per-machine daemon: committee streams, delivery, local IPC.
    Daemon,
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
        /// in bridge.toml, else `codex app-server proxy`).
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

fn run(command: Cmd) -> anyhow::Result<()> {
    match command {
        Cmd::Hook { .. } => Ok(()),
        Cmd::Daemon => {
            init_logging("senate_bridge=info");
            let paths = Paths::from_env()?;
            let config = BridgeConfig::load(&paths.config())?;
            runtime(true)?.block_on(daemon::run(DaemonOptions {
                paths,
                config,
                timing: Timing::default(),
            }))
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
            // The report is in the bridge's language; before bridge.toml
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
            eprintln!("senate-bridge: {e:#}");
            ExitCode::FAILURE
        }
    }
}
