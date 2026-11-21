//! `senate`: the command for people and their coding agents on a developer
//! machine. It installs Senate into Claude Code and Codex, is the MCP server
//! and the hooks they run, shows what the daemon is doing and what waits for
//! the person, and writes the local profile (`senate init --local`). The
//! daemon itself is `senated` (`senated --machine` on a developer machine).

// These two stay beside this file under their own names.
#[path = "cli_help.rs"]
mod cli_help;
#[path = "cli_secretary.rs"]
mod cli_secretary;

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use crate::binding::{self, Settings};
use crate::delivery::{codex, inbox};
use crate::hook::{self, HookEnv, HookKind, HookTimeouts};
use crate::ipc::SayKind;
use crate::ipc::{
    self, DocketReply, HostedRuntime, InboxReply, Posted, Request, SessionSelector, SpawnReply,
    StopReply,
};
use crate::session::Runtime;
use crate::terminal::Terminal;
use crate::{BridgeConfig, Paths, init, install, mcp, person, preside, service, speak, status};
use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use senate_core::alias::{AliasMatch, AliasName, KnownAliases, QualifiedAlias};
use senate_core::{CommitteeId, CommitteeSeatAction, EventId, Locale, Name, SenatorName, Seq};

#[derive(Parser)]
#[command(
    name = "senate",
    version,
    about = "Reach your coding agents through Senate, and see what waits for you."
)]
struct Cli {
    /// Print every detail and full IDs (also: SENATE_VERBOSE=1).
    #[arg(
        short,
        long,
        global = true,
        env = "SENATE_VERBOSE",
        hide_env = true,
        action = clap::ArgAction::SetTrue,
        // Any value but an empty one, `0`, `false`, `no` or `off` turns it
        // on, so no setting of the variable can make a hook fail to parse.
        value_parser = clap::builder::FalseyValueParser::new()
    )]
    verbose: bool,
    #[command(subcommand)]
    command: Cmd,
}

/// How much a command prints: the everyday view, or with `--verbose`
/// everything there is.
#[derive(Clone, Copy)]
struct Output {
    terminal: Option<Terminal>,
    verbose: bool,
}

impl Output {
    /// What a person reads: senators written short unless `--verbose`, and
    /// no blank lines trailing.
    fn text(self, text: &str) -> String {
        let text = text.trim_end_matches('\n');
        if self.verbose {
            text.to_owned()
        } else {
            crate::terminal::brief_names(text)
        }
    }

    fn print(self, text: &str) {
        println!("{}", self.text(text));
    }
}

