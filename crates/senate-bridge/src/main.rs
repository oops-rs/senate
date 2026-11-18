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
use clap::{Args, Parser, Subcommand};
use senate_bridge::binding::{self, Settings};
use senate_bridge::delivery::{codex, inbox};
use senate_bridge::hook::{self, HookEnv, HookKind, HookTimeouts};
use senate_bridge::ipc::SayKind;
use senate_bridge::ipc::{self, DocketReply, InboxReply, Posted, Request, SessionSelector};
use senate_bridge::session::Runtime;
use senate_bridge::{
    BridgeConfig, Paths, init, install, mcp, person, preside, service, speak, status,
};
use senate_core::{CommitteeId, CommitteeSeatAction, EventId, Locale, Name, SenatorName, Seq};

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
        /// The Speaker, as a senator id (`senator::claude`: an agent
        /// presides over an unattended root). Default: you.
        #[arg(long)]
        speaker: Option<String>,
        /// `en` or `zh`.
        #[arg(long, default_value = "en")]
        locale: Locale,
        /// The loopback address the local Senate serves on.
        #[arg(long, default_value = init::DEFAULT_LOCAL_LISTEN)]
        listen: String,
    },
    /// Bind a worktree to a committee, or give one session a persistent alias with --alias.
    /// The worktree form
    /// writes `.senate/committee` (untracked, kept per branch: switching
    /// branches swaps in that branch's own copy). Live sessions here pick it
    /// up at their next prompt. No daemon needed.
    ///
    /// Without a name, the committee is derived from where this is run, and
    /// printed with how: the repository's name (the last segment of the
    /// `origin` URL without `.git`; else the main worktree's folder; outside
    /// git, the directory's name), plus `-<branch>` unless on the default
    /// branch (what `origin/HEAD` points at, else main, master or trunk) or
    /// detached; lowercased, other characters becoming `-`, at most 63
    /// characters (a longer one ends in `-` and a short hash). Read-only.
    Join {
        /// The committee. With --alias, omit to use the session's current committee.
        committee: Option<CommitteeId>,
        /// Bind an alias to the primary ID of one coding session.
        #[arg(long, visible_alias = "senator")]
        alias: Option<senate_core::SenatorName>,
        /// Full session ID. Defaults to the calling coding session.
        #[arg(long, requires = "alias")]
        session: Option<String>,
    },
    /// List the registered aliases and their primary senator IDs.
    Aliases,
    /// Remove this worktree's committee binding on the current branch.
    Leave,
    /// Change one senator's membership of a committee.
    Seat {
        #[command(subcommand)]
        command: SeatCmd,
    },
    /// Suggest a flat initial senator alias to sessions on this worktree's current branch.
    Pin {
        /// Initial alias, e.g. reviewer.
        agent: String,
    },
    /// Remove this worktree's agent pin on the current branch.
    Unpin,
    /// Print the committee this directory resolves to and where it comes
    /// from, the agent a session here would speak as, the branch `.senate/`
    /// belongs to, and whether other branches have saved copies.
    Where,
    /// List every saved `.senate/` copy, by repository: its branch,
    /// committee, when it was saved, and whether it has expired.
    List {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Forget what has expired: saved copies whose repository, branch or
    /// detached worktree is gone; session records of sessions that ended
    /// long ago or whose process is gone; queued items and cursors of
    /// agents with no session and no contact for `free_after_days`
    /// (senate.toml, default 30). A hosted Senate's log is only listed.
    /// Dry run without --yes.
    Free {
        /// Do it.
        #[arg(long)]
        yes: bool,
        /// With --yes, also delete the data of hosted Senates that
        /// `senate.toml` no longer lists: logs, which nothing else removes.
        #[arg(long, requires = "yes")]
        delete_logs: bool,
    },
    /// What waits for you: first what was said to you that you have not
    /// seen (an answer, a note, an escalation with its reason), then open
    /// task threads on a committee for your agents or escalated to you, and
    /// what awaits the Speaker of every Senate one of your agents presides
    /// over here.
    Inbox {
        /// The committee (default: the one `senate where` shows for this directory).
        #[arg(long)]
        committee: Option<CommitteeId>,
        /// Also list threads whose last word is an answer (left out by
        /// default: they wait on no one).
        #[arg(long)]
        all: bool,
        /// Mark every thread listed as new for you as read, after listing
        /// it. An escalation to you keeps waiting until you act on it.
        #[arg(long)]
        mark_read: bool,
    },
    /// Ask a question as yourself. `<to>` is a senator's name (an agent
    /// such as `claude`, or a person such as `bob`), whichever agent (`*`),
    /// `speaker`, a `senator::…` address, or a qualified name on another
    /// committee (`ibot::codex`).
    /// Text `-` is read from stdin; put `--` before text that starts with `-`.
    /// Goes to the primary Senate of this machine.
    Ask {
        /// Who to ask.
        to: String,
        /// The question (`-`: read stdin).
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
        /// The committee (default: the one `senate where` shows for this directory).
        #[arg(long)]
        committee: Option<CommitteeId>,
        /// Stay until an answer arrives in the thread, or it is escalated to
        /// you (default 120 seconds), and print it; without it, return at once.
        #[arg(
            long,
            num_args = 0..=1,
            require_equals = true,
            default_missing_value = "120",
            value_name = "SECONDS"
        )]
        wait: Option<u64>,
        /// Print the daemon's result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Tell something as yourself (a note: no answer expected); `<to>` as for `ask`.
    Tell {
        /// Who to tell.
        to: String,
        /// The note (`-`: read stdin).
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
        /// The committee (default: the one `senate where` shows for this directory).
        #[arg(long)]
        committee: Option<CommitteeId>,
        /// Print the daemon's result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Ask an agent to do something, as yourself (a request); `<to>` as for `ask`.
    Request {
        /// Who to ask.
        to: String,
        /// What to do (`-`: read stdin).
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
        /// The committee (default: the one `senate where` shows for this directory).
        #[arg(long)]
        committee: Option<CommitteeId>,
        /// Stay until the agent answers or escalates to you in that thread
        /// (default 120 seconds) and print it; without it, return at once.
        #[arg(
            long,
            num_args = 0..=1,
            require_equals = true,
            default_missing_value = "120",
            value_name = "SECONDS"
        )]
        wait: Option<u64>,
        /// Print the daemon's result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Say something as yourself in an existing thread (a follow-up or a
    /// correction), to whoever took part in it.
    Reply {
        /// A seq in the thread (its root's, as `inbox` lists).
        thread: u64,
        /// What to say (`-`: read stdin).
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
        /// The committee (default: the one `senate where` shows for this directory).
        #[arg(long)]
        committee: Option<CommitteeId>,
        /// Print the daemon's result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Inspect a recorded event and its local delivery state.
    Receipt {
        seq: u64,
        /// Committee of the event. Defaults to this directory's committee.
        #[arg(long)]
        committee: Option<CommitteeId>,
        #[arg(long)]
        json: bool,
    },
    /// Print a thread: each event with who wrote it, when, and what.
    Thread {
        /// A seq in the thread.
        seq: u64,
        /// The committee (default: the one `senate where` shows for this directory).
        #[arg(long)]
        committee: Option<CommitteeId>,
        /// Print the daemon's result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Stay in the foreground and print, as they arrive, the events addressed
    /// to you (answers, notes, questions, escalations with their reason) on
    /// the committees this machine's sessions are on, or the one named. Not
    /// what is put to the Speaker: that is the agent's docket. It marks
    /// nothing as read.
    Watch {
        #[command(flatten)]
        scope: DisplayScope,
        /// One JSON object per line.
        #[arg(long)]
        json: bool,
    },
    /// Take a thread over as yourself: nothing more of it goes to agents.
    Take {
        /// The thread's root seq.
        thread: u64,
        /// The committee (default: the one `senate where` shows for this directory).
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
    /// Keep `senated --machine` running without a terminal: a launchd user
    /// agent on macOS, a systemd user unit on Linux.
    Service {
        #[command(subcommand)]
        command: ServiceCmd,
    },
    /// Show the daemon, the Senate it is connected to, each agent's committees, and registered sessions.
    Status {
        #[command(flatten)]
        scope: DisplayScope,
        /// Print full session IDs for use with join --session.
        #[arg(long)]
        full_ids: bool,
    },
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

#[derive(Args)]
struct DisplayScope {
    /// Show all committees on this machine.
    #[arg(long, conflicts_with = "committee")]
    global: bool,
    /// Show this committee. Defaults to the current worktree's committee.
    #[arg(long)]
    committee: Option<CommitteeId>,
}

impl DisplayScope {
    async fn resolve(self, paths: &Paths) -> anyhow::Result<Option<CommitteeId>> {
        if self.global || self.committee.is_some() {
            return Ok(self.committee);
        }
        let settings = Settings::load(&paths.config());
        let cwd = std::env::current_dir()?;
        Ok(binding::directory_committee(paths, &cwd, &settings).await)
    }
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Write the service file and load the service: it runs `senated
    /// --machine` from login, restarts it if it dies and logs to
    /// `senated.log` in the Senate home. Dry run without --yes.
    Install {
        /// Write the file and load the service.
        #[arg(long)]
        yes: bool,
        /// The daemon binary (default: `senated` next to this `senate`, else
        /// on PATH).
        #[arg(long, value_name = "PATH")]
        senated: Option<PathBuf>,
    },
    /// Stop the service and remove its file. Dry run without --yes.
    Uninstall {
        /// Stop it and remove the file.
        #[arg(long)]
        yes: bool,
    },
    /// Whether the service file exists, whether the service manager has the
    /// service loaded and running, and where its log is.
    Status,
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

#[derive(Subcommand)]
enum SeatCmd {
    /// Remove the senator's seat and end its connection on this committee.
    Unseat(SeatArgs),
    /// Explicitly join the committee again under the same senator id.
    Join(SeatArgs),
}

#[derive(Args)]
struct SeatArgs {
    #[arg(long)]
    committee: CommitteeId,
    /// A primary senator id or alias in this Senate.
    #[arg(long, value_parser = parse_seat_name)]
    senator: SenatorName,
    /// The primary id acting for the Speaker's office.
    #[arg(long, value_parser = parse_seat_name)]
    acted_by: Option<SenatorName>,
    /// Reuse the same operation id when retrying.
    #[arg(long)]
    id: Option<EventId>,
    /// Print the server's current seat state as JSON.
    #[arg(long)]
    json: bool,
}

fn parse_seat_name(raw: &str) -> Result<SenatorName, senate_core::SenatorNameError> {
    raw.strip_prefix("senator::").unwrap_or(raw).parse()
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

/// The committee named, else the one this directory resolves to
/// (`senate where`).
async fn committee_here(named: Option<CommitteeId>) -> anyhow::Result<CommitteeId> {
    let (paths, settings) = settings_here();
    binding::committee_here(&paths, named, &settings).await
}

fn settings_here() -> (Paths, Settings) {
    let paths = Paths::from_env().unwrap_or_else(|_| Paths::new("."));
    let settings = Settings::load(&paths.config());
    (paths, settings)
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
    mark_read: bool,
    locale: Locale,
) -> anyhow::Result<String> {
    let committee = committee_here(committee).await?;
    // The daemon's language, as every command that speaks as the person.
    let locale = match daemon_call::<ipc::Pong>(paths, &Request::Ping).await {
        Ok(pong) => pong.locale,
        Err(_) => locale,
    };
    let threads: InboxReply = daemon_call(
        paths,
        &Request::Inbox {
            session: SessionSelector::default(),
            committee: Some(committee.clone()),
            agent: None,
            all,
        },
    )
    .await?;
    let mut out = person::render_inbox(&threads, locale, mcp::ShowAll::Flag);
    if mark_read {
        let marks = person::unread_marks(&threads);
        if !marks.is_empty() {
            let _: serde_json::Value = daemon_call(
                paths,
                &Request::Seen {
                    committee: threads.committee.clone(),
                    marks: marks.clone(),
                },
            )
            .await?;
        }
        out.push('\n');
        out.push_str(&person::render_marked(marks.len(), locale));
    }
    let docket: DocketReply = daemon_call(
        paths,
        &Request::Docket {
            session: SessionSelector::default(),
            committee: Some(committee),
        },
    )
    .await?;
    if !docket.senates.is_empty() {
        out.push_str("\n\n");
        out.push_str(&preside::render_docket(&docket, locale));
    }
    Ok(out)
}

/// `senate watch`: what is addressed to the person, as it arrives.
async fn watch(paths: &Paths, committee: Option<CommitteeId>, json: bool) -> anyhow::Result<()> {
    let pong: ipc::Pong = daemon_call(paths, &Request::Ping).await?;
    let mut stream = ipc::IpcClient::connect(&paths.socket(), Duration::from_secs(1))
        .await?
        .watch(false, Duration::from_secs(5))
        .await?;
    // The daemon subscribes this connection before it answers, so from here
    // on nothing addressed to the person is missed. Said on stderr, in
    // `--json` too (stdout stays one object per line): a script that must
    // not miss anything waits for it.
    eprintln!(
        "{}",
        person::render_watching(committee.as_ref(), pong.locale)
    );
    loop {
        let Some(event) = stream.next().await? else {
            anyhow::bail!("{}", person::render_watch_closed(pong.locale));
        };
        let ipc::WatchEvent::ForPerson {
            news,
            escalated,
            text,
        } = &event
        else {
            continue;
        };
        if committee.as_ref().is_some_and(|c| c != &news.committee) {
            continue;
        }
        let mut out = std::io::stdout().lock();
        if json {
            writeln!(out, "{}", serde_json::to_string(&event)?)?;
        } else {
            writeln!(
                out,
                "{}\n",
                person::render_news(news, *escalated, text, pong.locale)
            )?;
        }
        out.flush()?;
    }
}

/// The words of a message, or stdin when they are a lone `-`.
fn message_text(words: Vec<String>) -> anyhow::Result<String> {
    let text = if words == ["-"] {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .context("reading the text from stdin")?;
        s.trim_end().to_string()
    } else {
        words.join(" ")
    };
    anyhow::ensure!(!text.trim().is_empty(), "there is no text to send");
    Ok(text)
}

fn print_json(value: &impl serde::Serialize) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// `ask`, `tell` and `request`: speak as the person, and with `wait` stay
/// for the answer.
fn speak_new(
    kind: SayKind,
    to: String,
    text: Vec<String>,
    committee: Option<CommitteeId>,
    wait: Option<u64>,
    json: bool,
) -> anyhow::Result<()> {
    let paths = Paths::from_env()?;
    let text = message_text(text)?;
    runtime(false)?.block_on(async {
        let committee = committee_here(committee).await?;
        let socket = paths.socket();
        let sent = speak::send(&socket, committee, kind, &to, text).await?;
        let Some(secs) = wait else {
            if json {
                return print_json(&sent.posted);
            }
            println!("{}", sent.render(false));
            return Ok(());
        };
        if !json {
            println!("{}", sent.render(true));
            std::io::stdout().flush()?;
        }
        match speak::wait_answer(&socket, &sent, Duration::from_secs(secs)).await? {
            Some(answer) if json => {
                print_json(&serde_json::json!({ "posted": sent.posted, "answer": answer }))
            }
            Some(answer) => {
                println!("{}", speak::render_arrival(&answer, sent.locale));
                Ok(())
            }
            None => anyhow::bail!(speak::render_timeout(sent.posted.seq, secs, sent.locale)),
        }
    })
}

fn service_spec() -> anyhow::Result<(service::Spec, Locale)> {
    let platform = service::Platform::current()
        .context("`senate service` supports macOS (launchd) and Linux (systemd)")?;
    let user_home = dirs::home_dir().context("cannot find the home directory")?;
    let paths = Paths::from_env()?;
    let senate_home = std::path::absolute(paths.home()).context("resolving the Senate home")?;
    let locale = BridgeConfig::locale_at(&paths.config());
    Ok((
        service::Spec {
            platform,
            user_home,
            senate_home,
            uid: service::current_uid(),
            path: std::env::var("PATH").ok().filter(|p| !p.is_empty()),
        },
        locale,
    ))
}

fn run_service(command: ServiceCmd) -> anyhow::Result<()> {
    let (spec, locale) = service_spec()?;
    match command {
        ServiceCmd::Install { yes, senated } => {
            let senated = match senated {
                Some(path) => std::path::absolute(path)?,
                None => {
                    let exe = std::env::current_exe().context("locating this binary")?;
                    service::locate_senated(&exe, std::env::var_os("PATH").as_deref()).context(
                        "`senated` is neither next to `senate` nor on PATH: pass it with --senated",
                    )?
                }
            };
            let plan = service::plan_install(&spec, &senated);
            if yes {
                service::apply_install(&plan, &senated, &service::SystemManager)?;
            }
            print!(
                "{}",
                service::render_install(&plan, spec.platform, yes, locale)
            );
        }
        ServiceCmd::Uninstall { yes } => {
            let plan = service::plan_uninstall(&spec);
            if yes {
                service::apply_uninstall(&plan, &service::SystemManager)?;
            }
            print!("{}", service::render_uninstall(&plan, yes, locale));
        }
        ServiceCmd::Status => {
            let status = service::status(&spec, &service::SystemManager);
            print!("{}", service::render_status(&status, locale));
        }
    }
    Ok(())
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
                speaker: speaker
                    .map(|s| {
                        let s = if s.contains("::") {
                            s
                        } else {
                            format!("senator::{s}")
                        };
                        s.parse::<senate_core::SenatorId>()
                    })
                    .transpose()
                    .map_err(anyhow::Error::msg)?,
                locale,
                listen,
            };
            let path = init::init_local(&paths, &profile)?;
            println!("{}", path.display());
            Ok(())
        }
        Cmd::Aliases => {
            let paths = Paths::from_env()?;
            let rows: senate_core::api::SenatorAliases =
                runtime(false)?.block_on(daemon_call(&paths, &Request::Aliases))?;
            if rows.is_empty() {
                println!(
                    "{}",
                    BridgeConfig::locale_at(&paths.config())
                        .pick("尚无 senator 别名。", "No senator aliases registered.")
                );
            }
            for (alias, primary) in rows {
                println!(
                    "{} -> {}",
                    senate_core::Address::senator(&alias).in_text(),
                    senate_core::Address::senator(&primary).in_text()
                );
            }
            Ok(())
        }
        Cmd::Seat { command } => {
            let (action, args) = match command {
                SeatCmd::Unseat(args) => (CommitteeSeatAction::Unseat, args),
                SeatCmd::Join(args) => (CommitteeSeatAction::Join, args),
            };
            let paths = Paths::from_env()?;
            let request = senate_core::api::SetCommitteeSeat {
                senator: args.senator,
                action,
                id: args.id.unwrap_or_default(),
                acted_by: args.acted_by,
            };
            let reply: senate_core::api::CommitteeSeatInfo =
                runtime(false)?.block_on(daemon_call(
                    &paths,
                    &Request::SetCommitteeSeat {
                        committee: args.committee,
                        request,
                    },
                ))?;
            if args.json {
                return print_json(&reply);
            }
            let locale = BridgeConfig::locale_at(&paths.config());
            let seat = senate_core::Address::senator(&reply.senator).in_text();
            match reply.action {
                CommitteeSeatAction::Unseat => println!(
                    "{}",
                    locale.pick(
                        format!("{seat} 已从 {} 除席（seq {}）", reply.committee, reply.seq),
                        format!(
                            "{seat} unseated from {} (seq {})",
                            reply.committee, reply.seq
                        ),
                    ),
                ),
                CommitteeSeatAction::Join => println!(
                    "{}",
                    locale.pick(
                        format!("{seat} 已加入 {}（seq {}）", reply.committee, reply.seq),
                        format!("{seat} joined {} (seq {})", reply.committee, reply.seq),
                    ),
                ),
            }
            Ok(())
        }
        Cmd::Join {
            committee,
            alias: Some(alias),
            session,
        } => {
            let paths = Paths::from_env()?;
            let locale = BridgeConfig::locale_at(&paths.config());
            let env_id = |key| std::env::var(key).ok().filter(|id| !id.is_empty());
            let id = match (
                session,
                env_id("CODEX_THREAD_ID"),
                env_id("CLAUDE_CODE_SESSION_ID"),
            ) {
                (Some(id), _, _) => id,
                (None, Some(id), None) | (None, None, Some(id)) => id,
                _ => anyhow::bail!(
                    "{}",
                    locale.pick(
                        "请用 --session 指定完整的会话 ID。",
                        "Specify the full session ID with --session."
                    )
                ),
            };
            let session = SessionSelector {
                session_id: Some(id),
                claude_pid: None,
            };
            runtime(false)?.block_on(async {
                let committee = match committee {
                    Some(c) => c,
                    None => daemon_call::<ipc::BoundReply>(
                        &paths,
                        &Request::Bound {
                            session: session.clone(),
                        },
                    )
                    .await?
                    .committee
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "{}",
                            locale.pick(
                                "请指定这个会话要加入的委员会。",
                                "Specify the committee this session should join."
                            )
                        )
                    })?,
                };
                let reply: ipc::JoinReply = daemon_call(
                    &paths,
                    &Request::JoinAs {
                        session,
                        committee,
                        senator: alias.clone(),
                    },
                )
                .await?;
                anyhow::ensure!(
                    reply.alias.as_ref() == Some(&alias),
                    "{}",
                    locale.pick(
                        "daemon 未确认此别名。",
                        "The daemon did not confirm the alias."
                    )
                );
                println!(
                    "{} -> {} ({})",
                    senate_core::Address::senator(&alias).in_text(),
                    senate_core::Address::senator(&reply.agent).in_text(),
                    reply.committee
                );
                Ok(())
            })
        }
        Cmd::Join {
            committee,
            alias: None,
            ..
        } => {
            let (paths, settings) = settings_here();
            let cwd = std::env::current_dir()?;
            let (change, choice) = runtime(false)?
                .block_on(binding::join_choosing(&paths, &cwd, committee, &settings))?;
            if let senate_bridge::joinname::Choice::Derived(d) = &choice {
                println!("{}", binding::render_derived(d, settings.locale));
            }
            println!("{}", binding::render_join(&change, settings.locale));
            Ok(())
        }
        Cmd::Leave => {
            let (paths, settings) = settings_here();
            let cwd = std::env::current_dir()?;
            let change = runtime(false)?.block_on(binding::change(
                &paths,
                &cwd,
                binding::Item::Committee,
                None,
                &settings,
            ))?;
            println!("{}", binding::render_leave(&change, settings.locale));
            Ok(())
        }
        Cmd::Pin { agent } => {
            let (paths, settings) = settings_here();
            let agent = binding::parse_pin(&agent)?;
            let cwd = std::env::current_dir()?;
            let change = runtime(false)?.block_on(binding::change(
                &paths,
                &cwd,
                binding::Item::Agent,
                Some(agent.to_string()),
                &settings,
            ))?;
            println!("{}", binding::render_pin(&change, settings.locale));
            Ok(())
        }
        Cmd::Unpin => {
            let (paths, settings) = settings_here();
            let cwd = std::env::current_dir()?;
            let change = runtime(false)?.block_on(binding::change(
                &paths,
                &cwd,
                binding::Item::Agent,
                None,
                &settings,
            ))?;
            println!("{}", binding::render_unpin(&change, settings.locale));
            Ok(())
        }
        Cmd::Where => {
            let (paths, settings) = settings_here();
            let cwd = std::env::current_dir()?;
            let rt = runtime(false)?;
            let resolved = rt.block_on(binding::resolve(&paths, &cwd, &settings));
            let would = match resolved.committee {
                Some(_) => None,
                None => Some(rt.block_on(binding::would_join(&cwd))),
            };
            print!(
                "{}",
                binding::render_where(&resolved, would.as_ref(), settings.locale)
            );
            Ok(())
        }
        Cmd::List { json } => {
            let (paths, settings) = settings_here();
            let rows = binding::list(&paths);
            if json {
                print_json(&rows)
            } else {
                print!("{}", binding::render_list(&rows, settings.locale));
                Ok(())
            }
        }
        Cmd::Free { yes, delete_logs } => {
            let (paths, settings) = settings_here();
            let report = binding::free(&paths, &settings, yes, delete_logs)?;
            print!("{}", binding::render_free(&report, settings.locale));
            Ok(())
        }
        Cmd::Inbox {
            committee,
            all,
            mark_read,
        } => {
            let paths = Paths::from_env()?;
            let locale = BridgeConfig::locale_at(&paths.config());
            let text =
                runtime(false)?.block_on(inbox(&paths, committee, all, mark_read, locale))?;
            println!("{text}");
            Ok(())
        }
        Cmd::Ask {
            to,
            text,
            committee,
            wait,
            json,
        } => speak_new(SayKind::Question, to, text, committee, wait, json),
        Cmd::Tell {
            to,
            text,
            committee,
            json,
        } => speak_new(SayKind::Note, to, text, committee, None, json),
        Cmd::Request {
            to,
            text,
            committee,
            wait,
            json,
        } => speak_new(SayKind::Request, to, text, committee, wait, json),
        Cmd::Reply {
            thread,
            text,
            committee,
            json,
        } => {
            let paths = Paths::from_env()?;
            let text = message_text(text)?;
            runtime(false)?.block_on(async {
                let committee = committee_here(committee).await?;
                let (posted, locale) =
                    speak::reply(&paths.socket(), committee, Seq(thread), text).await?;
                if json {
                    print_json(&posted)
                } else {
                    println!("{}", speak::render_replied(Seq(thread), &posted, locale));
                    Ok(())
                }
            })
        }
        Cmd::Receipt {
            seq,
            committee,
            json,
        } => {
            let paths = Paths::from_env()?;
            let reply: ipc::EventReceipt = runtime(false)?.block_on(async {
                let committee = committee_here(committee).await?;
                daemon_call(
                    &paths,
                    &Request::Receipt {
                        committee,
                        seq: Seq(seq),
                    },
                )
                .await
            })?;
            if json {
                return print_json(&reply);
            }
            println!("{}", reply.render(BridgeConfig::locale_at(&paths.config())));
            Ok(())
        }
        Cmd::Thread {
            seq,
            committee,
            json,
        } => {
            let paths = Paths::from_env()?;
            runtime(false)?.block_on(async {
                let committee = committee_here(committee).await?;
                let (reply, locale) = speak::thread(&paths.socket(), committee, Seq(seq)).await?;
                if json {
                    print_json(&reply)
                } else {
                    println!("{}", speak::render_thread(&reply, locale));
                    Ok(())
                }
            })
        }
        Cmd::Watch { scope, json } => {
            let paths = Paths::from_env()?;
            runtime(false)?.block_on(async {
                let committee = scope.resolve(&paths).await?;
                watch(&paths, committee, json).await
            })
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
        Cmd::Service { command } => run_service(command),
        Cmd::Status { scope, full_ids } => {
            let paths = Paths::from_env()?;
            let (view, committee) = runtime(false)?.block_on(async {
                let scope = scope.resolve(&paths).await?;
                let view = status::collect_for(&paths, scope.as_ref()).await?;
                Ok::<_, anyhow::Error>((view, scope))
            })?;
            print!(
                "{}",
                status::render_scope(&view, chrono::Utc::now(), full_ids, committee.as_ref())
            );
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