// The first paragraph of each doc comment is the one-line summary that
// `senate --help` and `-h` print; the rest is shown by `senate help <command>`.
// A new command also needs a place in `cli_help::GROUPS`.
#[derive(Subcommand)]
enum Cmd {
    /// Write the profile for using Senate alone on this machine.
    ///
    /// Writes `senate.toml` in the Senate home, with a local top Senate
    /// whose Speaker is you or one of your agents. Writes nothing else and
    /// never overwrites an existing profile. Under a team Senate, write
    /// senate.toml from config/senate.example.toml instead.
    #[command(after_help = "Examples:
  senate init --local --person alice
  senate init --local --person alice --speaker claude --locale zh")]
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
    /// Bind this worktree to a committee, or give a session an alias.
    ///
    /// The worktree form writes `.senate/committee` (untracked, kept per
    /// branch: switching branches swaps in that branch's own copy). Live
    /// sessions here pick it up at their next prompt. No daemon needed.
    ///
    /// Without a name, the committee is derived from where this is run, and
    /// printed with how: the repository's name (the last segment of the
    /// `origin` URL without `.git`; else the main worktree's folder; outside
    /// git, the directory's name), plus `-<branch>` unless on the default
    /// branch (what `origin/HEAD` points at, else main, master or trunk) or
    /// detached; lowercased, other characters becoming `-`, at most 63
    /// characters (a longer one ends in `-` and a short hash). Read-only.
    ///
    /// With --alias, one coding session gets a persistent name instead, and
    /// the worktree is left alone.
    #[command(after_help = "Examples:
  senate join                    Derive the committee from repository and branch
  senate join refund-v2          Bind this worktree to refund-v2
  senate join --alias reviewer   Inside a coding session: name it reviewer
  senate join refund-v2 --duty 'reviews the server crate'
                                 Also set this session's duty there
  senate join --alias reviewer --session <id>
                                 Name a session from another terminal
                                 (`senate status --full-ids` prints the ids)")]
    Join {
        /// The committee. With --alias, omit to use the session's current committee.
        committee: Option<CommitteeId>,
        /// Bind an alias to the primary ID of one coding session.
        #[arg(long, visible_alias = "senator")]
        alias: Option<senate_core::SenatorName>,
        /// Full session ID. Defaults to the calling coding session.
        #[arg(long, requires = "alias")]
        session: Option<String>,
        /// Also set this session's duty on the committee: one line saying
        /// what it does there. An empty string clears it. Run inside a
        /// coding session, or with --session.
        #[arg(long)]
        duty: Option<String>,
    },
    /// Watch every hosted senator's terminal, side by side.
    ///
    /// A full-screen grid of read-only views of the sessions hosted on this
    /// machine, one tile a session, in pages when they do not all fit.
    /// Nothing typed here reaches a session. Arrows or h j k l move
    /// between tiles, PgUp and PgDn between pages; Enter attaches to the
    /// tile's session (detaching comes back to the grid); q or Ctrl-]
    /// leaves.
    #[command(name = "watch-all")]
    WatchAll,
    /// List senator aliases by committee, with the IDs they stand for.
    Aliases,
    /// Manage senator aliases.
    Alias {
        #[command(subcommand)]
        command: AliasCmd,
    },
    /// Appoint session secretaries and manage their shared work.
    SecretaryTeam(cli_secretary::SecretaryTeam),
    /// Remove this worktree's committee binding on the current branch.
    Leave,
    /// List, stop or prune this machine's hosted sessions.
    ///
    /// With no subcommand: each session's name (`committee::alias`, else
    /// its senator), committee, runtime, state, uptime, who is at its
    /// terminal, and directory.
    #[command(after_help = "Examples:
  senate sessions                     List them
  senate sessions stop reviewer       Stop one (senate restore reopens it)
  senate sessions stop --idle         Stop each one nobody is attached to
  senate sessions stop --all --force  Stop all, even if typed into lately
  senate sessions prune               Stop hosts with no session")]
    Sessions {
        #[command(subcommand)]
        command: Option<SessionsCmd>,
    },
    /// Remove a senator from a committee, or seat it again.
    Seat {
        #[command(subcommand)]
        command: SeatCmd,
    },
    /// Suggest an alias for sessions in this worktree.
    ///
    /// Writes `.senate/agent` for the current branch: a session started here
    /// is offered this flat name as its initial alias.
    Pin {
        /// Initial alias, e.g. reviewer.
        agent: String,
    },
    /// Remove this worktree's alias suggestion on the current branch.
    Unpin,
    /// Show the committee and agent names this directory resolves to.
    ///
    /// Prints the committee and where it comes from, the agent a session
    /// here would speak as, the branch `.senate/` belongs to, and whether
    /// other branches have saved copies.
    Where,
    /// List saved worktree bindings, by repository.
    ///
    /// Every saved `.senate/` copy: its branch, committee, when it was
    /// saved, and whether it has expired.
    List {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Forget expired local data. Dry run without --yes.
    ///
    /// Removes saved copies whose repository, branch or detached worktree is
    /// gone; session records of sessions that ended long ago or whose
    /// process is gone; queued items and cursors of agents with no session
    /// and no contact for `free_after_days` (senate.toml, default 30). A
    /// hosted Senate's log is only listed.
    Free {
        /// Do it.
        #[arg(long)]
        yes: bool,
        /// With --yes, also delete the data of hosted Senates that
        /// `senate.toml` no longer lists: logs, which nothing else removes.
        #[arg(long, requires = "yes")]
        delete_logs: bool,
    },
    /// Show what waits for you.
    ///
    /// First what was said to you that you have not seen (an answer, a
    /// note, an escalation with its reason), then open task threads on a
    /// committee for your agents or escalated to you, and what awaits the
    /// Speaker of every Senate one of your agents presides over here.
    #[command(after_help = "Examples:
  senate inbox                Open threads on this directory's committee
  senate inbox --all          Also answered threads and idle seats
  senate inbox --mark-read    List, then mark what was new as read
  senate thread 12            Read one of the threads listed")]
    Inbox {
        /// The committee (default: the one `senate where` shows for this directory).
        #[arg(long)]
        committee: Option<CommitteeId>,
        /// Also list threads whose last word is an answer (they wait on no
        /// one) and seated senators with nothing going on.
        #[arg(long)]
        all: bool,
        /// Mark every thread listed as new for you as read, after listing
        /// it. An escalation to you keeps waiting until you act on it.
        #[arg(long)]
        mark_read: bool,
    },
    /// Ask a question.
    ///
    /// You speak as yourself. `<TO>` is a senator's name (an agent such as `claude`, or a person
    /// such as `bob`), whichever agent (`*`), `speaker`, a `senator::…`
    /// address, or a qualified name on another committee (`ibot::codex`).
    /// Text `-` is read from stdin; put `--` before text that starts with
    /// `-`. Goes to the primary Senate of this machine.
    #[command(after_help = "Examples:
  senate ask claude \"Where is the retry limit set?\"
  senate ask '*' \"Which header carries the key?\" --wait
  senate ask ibot::codex \"Is the schema frozen?\"
  git diff | senate ask reviewer -

Without --wait it returns at once: read the answer with `senate inbox`,
`senate watch` or `senate thread <seq>`. `'*'` is whichever agent is free,
`ibot::codex` an agent on another committee, `-` the text on stdin.")]
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
    /// Leave a note (no answer expected).
    ///
    /// You speak as yourself. `<TO>` as for `ask`. Text `-` is read from stdin.
    #[command(after_help = "Examples:
  senate tell codex \"The retry limit is 3 now.\"
  senate tell '*' \"main is frozen until the release is cut.\"")]
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
    /// Ask an agent to do work.
    ///
    /// You speak as yourself. `<TO>` as for `ask`. Text `-` is read from stdin. The agent reports
    /// on the thread and answers when it is done.
    #[command(after_help = "Examples:
  senate request codex \"Add a test for the empty-queue case.\"
  senate request claude \"Review the diff on this branch.\" --wait=600")]
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
    /// Say something in an existing thread.
    ///
    /// You speak as yourself: a follow-up or a correction, to whoever took part in the thread.
    #[command(after_help = "Examples:
  senate reply 12 \"No: I meant the webhook retry.\"
  senate reply 12 - < notes.txt")]
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
    /// Show how one event was recorded and delivered on this machine.
    Receipt {
        /// The event's seq.
        seq: u64,
        /// The committee (default: the one `senate where` shows for this directory).
        #[arg(long)]
        committee: Option<CommitteeId>,
        /// Print the daemon's result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print a thread: each event with who wrote it, when, and what.
    #[command(after_help = "Examples:
  senate thread 12
  senate thread 12 --committee refund-v2 --json")]
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
    /// Print what is addressed to you as it arrives.
    ///
    /// Stays in the foreground and prints answers, notes, questions and
    /// escalations (with their reason) on the committees this machine's
    /// sessions are on, or the one named. Not what is put to the Speaker:
    /// that is the agent's docket. It marks nothing as read.
    #[command(after_help = "Examples:
  senate watch
  senate watch --global
  senate watch --json")]
    Watch {
        #[command(flatten)]
        scope: DisplayScope,
        /// One JSON object per line.
        #[arg(long)]
        json: bool,
    },
    /// Take a thread over: nothing more of it goes to agents.
    Take {
        /// The thread's root seq.
        thread: u64,
        /// The committee (default: the one `senate where` shows for this directory).
        #[arg(long)]
        committee: Option<CommitteeId>,
    },
    /// Run the Senate MCP server over stdio.
    Mcp {
        /// Resolve the caller using Antigravity root conversation metadata.
        #[arg(long, conflicts_with = "acp_session")]
        agy: bool,
        /// Exact session supplied by the owning Senate ACP client.
        #[arg(long, hide = true)]
        acp_session: Option<String>,
    },
    /// Run a coding session's hook.
    ///
    /// Claude Code / Codex hook entry point; reads the hook JSON on stdin.
    /// Always exits 0.
    Hook {
        /// The hook event.
        #[arg(value_enum)]
        event: HookKind,
        /// Run as a Codex hook (the session is a Codex thread).
        #[arg(long, conflicts_with = "agy")]
        codex: bool,
        /// Run as an Antigravity SessionStart or PreInvocation hook.
        #[arg(long)]
        agy: bool,
    },
    /// Set up Claude Code and Codex. Dry run without --yes.
    ///
    /// Registers the MCP server and the session hooks. Codex is set up when
    /// `codex` is on PATH. Without --yes it prints what it would change and
    /// writes nothing.
    #[command(after_help = "Examples:
  senate install          Show what would change
  senate install --yes    Write it")]
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
    /// Keep the daemon running in the background, from login.
    ///
    /// Runs `senated --machine` without a terminal: a launchd user agent on
    /// macOS, a systemd user unit on Linux.
    #[command(after_help = "Examples:
  senate service install --yes
  senate service status
  senate service uninstall --yes")]
    Service {
        #[command(subcommand)]
        command: ServiceCmd,
    },
    /// Show the daemon, the Senate, sessions and delivery.
    ///
    /// The daemon and the Senate it is connected to, the committee's chair,
    /// each agent's delivery state, and the registered sessions. Scoped to
    /// this directory's committee unless --global or --committee is given;
    /// outside a bound directory it shows every committee.
    #[command(after_help = "Examples:
  senate status
  senate status --global
  senate status --committee refund-v2
  senate status --full-ids    Complete IDs, e.g. for `senate join --session`")]
    Status {
        #[command(flatten)]
        scope: DisplayScope,
        /// Show complete session and senator IDs.
        #[arg(long)]
        full_ids: bool,
    },
    /// Check the Codex app-server connection.
    ///
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
    /// Run a local ACP agent in a Senate-owned terminal session.
    #[command(after_help = "Examples:
  senate acp -- agy acp")]
    Acp {
        /// Command and arguments of the ACP stdio server.
        #[arg(last = true, required = true, num_args = 1..)]
        command: Vec<std::ffi::OsString>,
    },
    /// Start a coding agent in the background as a senator.
    ///
    /// Starts the Codex or Claude Code TUI in a Senate-owned terminal,
    /// seats it on the committee and prints its senator name. Open its
    /// terminal with `senate attach`.
    #[command(after_help = "Examples:
  senate spawn claude                    On this directory's committee
  senate spawn codex --alias reviewer    With a name to address it by
  senate spawn claude --committee refund-v2 --dir ../refund
  senate spawn codex --prompt 'Review the open PR'
                                         Send it a first request once seated
  senate attach reviewer                 Open its terminal (Ctrl-] detaches)")]
    Spawn {
        /// The coding agent to start.
        runtime: HostedRuntime,
        /// The committee to seat it on (default: the one `senate where`
        /// shows for this directory).
        #[arg(long)]
        committee: Option<CommitteeId>,
        /// A name to address the senator by, e.g. reviewer.
        #[arg(long)]
        alias: Option<SenatorName>,
        /// The directory where the coding session will run.
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        /// A first instruction for the new senator, sent once it is seated
        /// as an ordinary Senate request from you. It is never typed into
        /// the terminal.
        #[arg(long, conflicts_with = "prompt_file")]
        prompt: Option<String>,
        /// Like --prompt, read from a file.
        #[arg(long, value_name = "PATH")]
        prompt_file: Option<PathBuf>,
    },
    /// Open a hosted senator's terminal. Press Ctrl-] to detach.
    ///
    /// After /new or /clear in hosted Codex, Senate still delivers to the
    /// original thread, which the TUI no longer shows. Exit that TUI and use
    /// senate restore to return. A Codex session that was live before restore
    /// stays live on host close because a native client may remain.
    ///
    /// Hosted Claude keeps the senator when the same process reports a new
    /// session after /clear or /resume. A refused change appears in status.
    /// The configured executable must be Claude itself or a wrapper that execs it.
    ///
    /// With --read-only the terminal is only shown: nothing typed reaches
    /// the session, its size is left as it is, and any number of people can
    /// look at once, beside whoever is attached. Ctrl-] leaves.
    Attach {
        /// The senator's primary name or alias (`committee::alias` for an
        /// alias on one committee). Without one, choose a running session
        /// from a list.
        senator: Option<AliasName>,
        /// Look without typing: every key but Ctrl-] is dropped.
        #[arg(long)]
        read_only: bool,
    },
    /// Resume a stopped hosted senator in the background.
    ///
    /// The recorded session reopens in a detached Senate-owned terminal. A
    /// hosted Claude session with no saved conversation opens with its
    /// original ID.
    Restore {
        /// The senator's primary name or alias (`committee::alias` for an
        /// alias on one committee).
        senator: AliasName,
    },
    /// End a hosted senator's terminal (`senate restore` resumes it).
    Stop {
        /// The senator's primary name or alias (`committee::alias` for an
        /// alias on one committee).
        senator: AliasName,
        /// Stop even when the attached viewer has typed in the past three minutes.
        #[arg(long)]
        force: bool,
    },
    #[command(hide = true)]
    SessionHost {
        id: EventId,
        runtime: HostedRuntime,
        cwd: PathBuf,
        session_id: String,
        #[arg(long)]
        program: PathBuf,
        #[arg(long)]
        resume: bool,
        #[arg(long)]
        codex_remote: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
    },
    /// Enable live push for a running Antigravity conversation.
    ///
    /// Reads the conversation's local API token from stdin.
    AgyRegister {
        /// The conversation's UUID.
        conversation: String,
        /// Antigravity's HTTP language-server endpoint on loopback.
        endpoint: std::net::SocketAddr,
    },
    /// Close the Senate registration of an Antigravity conversation.
    AgyUnregister {
        /// The conversation's UUID.
        conversation: String,
    },
    /// Manual checks of the delivery plumbing.
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
    /// Write the service file and load the service. Dry run without --yes.
    ///
    /// The service runs `senated --machine` from login, restarts it if it
    /// dies and logs to `senated.log` in the Senate home.
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
    /// Show whether the service is installed, loaded and running, and where it logs.
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
enum AliasCmd {
    /// Remove an alias. A bare alias on several committees asks which;
    /// `committee::alias` names one.
    #[command(after_help = "Examples:
  senate alias rm smoke          Remove the alias smoke
  senate alias rm nous::main     Remove main from the committee nous")]
    Rm {
        /// The alias, or `committee::alias`.
        alias: AliasName,
    },
}

#[derive(Subcommand)]
enum SessionsCmd {
    /// End hosted sessions' terminals (`senate restore` reopens one).
    ///
    /// Each TUI is asked to end, then killed. A session someone typed into
    /// in the last 5 minutes is skipped unless --force.
    Stop {
        /// Sessions by name (`committee::alias` or senator name).
        #[arg(required_unless_present_any = ["idle", "all"])]
        names: Vec<String>,
        /// Every running session nobody is attached to.
        #[arg(long, conflicts_with_all = ["names", "all"])]
        idle: bool,
        /// Every running session.
        #[arg(long, conflicts_with = "names")]
        all: bool,
        /// Stop even a session typed into in the last 5 minutes, or with a
        /// viewer attending it.
        #[arg(long)]
        force: bool,
    },
    /// Stop terminal hosts that no session owns, and clear ended hosts'
    /// leftovers.
    Prune,
}

#[derive(Subcommand)]
enum SeatCmd {
    /// Remove the senator's seat and end its connection on this committee.
    Unseat(SeatArgs),
    /// Seat the senator on the committee again under the same senator id.
    Join(SeatArgs),
}

#[derive(Args)]
struct SeatArgs {
    /// The committee the seat is on.
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
fn read_stdin(timeout: Duration) -> Result<Option<String>, std::sync::mpsc::RecvTimeoutError> {
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = std::thread::Builder::new().spawn(move || {
        let mut s = String::new();
        let r = std::io::stdin().read_to_string(&mut s).map(|_| s);
        // The receiver is gone if we already timed out; nothing to do then.
        let _ = tx.send(r);
    });
    rx.recv_timeout(timeout).map(Result::ok)
}

/// Hooks never fail and never print anything but their one JSON line.
fn run_hook(kind: HookKind, runtime_override: Option<Runtime>) {
    let stdin = match read_stdin(hook::STDIN_BUDGET) {
        Ok(Some(stdin)) => stdin,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            hook::report_stdin_timeout(kind, runtime_override);
            return;
        }
        Ok(None) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
    };
    let Ok(paths) = Paths::from_env() else {
        return;
    };
    let Ok(rt) = runtime(false) else {
        return;
    };
    let env = if runtime_override == Some(Runtime::Agy) {
        HookEnv {
            token: std::env::var(crate::delivery::agy::PROOF_ENV).ok(),
            ..HookEnv::default()
        }
    } else {
        HookEnv::from_process()
    };
    let hosted_claude_start = kind == HookKind::SessionStart
        && runtime_override.is_none_or(|runtime| runtime == Runtime::Claude)
        && std::env::var_os(crate::host::HOSTED_CLAUDE_ENV).is_some();
    let mut timeouts = HookTimeouts::default();
    if hosted_claude_start {
        // The installed native hook has a 10s limit. Reserve time for IPC
        // connect, gathering git facts, and writing its brief back.
        timeouts.request = Duration::from_secs(8);
    }
    let out = rt.block_on(async {
        tokio::time::timeout(
            if hosted_claude_start {
                Duration::from_millis(9_500)
            } else {
                Duration::from_secs(4)
            },
            hook::run_as(
                kind,
                &paths.socket(),
                &stdin,
                &env,
                runtime_override,
                timeouts,
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

/// The hosted sessions of this machine whose terminal host is running, as
/// `senate watch-all` shows them: `senate sessions`' rows, each named as
/// a command takes it.
async fn hosted_sessions(paths: &Paths) -> anyhow::Result<Vec<crate::host::view::Session>> {
    let reply: crate::ipc::SessionsReply = daemon_call(paths, &Request::Sessions).await?;
    // Only a running host can be watched; the rows come ordered by name.
    Ok(reply
        .rows
        .into_iter()
        .filter(crate::ipc::SessionRow::is_running)
        .map(|row| crate::host::view::Session {
            key: row.session_id.clone(),
            title: format!(
                "{} · {} · {} · {}",
                row.label(),
                row.committee
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), ToString::to_string),
                row.runtime.as_str(),
                row.state.as_str()
            ),
            // What a command takes: `committee::alias`, so it names one
            // session whatever other committees call theirs.
            alias: row.label(),
            dim: !row.is_running(),
            tty: row.tty,
        })
        .collect())
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

/// Open the joined committee on the server when it has no event there
/// yet, so sessions can be spawned on it; the line to print. Never fails
/// the join: without the daemon or the server it says when the committee
/// opens instead.
async fn open_committee(paths: &Paths, committee: CommitteeId, locale: Locale) -> String {
    let request = Request::OpenCommittee {
        committee: committee.clone(),
    };
    let opened = tokio::time::timeout(
        Duration::from_secs(10),
        ipc::request::<ipc::OpenCommitteeReply>(
            &paths.socket(),
            &request,
            Duration::from_secs(1),
            Duration::from_secs(8),
        ),
    )
    .await;
    match opened {
        Ok(Ok(ipc::OpenCommitteeReply { opened: Some(seq) })) => locale.pick(
            format!("已在服务器上开设委员会 {committee}（seq {seq}）。"),
            format!("Opened committee {committee} on the server (seq {seq})."),
        ),
        Ok(Ok(ipc::OpenCommitteeReply { opened: None })) => locale.pick(
            format!("服务器上已有委员会 {committee}。"),
            format!("Committee {committee} is already open on the server."),
        ),
        Ok(Err(error)) => {
            tracing::debug!(%error, %committee, "opening the joined committee on the server");
            not_open_yet(&committee, locale)
        }
        Err(_) => not_open_yet(&committee, locale),
    }
}

fn not_open_yet(committee: &CommitteeId, locale: Locale) -> String {
    locale.pick(
        format!(
            "暂时连不上 Senate 服务器，委员会 {committee} 还没在服务器上开设：下次有会话在那里启动，\
             或再运行一次 `senate join {committee}`，它就会开设。"
        ),
        format!(
            "Could not reach the Senate server, so committee {committee} is not open there yet: it \
             opens the next time a session starts there, or when you run `senate join {committee}` \
             again."
        ),
    )
}

fn host_call<T: serde::de::DeserializeOwned>(request: &Request) -> anyhow::Result<T> {
    let paths = Paths::from_env()?;
    Ok(runtime(false)?.block_on(ipc::request(
        &paths.socket(),
        request,
        Duration::from_secs(1),
        crate::host::REQUEST_TIMEOUT,
    ))?)
}

/// A host starts with nobody attached. Leave it what this terminal looks
/// like, so the TUI's first questions about colours have an answer.
fn remember_terminal() -> anyhow::Result<()> {
    let paths = Paths::from_env()?;
    runtime(false)?.block_on(crate::host::remember_terminal(&paths));
    Ok(())
}

/// `spawn` and `restore`: stdout carries the senator's name alone, for a
/// script to capture; a person at a terminal is also told the next step.
/// What an interactive command names: a bare alias several agents have is
/// the person's to pick among its candidates, even when one is on their
/// own committee. Anything else goes to the daemon as written.
fn pick_alias(name: AliasName) -> anyhow::Result<AliasName> {
    if name.committee.is_some() || name.alias.is_session_name() {
        return Ok(name);
    }
    let paths = Paths::from_env()?;
    let known: KnownAliases = runtime(false)?.block_on(daemon_call(&paths, &Request::Aliases))?;
    match known.resolve(&name, None) {
        AliasMatch::Ambiguous { candidates, .. } => {
            let chosen = choose_alias(&name, candidates)?;
            Ok(AliasName::bare(chosen.primary))
        }
        _ => Ok(name),
    }
}

/// `senate sessions stop`: stops each chosen session as `senate stop`
/// does, one after another, and says what it left and why. Fails when
/// any stop failed.
async fn stop_sessions(
    paths: &Paths,
    selector: &crate::sessions::StopSelector,
    force: bool,
    locale: Locale,
) -> anyhow::Result<()> {
    let reply: crate::ipc::SessionsReply = daemon_call(paths, &Request::Sessions).await?;
    let crate::sessions::StopPlan { stop, kept } =
        crate::sessions::select_for_stop(&reply.rows, selector, force)
            .map_err(anyhow::Error::msg)?;
    for (row, ago) in &kept {
        eprintln!(
            "{}",
            locale.pick(
                format!(
                    "跳过 {}：{} 秒前有人在它的终端输入（加 --force 也停止）",
                    row.label(),
                    ago
                ),
                format!(
                    "skipped {}: someone typed into it {ago}s ago (--force stops it too)",
                    row.label()
                ),
            )
        );
    }
    let mut failed = 0;
    for row in &stop {
        let request = Request::Stop {
            session: SessionSelector::default(),
            senator: AliasName::bare(row.primary.clone()),
            force,
        };
        let result: Result<StopReply, _> = ipc::request(
            &paths.socket(),
            &request,
            Duration::from_secs(1),
            crate::host::REQUEST_TIMEOUT,
        )
        .await;
        match result {
            Ok(_) => println!(
                "{}",
                locale.pick(
                    format!("已停止 {}", row.label()),
                    format!("stopped {}", row.label())
                )
            ),
            Err(error) => {
                failed += 1;
                eprintln!("{}: {error}", row.label());
            }
        }
    }
    if stop.is_empty() && kept.is_empty() {
        println!(
            "{}",
            locale.pick("没有要停止的会话。", "No session to stop.")
        );
    }
    anyhow::ensure!(failed == 0, "{failed} session(s) could not be stopped");
    Ok(())
}

/// `senate attach` with no name: the running hosted sessions, in the
/// picker; `None` when the person cancels.
fn pick_running_session() -> anyhow::Result<Option<SenatorName>> {
    let paths = Paths::from_env()?;
    let rt = runtime(false)?;
    let reply: crate::ipc::SessionsReply = rt.block_on(daemon_call(&paths, &Request::Sessions))?;
    let running: Vec<crate::ipc::SessionRow> =
        reply.rows.into_iter().filter(|r| r.is_running()).collect();
    anyhow::ensure!(
        !running.is_empty(),
        "no hosted session is running; `senate spawn` starts one, `senate sessions` lists them"
    );
    rt.block_on(crate::host::pick_session(
        &running,
        "Attach to which session?",
    ))
}

/// Let the person pick one of an alias's `candidates`: on a terminal,
/// in the picker, each line with what `senate sessions` knows of that
/// senator here; elsewhere, an error listing them to name one.
fn choose_alias(
    name: &AliasName,
    candidates: Vec<QualifiedAlias>,
) -> anyhow::Result<QualifiedAlias> {
    use std::io::IsTerminal as _;
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        let listed: Vec<String> = candidates
            .iter()
            .map(|q| {
                format!(
                    "  {q} -> {}",
                    senate_core::Address::senator(&q.primary).in_text()
                )
            })
            .collect();
        anyhow::bail!(
            "{name} is an alias on several committees; name one as committee::alias:\n{}",
            listed.join("\n")
        );
    }
    let paths = Paths::from_env()?;
    let rt = runtime(false)?;
    // Sessions this machine hosts say more; any other senator is listed by
    // its alias alone.
    let rows = rt
        .block_on(daemon_call::<crate::ipc::SessionsReply>(
            &paths,
            &Request::Sessions,
        ))
        .map(|reply| reply.rows)
        .unwrap_or_default();
    let choices: Vec<crate::host::Choice> = candidates
        .iter()
        .map(|q| match rows.iter().find(|row| row.primary == q.primary) {
            Some(row) => crate::host::Choice {
                label: q.to_string(),
                ..crate::host::Choice::of_row(row)
            },
            None => crate::host::Choice {
                label: q.to_string(),
                detail: format!(
                    "{} · not hosted here",
                    senate_core::Address::senator(&q.primary).in_text()
                ),
                primary: q.primary.clone(),
                dim: false,
            },
        })
        .collect();
    let prompt = format!("{name} is an alias on several committees: which one?");
    match rt.block_on(crate::host::pick(&choices, &prompt))? {
        Some(primary) => candidates
            .into_iter()
            .find(|q| q.primary == primary)
            .ok_or_else(|| anyhow::anyhow!("the choice is not one of {name}'s")),
        None => anyhow::bail!("cancelled"),
    }
}

fn launch(request: &Request, terminal: Option<Terminal>) -> anyhow::Result<()> {
    let reply: SpawnReply = host_call(request)?;
    println!("{}", reply.senator);
    let locale = Paths::from_env()
        .map(|p| BridgeConfig::locale_at(&p.config()))
        .unwrap_or_default();
    if let Some(seq) = reply.request {
        eprintln!(
            "{}",
            locale.pick(
                format!("已把提示作为请求发给它（{} seq {seq}）。", reply.committee),
                format!(
                    "Sent it the prompt as a request ({} seq {seq}).",
                    reply.committee
                ),
            )
        );
    }
    if terminal.is_some() {
        eprintln!(
            "{}",
            locale.pick(
                format!(
                    "已在后台运行。用 `senate attach {}` 打开它的终端（Ctrl-] 脱离）。",
                    reply.senator
                ),
                format!(
                    "Running in the background. Open its terminal with `senate attach {}` (Ctrl-] detaches).",
                    reply.senator
                ),
            )
        );
    }
    Ok(())
}

async fn inbox(
    paths: &Paths,
    committee: Option<CommitteeId>,
    all: bool,
    mark_read: bool,
    locale: Locale,
    verbose: bool,
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
    let mut docket: DocketReply = daemon_call(
        paths,
        &Request::Docket {
            session: SessionSelector::default(),
            committee: Some(committee),
        },
    )
    .await?;
    let awaiting: usize = docket
        .senates
        .iter()
        .flat_map(|senate| &senate.pages)
        .map(|page| page.items.len())
        .sum();
    // `--all` and `--verbose` both show every seat; only `--all` adds the
    // answered threads.
    let all = all || verbose;
    if !docket.senates.is_empty() && !all && awaiting == 0 {
        // Day to day, who is seated is `senate status`'s to say.
        out.push('\n');
        out.push_str(locale.pick("没有等待 Speaker 的事项。", "Nothing awaits the Speaker."));
    } else if !docket.senates.is_empty() {
        let idle = if all { 0 } else { drop_idle_seats(&mut docket) };
        out.push_str("\n\n");
        out.push_str(&preside::render_docket(&docket, locale));
        if idle > 0 {
            out.push_str(&locale.pick(
                format!(
                    "另有 {idle} 个席位没有会话、checkpoint 和未结 thread，未列出；--all 会列出。\n"
                ),
                format!(
                    "{idle} more seat(s) with no session, checkpoint or open thread are not listed; --all lists them.\n"
                ),
            ));
        }
    }
    Ok(out)
}

/// Leave out the seats nothing is happening on (no live session, no
/// checkpoint to cover from, no open thread) and say how many they were: a
/// committee keeps a seat for every session that ever joined it.
fn drop_idle_seats(docket: &mut DocketReply) -> usize {
    let mut idle = 0;
    for page in docket.senates.iter_mut().flat_map(|s| s.pages.iter_mut()) {
        let before = page.seats.len();
        page.seats
            .retain(|seat| seat.connected || seat.checkpoint.is_some() || seat.open_threads > 0);
        idle += before - page.seats.len();
    }
    idle
}

/// `senate watch`: what is addressed to the person, as it arrives.
async fn watch(
    paths: &Paths,
    committee: Option<CommitteeId>,
    json: bool,
    output: Output,
) -> anyhow::Result<()> {
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
                output.text(&person::render_news(news, *escalated, text, pong.locale))
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
    output: Output,
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
            output.print(&sent.render(false));
            return Ok(());
        };
        if !json {
            output.print(&sent.render(true));
            std::io::stdout().flush()?;
        }
        match speak::wait_answer(&socket, &sent, Duration::from_secs(secs)).await? {
            Some(answer) if json => {
                print_json(&serde_json::json!({ "posted": sent.posted, "answer": answer }))
            }
            Some(answer) => {
                output.print(&speak::render_arrival(&answer, sent.locale));
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

fn run_service(command: ServiceCmd, output: Output) -> anyhow::Result<()> {
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
            if output.verbose {
                print!("{}", service::render_status(&status, locale));
            } else {
                print!(
                    "{}",
                    service::render_status_brief(&status, locale, output.terminal)
                );
            }
            if spec.platform == service::Platform::Linux
                && status.file_exists
                && std::fs::read_to_string(&status.file)
                    .is_ok_and(|unit| !unit.lines().any(|line| line.trim() == "KillMode=process"))
            {
                println!("{}", locale.pick(
                    "请重新运行 senate service install --yes，以便托管终端在 systemd 重启 Senate 守护进程时继续运行。",
                    "Run senate service install --yes to update the systemd unit so hosted terminals survive a daemon restart.",
                ));
            }
        }
    }
    Ok(())
}

fn run(command: Cmd, output: Output) -> anyhow::Result<()> {
    let Output { terminal, verbose } = output;
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
        Cmd::Sessions { command } => {
            let paths = Paths::from_env()?;
            let locale = BridgeConfig::locale_at(&paths.config());
            let rt = runtime(false)?;
            match command {
                None => {
                    let reply: crate::ipc::SessionsReply =
                        rt.block_on(daemon_call(&paths, &Request::Sessions))?;
                    println!(
                        "{}",
                        crate::sessions::render(&reply, chrono::Utc::now(), locale)
                    );
                    Ok(())
                }
                Some(SessionsCmd::Stop {
                    names,
                    idle,
                    all,
                    force,
                }) => {
                    let selector = if all {
                        crate::sessions::StopSelector::All
                    } else if idle {
                        crate::sessions::StopSelector::Idle
                    } else {
                        crate::sessions::StopSelector::Named(names)
                    };
                    rt.block_on(stop_sessions(&paths, &selector, force, locale))
                }
                Some(SessionsCmd::Prune) => {
                    let reply: crate::ipc::PruneReply = rt.block_on(ipc::request(
                        &paths.socket(),
                        &Request::PruneHosts,
                        Duration::from_secs(1),
                        crate::host::REQUEST_TIMEOUT,
                    ))?;
                    for id in &reply.stopped {
                        println!(
                            "{}",
                            locale.pick(
                                format!("已停止孤立的终端宿主 {id}"),
                                format!("stopped orphaned host {id}"),
                            )
                        );
                    }
                    println!(
                        "{}",
                        locale.pick(
                            format!(
                                "停止了 {} 个孤立宿主，清理了 {} 个已结束宿主的目录。",
                                reply.stopped.len(),
                                reply.removed.len()
                            ),
                            format!(
                                "Stopped {} orphaned host(s); cleared {} ended host director(ies).",
                                reply.stopped.len(),
                                reply.removed.len()
                            ),
                        )
                    );
                    Ok(())
                }
            }
        }
        Cmd::WatchAll => {
            let paths = Paths::from_env()?;
            runtime(false)?.block_on(crate::host::view::watch_all(&paths, || {
                let paths = paths.clone();
                Box::pin(async move { hosted_sessions(&paths).await })
            }))
        }
        Cmd::Aliases => {
            let paths = Paths::from_env()?;
            let locale = BridgeConfig::locale_at(&paths.config());
            let known: KnownAliases =
                runtime(false)?.block_on(daemon_call(&paths, &Request::Aliases))?;
            let rows: Vec<(String, SenatorName)> = match &known {
                KnownAliases::Scoped(table) => {
                    table.iter().map(|q| (q.to_string(), q.primary)).collect()
                }
                KnownAliases::Unscoped(map) => {
                    eprintln!(
                        "{}",
                        locale.pick(
                            "这个 Senate 的服务器较旧：别名不分委员会，在整个 Senate 内有效。",
                            "This Senate's server is older: its aliases are Senate-wide, not per committee.",
                        )
                    );
                    map.iter()
                        .map(|(alias, primary)| (alias.to_string(), primary.clone()))
                        .collect()
                }
            };
            if rows.is_empty() {
                println!(
                    "{}",
                    locale.pick("尚无 senator 别名。", "No senator aliases registered.")
                );
            }
            for (alias, primary) in rows {
                output.print(&format!(
                    "{alias} -> {}",
                    senate_core::Address::senator(&primary).in_text()
                ));
            }
            Ok(())
        }
        Cmd::Alias {
            command: AliasCmd::Rm { alias },
        } => {
            let paths = Paths::from_env()?;
            let rt = runtime(false)?;
            let known: KnownAliases = rt.block_on(daemon_call(&paths, &Request::Aliases))?;
            // What goes: (shown name, primary, committee). An older
            // server's alias is on no committee.
            let target: (String, SenatorName, Option<CommitteeId>) = match &known {
                KnownAliases::Unscoped(map) => match map.get(&alias.alias) {
                    Some(primary) => (alias.alias.to_string(), primary.clone(), None),
                    None => anyhow::bail!("no alias {alias}"),
                },
                KnownAliases::Scoped(table) => {
                    let found: Vec<QualifiedAlias> = match &alias.committee {
                        Some(committee) => table
                            .get(committee, &alias.alias)
                            .map(|primary| QualifiedAlias {
                                committee: committee.clone(),
                                alias: alias.alias.clone(),
                                primary: primary.clone(),
                            })
                            .into_iter()
                            .collect(),
                        None => table.matches(&alias.alias),
                    };
                    let one = match found.as_slice() {
                        [] => anyhow::bail!("no alias {alias}"),
                        [one] => one.clone(),
                        _ => choose_alias(&alias, found)?,
                    };
                    (one.to_string(), one.primary, Some(one.committee))
                }
            };
            let (shown, primary, committee) = target;
            let _: serde_json::Value = rt.block_on(daemon_call(
                &paths,
                &Request::RemoveAlias {
                    primary: primary.clone(),
                    alias: alias.alias.clone(),
                    committee,
                },
            ))?;
            output.print(&format!(
                "removed {shown} -> {}",
                senate_core::Address::senator(&primary).in_text()
            ));
            Ok(())
        }
        Cmd::SecretaryTeam(args) => cli_secretary::run(args),
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
                CommitteeSeatAction::Unseat => output.print(&locale.pick(
                    format!("{seat} 已从 {} 除席（seq {}）", reply.committee, reply.seq),
                    format!(
                        "{seat} unseated from {} (seq {})",
                        reply.committee, reply.seq
                    ),
                )),
                CommitteeSeatAction::Join => output.print(&locale.pick(
                    format!("{seat} 已加入 {}（seq {}）", reply.committee, reply.seq),
                    format!("{seat} joined {} (seq {})", reply.committee, reply.seq),
                )),
            }
            Ok(())
        }
        Cmd::Join {
            committee,
            alias: Some(alias),
            session,
            duty,
        } => {
            let paths = Paths::from_env()?;
            let locale = BridgeConfig::locale_at(&paths.config());
            let session = calling_session(session, locale)?;
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
                        session: session.clone(),
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
                output.print(&format!(
                    "{} -> {} ({})",
                    senate_core::Address::senator(&alias).in_text(),
                    senate_core::Address::senator(&reply.agent).in_text(),
                    reply.committee
                ));
                if let Some(duty) = duty {
                    let line = set_duty(&paths, session, reply.committee, duty, locale).await?;
                    output.print(&line);
                }
                Ok(())
            })
        }
        Cmd::Join {
            committee,
            alias: None,
            duty,
            ..
        } => {
            let (paths, settings) = settings_here();
            let cwd = std::env::current_dir()?;
            // The session is found before anything is written, so a duty
            // that cannot be set leaves the worktree as it was.
            let session = duty
                .as_ref()
                .map(|_| calling_session(None, settings.locale))
                .transpose()?;
            let rt = runtime(false)?;
            let (change, choice) =
                rt.block_on(binding::join_choosing(&paths, &cwd, committee, &settings))?;
            if let crate::joinname::Choice::Derived(d) = &choice {
                println!("{}", binding::render_derived(d, settings.locale));
            }
            println!("{}", binding::render_join(&change, settings.locale));
            if let Some(committee) = &change.value {
                let committee = CommitteeId::new(committee)?;
                println!(
                    "{}",
                    rt.block_on(open_committee(&paths, committee, settings.locale))
                );
            }
            if let (Some(duty), Some(session), Some(committee)) = (duty, session, &change.value) {
                let committee = CommitteeId::new(committee)?;
                let line =
                    rt.block_on(set_duty(&paths, session, committee, duty, settings.locale))?;
                println!("{line}");
            }
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
                if verbose {
                    binding::render_where(&resolved, would.as_ref(), settings.locale)
                } else {
                    binding::render_where_brief(
                        &resolved,
                        would.as_ref(),
                        settings.locale,
                        terminal,
                    )
                }
            );
            Ok(())
        }
        Cmd::List { json } => {
            let (paths, settings) = settings_here();
            let rows = binding::list(&paths);
            if json {
                print_json(&rows)
            } else if verbose {
                print!("{}", binding::render_list(&rows, settings.locale));
                Ok(())
            } else {
                print!(
                    "{}",
                    binding::render_list_brief(&rows, settings.locale, terminal)
                );
                Ok(())
            }
        }
        Cmd::Free { yes, delete_logs } => {
            let (paths, settings) = settings_here();
            let report = binding::free(&paths, &settings, yes, delete_logs)?;
            print!(
                "{}",
                if verbose {
                    binding::render_free(&report, settings.locale)
                } else {
                    binding::render_free_brief(&report, settings.locale, terminal)
                }
            );
            Ok(())
        }
        Cmd::Inbox {
            committee,
            all,
            mark_read,
        } => {
            let paths = Paths::from_env()?;
            let locale = BridgeConfig::locale_at(&paths.config());
            let text = runtime(false)?
                .block_on(inbox(&paths, committee, all, mark_read, locale, verbose))?;
            output.print(&text);
            Ok(())
        }
        Cmd::Ask {
            to,
            text,
            committee,
            wait,
            json,
        } => speak_new(SayKind::Question, to, text, committee, wait, json, output),
        Cmd::Tell {
            to,
            text,
            committee,
            json,
        } => speak_new(SayKind::Note, to, text, committee, None, json, output),
        Cmd::Request {
            to,
            text,
            committee,
            wait,
            json,
        } => speak_new(SayKind::Request, to, text, committee, wait, json, output),
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
                    output.print(&speak::render_replied(Seq(thread), &posted, locale));
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
            output.print(&reply.render(BridgeConfig::locale_at(&paths.config())));
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
                    output.print(&speak::render_thread(&reply, locale));
                    Ok(())
                }
            })
        }
        Cmd::Watch { scope, json } => {
            let paths = Paths::from_env()?;
            runtime(false)?.block_on(async {
                let committee = scope.resolve(&paths).await?;
                watch(&paths, committee, json, output).await
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
            output.print(&match locale {
                Locale::Zh => format!(
                    "你接管了 {} 上的 thread {thread}（seq {}）。",
                    posted.committee, posted.seq
                ),
                Locale::En => format!(
                    "You took thread {thread} on {} over (seq {}).",
                    posted.committee, posted.seq
                ),
            });
            Ok(())
        }
        Cmd::Acp { command } => {
            let paths = Paths::from_env()?;
            runtime(true)?.block_on(crate::delivery::acp::run(
                &paths,
                &std::env::current_dir()?,
                &command,
            ))
        }
        Cmd::Spawn {
            runtime: harness,
            committee,
            alias,
            dir,
            prompt,
            prompt_file,
        } => {
            let prompt = match prompt_file {
                Some(path) => Some(
                    std::fs::read_to_string(&path)
                        .with_context(|| format!("read the prompt file {}", path.display()))?,
                ),
                None => prompt,
            };
            if prompt.as_deref().is_some_and(|p| p.trim().is_empty()) {
                anyhow::bail!("the prompt is empty");
            }
            let committee = runtime(false)?.block_on(committee_here(committee))?;
            remember_terminal()?;
            launch(
                &Request::Spawn {
                    runtime: harness,
                    committee,
                    chair: false,
                    alias,
                    cwd: std::path::absolute(dir)?,
                    session: SessionSelector::default(),
                    prompt,
                },
                terminal,
            )
        }
        Cmd::Restore { senator } => {
            let senator = pick_alias(senator)?;
            remember_terminal()?;
            launch(
                &Request::Restore {
                    senator,
                    session: SessionSelector::default(),
                },
                terminal,
            )
        }
        Cmd::Stop { senator, force } => {
            let senator = pick_alias(senator)?;
            let reply: StopReply = host_call(&Request::Stop {
                session: SessionSelector::default(),
                senator,
                force,
            })?;
            println!("{}", reply.senator);
            if reply.audit_pending {
                eprintln!("The stop note is queued for the committee log.");
            }
            Ok(())
        }
        Cmd::Attach { senator, read_only } => {
            let senator = match senator {
                Some(senator) => pick_alias(senator)?,
                None => match pick_running_session()? {
                    Some(primary) => AliasName::bare(primary),
                    None => return Ok(()),
                },
            };
            let paths = Paths::from_env()?;
            runtime(false)?.block_on(async {
                let request = Request::Attach {
                    senator: senator.clone(),
                };
                let socket: PathBuf = daemon_call(&paths, &request).await?;
                if read_only {
                    let alias = senator.to_string();
                    return crate::host::view::watch_one(crate::host::view::Session {
                        title: alias.clone(),
                        key: alias.clone(),
                        alias,
                        tty: socket,
                        dim: false,
                    })
                    .await;
                }
                crate::host::attach(&socket, &paths).await
            })
        }
        Cmd::SessionHost {
            id,
            runtime: harness,
            cwd,
            session_id,
            program,
            resume,
            codex_remote,
            name,
            model,
            effort,
        } => {
            let paths = Paths::from_env()?;
            let model = crate::ipc::HostedModel { model, effort };
            runtime(false)?.block_on(crate::host::run(
                &paths,
                &id,
                harness,
                &cwd,
                &crate::host::SessionCommand {
                    session_id: &session_id,
                    program: &program,
                    resume,
                    codex_remote: codex_remote.as_deref(),
                    name: name.as_deref(),
                    model: &model,
                },
            ))
        }
        Cmd::AgyRegister {
            conversation,
            endpoint,
        } => {
            let paths = Paths::from_env()?;
            let mut token = String::new();
            std::io::stdin().take(515).read_to_string(&mut token)?;
            let endpoint = crate::delivery::agy::Endpoint {
                address: endpoint,
                token: token.trim_end_matches(['\r', '\n']).to_owned(),
            };
            endpoint.validate()?;
            let reply: serde_json::Value = runtime(false)?.block_on(daemon_call(
                &paths,
                &Request::RegisterAgy {
                    session_id: conversation,
                    endpoint,
                },
            ))?;
            println!("{reply}");
            Ok(())
        }
        Cmd::AgyUnregister { conversation } => {
            let paths = Paths::from_env()?;
            let key = crate::session::agy_session_key(&conversation)
                .context("invalid Antigravity conversation UUID")?;
            let _: serde_json::Value = runtime(false)?.block_on(daemon_call(
                &paths,
                &Request::SessionEnd {
                    session_id: key,
                    reason: None,
                },
            ))?;
            Ok(())
        }
        Cmd::Mcp { agy, acp_session } => {
            init_logging("senate_bridge=warn");
            let paths = Paths::from_env()?;
            let locale = BridgeConfig::locale_at(&paths.config());
            let (harness, selector) = if agy {
                (Runtime::Agy, SessionSelector::default())
            } else if let Some(session_id) = acp_session {
                anyhow::ensure!(
                    Runtime::Acp.native_id(&session_id).is_some(),
                    "invalid ACP session key"
                );
                (
                    Runtime::Acp,
                    SessionSelector {
                        session_id: Some(session_id),
                        claude_pid: None,
                    },
                )
            } else {
                (Runtime::Claude, mcp::selector_from_env())
            };
            runtime(false)?.block_on(mcp::serve_stdio(paths.socket(), selector, locale, harness))
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
            // Applying is rare and its by-hand steps matter: say it all.
            if apply || verbose {
                print!("{}", install::render(&plan, &opts, apply));
            } else {
                print!("{}", install::render_brief(&plan, &opts, terminal));
            }
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
        Cmd::Service { command } => run_service(command, output),
        Cmd::Status { scope, full_ids } => {
            let paths = Paths::from_env()?;
            let (view, committee) = runtime(false)?.block_on(async {
                let scope = scope.resolve(&paths).await?;
                let view = status::collect_for(&paths, scope.as_ref()).await?;
                Ok::<_, anyhow::Error>((view, scope))
            })?;
            let now = chrono::Utc::now();
            let text = match (verbose, terminal) {
                (true, Some(terminal)) => {
                    status::render_terminal(&view, now, full_ids, committee.as_ref(), terminal)
                }
                (true, None) => status::render_scope(&view, now, full_ids, committee.as_ref()),
                // Piped, the everyday view is the terminal's without colour.
                (false, _) => status::render_brief(
                    &view,
                    now,
                    full_ids,
                    committee.as_ref(),
                    terminal.unwrap_or(Terminal::new(100, false)),
                ),
            };
            print!("{text}");
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

/// What went wrong and, when the daemon cannot be reached, what to do next.
fn describe_error(error: &anyhow::Error) -> String {
    let client = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ipc::ClientError>());
    let locale = || {
        Paths::from_env()
            .map(|p| BridgeConfig::locale_at(&p.config()))
            .unwrap_or_default()
    };
    match client {
        Some(ipc::ClientError::Connect { path, source })
            if matches!(
                source.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            let socket = path.display();
            locale().pick(
                format!(
                    "Senate daemon 没有运行（{socket} 上没有监听）。\n  \
                     启动：senated --machine\n  \
                     保持运行：senate service install --yes\n  \
                     查看：senate status、senate service status"
                ),
                format!(
                    "the Senate daemon is not running (nothing listens on {socket}).\n  \
                     Start it:        senated --machine\n  \
                     Keep it running: senate service install --yes\n  \
                     Check:           senate status, senate service status"
                ),
            )
        }
        Some(ipc::ClientError::Connect { path, source }) => {
            let socket = path.display();
            locale().pick(
                format!("无法连接 {socket} 上的 Senate daemon：{source}"),
                format!("cannot reach the Senate daemon on {socket}: {source}"),
            )
        }
        Some(ipc::ClientError::Timeout) => locale().pick(
            format!(
                "{error:#}\n  `senate status` 显示 daemon 的状态；`senate service status` 给出日志位置。"
            ),
            format!(
                "{error:#}\n  `senate status` shows the daemon's state; `senate service status` says where its log is."
            ),
        ),
        _ => format!("{error:#}"),
    }
}

/// Run the `senate` command with the process's arguments. This is all of
/// the command: the `senate-bridge` and `senate` packages both install a
/// binary that only calls it.
pub fn main() -> ExitCode {
    let terminal = Terminal::stdout();
    let cli = cli_help::parse(terminal);
    if let Cmd::Hook { event, codex, agy } = cli.command {
        let harness = agy
            .then_some(Runtime::Agy)
            .or_else(|| codex.then_some(Runtime::Codex));
        run_hook(event, harness);
        return ExitCode::SUCCESS;
    }
    let output = Output {
        terminal,
        verbose: cli.verbose,
    };
    match run(cli.command, output) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("senate: {}", describe_error(&e));
            ExitCode::FAILURE
        }
    }
}

/// The coding session `senate` runs in: `explicit`, or the one its
/// environment names (`CODEX_THREAD_ID`, `CLAUDE_CODE_SESSION_ID`).
fn calling_session(explicit: Option<String>, locale: Locale) -> anyhow::Result<SessionSelector> {
    let env_id = |key| std::env::var(key).ok().filter(|id: &String| !id.is_empty());
    let id = match (
        explicit,
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
    Ok(SessionSelector {
        session_id: Some(id),
        claude_pid: None,
    })
}

/// Sets `session`'s duty on `committee` through the daemon, and says so.
async fn set_duty(
    paths: &Paths,
    session: SessionSelector,
    committee: CommitteeId,
    duty: String,
    locale: Locale,
) -> anyhow::Result<String> {
    let cleared = senate_core::duty::normalize(&duty).is_empty();
    let posted: ipc::Posted = daemon_call(
        paths,
        &Request::Duty {
            session,
            duty,
            senator: None,
            committee: Some(committee),
            as_person: false,
            id: Some(senate_core::EventId::new()),
        },
    )
    .await?;
    let (c, seq) = (&posted.committee, posted.seq);
    Ok(match (locale, cleared) {
        (Locale::Zh, false) => format!("已在 {c} 上设置职责（seq {seq}）。"),
        (Locale::Zh, true) => format!("已在 {c} 上清除职责（seq {seq}）。"),
        (Locale::En, false) => format!("Set the duty on {c} (seq {seq})."),
        (Locale::En, true) => format!("Cleared the duty on {c} (seq {seq})."),
    })
}
