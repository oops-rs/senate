//! `senate`: the command for people and their coding agents on a developer
//! machine. It installs Senate into Claude Code and Codex, is the MCP server
//! and the hooks they run, shows what the daemon is doing and what waits for
//! the person, and writes the local profile (`senate setup init --local`). The
//! daemon itself is `senated` (`senated --machine` on a developer machine).

// These two stay beside this file under their own names.
#[path = "cli_chat.rs"]
mod cli_chat;
#[path = "cli_desk.rs"]
mod cli_desk;
#[path = "cli_help.rs"]
mod cli_help;
#[path = "cli_history.rs"]
mod cli_history;
#[path = "cli_legacy.rs"]
mod cli_legacy;
#[path = "cli_observe.rs"]
mod cli_observe;
#[path = "cli_secretary.rs"]
mod cli_secretary;
#[path = "cli_remote.rs"]
mod remote_cli;
#[path = "cli_session_role.rs"]
mod session_role;

use std::io::{Read, Write};
#[path = "cli_completion.rs"]
mod completion;
use clap_complete::engine::ArgValueCandidates;
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
    /// Without one, `senate` runs `senate status`.
    #[command(subcommand)]
    command: Option<Cmd>,
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
/// `senate committees archive|unarchive`.
#[derive(Subcommand)]
enum CommitteesCmd {
    /// Archive committees: hidden from listings, the app and completion,
    /// taking no new threads, seats or sessions; their log, memory and
    /// search stay readable, and their open threads may still be answered.
    ///
    /// Only the Speaker in person, from their own terminal. Refused while a
    /// hosted session on one runs (stop it first). On a terminal it lists
    /// what will be hidden and asks once for them all.
    Archive {
        /// The committees.
        #[arg(required = true, add = ArgValueCandidates::new(completion::committees))]
        committees: Vec<CommitteeId>,
        /// Do not ask.
        #[arg(long, short)]
        yes: bool,
    },
    /// Unarchive committees: listed and open for new threads again.
    Unarchive {
        /// The committees.
        #[arg(required = true)]
        committees: Vec<CommitteeId>,
    },
}

/// `senate memory forget|correct`.
#[derive(Subcommand)]
enum MemoryCmd {
    /// Delete points (by seq) from what the committee knows.
    Forget {
        /// The points' seqs, as `senate memory` shows them.
        #[arg(required = true)]
        seqs: Vec<u64>,
        /// The committee (default: the one `senate where` shows for this directory).
        #[arg(long, add = ArgValueCandidates::new(completion::committees))]
        committee: Option<CommitteeId>,
    },
    /// Correct a point to read <text>.
    Correct {
        /// The point's seq, as `senate memory` shows it.
        seq: u64,
        /// What it should say instead.
        #[arg(required = true)]
        text: Vec<String>,
        /// The committee (default: the one `senate where` shows for this directory).
        #[arg(long, add = ArgValueCandidates::new(completion::committees))]
        committee: Option<CommitteeId>,
    },
}

#[derive(Subcommand)]
enum Cmd {
    /// Issue or manage read-only observation grants.
    Observe(cli_observe::Observe),
    /// Show your recorded local Senate activity by day.
    Stats {
        /// Number of local calendar days ending today (1d to 366d).
        #[arg(long, default_value = "30d")]
        since: String,
        /// Include daily breakdowns and indexing progress as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Bind this worktree to a committee, and suggest a session alias.
    ///
    /// Writes `.senate/committee` (untracked, kept per branch: switching
    /// branches swaps in that branch's own copy). Live sessions here pick
    /// it up at their next prompt. No daemon needed.
    ///
    /// Without a name, the committee is derived from where this is run, and
    /// printed with how: the repository's name (the last segment of the
    /// `origin` URL without `.git`; else the main worktree's folder; outside
    /// git, the directory's name), plus `-<branch>` unless on the default
    /// branch (what `origin/HEAD` points at, else main, master or trunk) or
    /// detached; lowercased, other characters becoming `-`, at most 63
    /// characters (a longer one ends in `-` and a short hash). Read-only.
    ///
    /// With --alias, `.senate/agent` is written too: a session started here
    /// is offered that alias. Run inside a coding session, that session
    /// takes the alias on the committee as well.
    #[command(after_help = "Examples:
  senate join                    Derive the committee from repository and branch
  senate join refund-v2          Bind this worktree to refund-v2
  senate join --alias reviewer   Also name sessions here (and this one) reviewer
  senate senator alias reviewer --session <id>
                                 Name a session from another terminal")]
    Join {
        /// The committee (default: derived from repository and branch).
        #[arg(add = ArgValueCandidates::new(completion::committees))]
        committee: Option<CommitteeId>,
        /// The alias sessions in this worktree are offered, e.g. reviewer;
        /// also given to the calling coding session.
        #[arg(long)]
        alias: Option<String>,
        /// Moved to `senate senator duty`; kept for one release.
        #[arg(long, hide = true)]
        duty: Option<String>,
    },
    /// Remove this worktree's committee binding on the current branch.
    ///
    /// With --alias, only the alias suggestion goes and the committee
    /// binding stays. A live session's alias is removed with `senate
    /// senator unalias`.
    Leave {
        /// Remove only the alias suggestion (`.senate/agent`).
        #[arg(long)]
        alias: bool,
    },
    /// Show the committee and agent names this directory resolves to.
    ///
    /// Prints the committee and where it comes from, the agent a session
    /// here would speak as, the branch `.senate/` belongs to, and whether
    /// other branches have saved copies. With --all, every saved `.senate/`
    /// copy instead, by repository: its branch, committee, when it was
    /// saved, and whether it has expired.
    #[command(after_help = "Examples:
  senate where
  senate where --all --json")]
    Where {
        /// List every saved worktree binding on this machine.
        #[arg(long)]
        all: bool,
        /// With --all, print JSON.
        #[arg(long, requires = "all")]
        json: bool,
    },
    /// List, watch, stop or restore this machine's hosted sessions.
    Session {
        #[command(subcommand)]
        command: SessionCmd,
    },
    /// List senators and their aliases, seat them, set their duty.
    Senator {
        #[command(subcommand)]
        command: SenatorCmd,
    },
    /// Appoint session secretaries and manage their shared work.
    Secretary(cli_secretary::Secretary),
    /// List and decide what awaits the Speaker.
    ///
    /// Read an item, and answer, close, forward or refuse it: the Speaker
    /// in person (or the owner of the agent serving as Speaker), from your
    /// own terminal.
    Desk(cli_desk::Desk),
    /// Simulate chat in a bound group and read the replies
    #[command(after_help = "Examples:
  senate chat send --committee refund-v2 \"ask bob's agent how long refunds take\"
  senate chat thread sim:01j… --wait")]
    Chat(cli_chat::Chat),
    /// The profile, Claude Code and Codex hooks, the daemon service.
    #[command(after_help = "Examples:
  senate setup install --yes          Register Senate with Claude Code and Codex
  senate setup service install --yes  Keep the daemon running, from login
  senate setup clean                  What expired local data would go")]
    Setup {
        #[command(subcommand)]
        command: SetupCmd,
    },
    /// Show what waits for you; with --follow, as it arrives.
    ///
    /// First what was said to you that you have not seen (an answer, a
    /// note, an escalation with its reason), then open task threads on a
    /// committee for your agents or escalated to you, and what awaits the
    /// Speaker of every Senate one of your agents presides over here.
    ///
    /// With --follow it stays in the foreground and prints answers, notes,
    /// questions and escalations (with their reason) as they arrive, on the
    /// committees this machine's sessions are on, or the one named. Not
    /// what is put to the Speaker: that is the agent's docket. Following
    /// marks nothing as read.
    #[command(after_help = "Examples:
  senate inbox                Open threads on this directory's committee
  senate inbox --all          Also answered threads and idle seats
  senate inbox --mark-read    List, then mark what was new as read
  senate inbox --follow       Print what is addressed to you as it arrives
  senate inbox --follow --global --json
  senate thread 12            Read one of the threads listed")]
    Inbox {
        /// The committee (default: the one `senate where` shows for this directory).
        #[arg(long, add = ArgValueCandidates::new(completion::committees))]
        committee: Option<CommitteeId>,
        /// Also list threads whose last word is an answer (they wait on no
        /// one) and seated senators with nothing going on.
        #[arg(long, conflicts_with = "follow")]
        all: bool,
        /// Mark every thread listed as new for you as read, after listing
        /// it. An escalation to you keeps waiting until you act on it.
        #[arg(long, conflicts_with = "follow")]
        mark_read: bool,
        /// Stay and print what is addressed to you as it arrives.
        #[arg(long)]
        follow: bool,
        /// With --follow, every committee on this machine.
        #[arg(long, requires = "follow", conflicts_with = "committee")]
        global: bool,
        /// With --follow, one JSON object per line.
        #[arg(long, requires = "follow")]
        json: bool,
    },
    /// Ask a question.
    ///
    /// You speak as yourself. `<TO>` is a senator's name (an agent such as `claude`, or a person
    /// such as `bob`), whichever agent (`*`), `speaker`, a `senator::…`
    /// address, or a qualified name on another committee (`ledger::codex`).
    /// Text `-` is read from stdin; put `--` before text that starts with
    /// `-`. Goes to the primary Senate of this machine.
    #[command(after_help = "Examples:
  senate ask claude \"Where is the retry limit set?\"
  senate ask '*' \"Which header carries the key?\" --wait
  senate ask ledger::codex \"Is the schema frozen?\"
  git diff | senate ask reviewer -

Without --wait it returns at once: read the answer with `senate inbox`
(`--follow` to see it arrive) or `senate thread <seq>`. `'*'` is whichever
agent is free, `ledger::codex` an agent on another committee, `-` the text on
stdin.")]
    Ask {
        /// Who to ask.
        to: String,
        /// The question (`-`: read stdin).
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
        /// The committee (default: the one `senate where` shows for this directory).
        #[arg(long, add = ArgValueCandidates::new(completion::committees))]
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
        #[arg(long, add = ArgValueCandidates::new(completion::committees))]
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
        #[arg(long, add = ArgValueCandidates::new(completion::committees))]
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
        #[arg(long, add = ArgValueCandidates::new(completion::committees))]
        committee: Option<CommitteeId>,
        /// Print the daemon's result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Search what the committee knows; correct or delete a point.
    ///
    /// The Speaker distils the committee's discussion into short points,
    /// each with the seqs it came from; a newer point replaces an older one.
    /// Without words, the newest points; with them, the best matches. A
    /// correction or deletion is written in your own name.
    #[command(
        args_conflicts_with_subcommands = true,
        after_help = "Examples:
  senate memory
  senate memory release day
  senate memory --all --committee refund-v2
  senate memory correct 140 Releases go out on Tuesdays
  senate memory forget 140 152"
    )]
    Memory {
        #[command(subcommand)]
        action: Option<MemoryCmd>,
        /// Words to look for (default: the newest points).
        words: Vec<String>,
        /// Include points that were replaced or deleted.
        #[arg(long)]
        all: bool,
        /// The committee (default: the one `senate where` shows for this directory).
        #[arg(long, add = ArgValueCandidates::new(completion::committees))]
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
        #[arg(long, add = ArgValueCandidates::new(completion::committees))]
        committee: Option<CommitteeId>,
        /// Print the daemon's result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Take a thread over: nothing more of it goes to agents.
    Take {
        /// The thread's root seq.
        thread: u64,
        /// The committee (default: the one `senate where` shows for this directory).
        #[arg(long, add = ArgValueCandidates::new(completion::committees))]
        committee: Option<CommitteeId>,
    },
    /// Run the Senate MCP server over stdio.
    #[command(hide = true)]
    Mcp {
        /// The coding runtime that started this server.
        #[arg(long, value_enum, conflicts_with = "acp_session")]
        runtime: Option<RuntimeFlag>,
        /// Old spelling of `--runtime agy`, kept for one release.
        #[arg(long, hide = true, conflicts_with_all = ["runtime", "acp_session"])]
        agy: bool,
        /// Exact session supplied by the owning Senate ACP client.
        #[arg(long, hide = true)]
        acp_session: Option<String>,
    },
    /// Run a coding session's hook.
    ///
    /// Claude Code / Codex hook entry point; reads the hook JSON on stdin.
    /// Always exits 0.
    #[command(hide = true)]
    Hook {
        /// The hook event.
        #[arg(value_enum)]
        event: HookKind,
        /// The coding runtime running the hook. Without it, the hook
        /// input says (Claude Code unless it names a Codex rollout).
        #[arg(long, value_enum)]
        runtime: Option<RuntimeFlag>,
        /// Old spelling of `--runtime codex`, kept for one release.
        #[arg(long, hide = true, conflicts_with_all = ["runtime", "agy"])]
        codex: bool,
        /// Old spelling of `--runtime agy`, kept for one release.
        #[arg(long, hide = true, conflicts_with = "runtime")]
        agy: bool,
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
  senate status --full-ids    Complete IDs, e.g. for `senator alias --session`")]
    Status {
        #[command(flatten)]
        scope: DisplayScope,
        /// Show complete session and senator IDs.
        #[arg(long)]
        full_ids: bool,
        /// Include inactive agents and sessions.
        #[arg(long)]
        all: bool,
    },
    /// List, archive or unarchive committees, with chairs and members.
    #[command(after_help = "Examples:
  senate committees                         The open ones
  senate committees --archived              The archived ones
  senate committees archive api-test sim-relay
                                            Hide them (asks first on a terminal)
  senate committees unarchive api-test      Bring one back")]
    Committees {
        /// List the archived committees instead.
        #[arg(long)]
        archived: bool,
        #[command(subcommand)]
        action: Option<CommitteesCmd>,
    },
    /// Set or rename a senator's alias on its committee.
    Alias {
        /// Full senator ID, alias, or committee::alias.
        #[arg(value_parser = mcp::parse_alias_name)]
        senator: AliasName,
        /// The new role name.
        alias: SenatorName,
        /// Committee to rename the senator on, if omitted from the senator.
        #[arg(long, add = ArgValueCandidates::new(completion::committees))]
        committee: Option<CommitteeId>,
    },
    /// Check the Codex app-server connection.
    ///
    /// Smoke test of the Codex app-server adapter: connect, initialize, read
    /// a thread's status and, with --text, start or steer a turn with it.
    #[command(hide = true)]
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
    #[command(
        hide = true,
        after_help = "Examples:
  senate acp -- agy acp"
    )]
    Acp {
        /// Command and arguments of the ACP stdio server.
        #[arg(last = true, required = true, num_args = 1..)]
        command: Vec<std::ffi::OsString>,
    },
    /// List this machine's named launch profiles and when to use them.
    Profiles,
    /// Start a coding agent in the background as a senator.
    ///
    /// Starts the Claude Code, Codex or Antigravity (agy) TUI in a
    /// Senate-owned terminal,
    /// seats it on the committee and prints its senator name. Open its
    /// terminal with `--attach` or `senate attach`.
    #[command(after_help = "Examples:
  senate spawn claude                    On this directory's committee
  senate spawn codex --alias reviewer    With a name to address it by
  senate spawn --profile review          Use this machine's review settings
  senate spawn claude --dir ../refund    On the committee bound in ../refund
  senate spawn codex --prompt 'Review the open PR'
                                         Send it a first request once seated
  senate spawn claude --model opus --effort high
                                         Not the runtime's default model
  senate spawn agy --prompt 'Triage the failing tests'
                                         An Antigravity CLI senator
  senate attach reviewer                 Open its terminal (Ctrl-] detaches)")]
    Spawn {
        /// Reuse a spawn attempt UUIDv7 with the same arguments, within seven days.
        #[arg(long)]
        attempt_id: Option<String>,
        /// The coding agent to start. Must agree with --profile if both are given.
        #[arg(value_enum)]
        runtime: Option<HostedRuntime>,
        /// Use a named launch profile from this machine. See `senate profiles`.
        #[arg(long, add = ArgValueCandidates::new(completion::profiles))]
        profile: Option<String>,
        /// The committee to seat it on (default: the one bound at --dir,
        /// else the one `senate where` shows for this directory).
        #[arg(long, add = ArgValueCandidates::new(completion::committees))]
        committee: Option<CommitteeId>,
        /// A role name such as reviewer. Defaults to the profile name or a runtime name and counter.
        #[arg(long)]
        alias: Option<SenatorName>,
        /// Appoint the new senator as the committee chair.
        #[arg(long, conflicts_with = "secretary")]
        chair: bool,
        /// Appoint the new senator as a secretary of the Speaker’s desk.
        #[arg(long)]
        secretary: bool,
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
        /// Attach to the new terminal after it is ready. Requires a TTY.
        #[arg(short = 'a', long)]
        attach: bool,
        #[command(flatten)]
        model: ModelArgs,
    },
    /// Open a hosted terminal, starting it if needed.
    ///
    /// Stopped sessions are restored and paused ones resumed. Press Ctrl-] to detach.
    ///
    /// After /new or /clear in hosted Codex, Senate still delivers to the
    /// original thread, which the TUI no longer shows. Exit that TUI and use
    /// senate session restore to return. A Codex session that was live before restore
    /// stays live on host close because a native client may remain.
    ///
    /// Hosted Claude keeps the senator when the same process reports a new
    /// session after /clear or /resume. A refused change appears in status.
    /// The configured executable must be Claude itself or a wrapper that execs it.
    ///
    /// With --read-only the terminal is only shown: nothing typed reaches
    /// the session, its size is left as it is, and any number of people can
    /// look at once, beside whoever is attached. It never starts a session. Ctrl-] leaves.
    Attach {
        /// Connect to an already-running session on a paired machine.
        #[arg(long, add = ArgValueCandidates::new(remote_cli::complete_machines))]
        machine: Option<Name>,
        /// Primary name, alias, chair, committee::chair, secretary or speaker.
        /// Bare chair follows this directory's committee. Without a selector,
        /// choose a hosted session in the terminal.
        #[arg(add = ArgValueCandidates::new(completion::sessions))]
        senator: Option<String>,
        /// Select an exact native session ID (used by the app).
        #[arg(long, hide = true, conflicts_with = "senator")]
        session: Option<String>,
        /// Look without typing: every key but Ctrl-] is dropped.
        #[arg(long)]
        read_only: bool,
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
        /// See `host::prompt::QUIET`.
        #[arg(long)]
        prompt_quiet_ms: Option<u64>,
    },
    /// Manual checks of the delivery plumbing.
    #[command(hide = true)]
    Debug {
        #[command(subcommand)]
        command: DebugCmd,
    },
}

/// The runtime's own model and effort for a hosted session. Omitted
/// choices leave the runtime's settings in control.
#[derive(Args)]
struct ModelArgs {
    /// A model ID or family (e.g. astra). Claude Code resolves its own aliases.
    #[arg(long)]
    model: Option<String>,
    /// Reasoning effort. Codex: low, medium, high, xhigh.
    /// Claude Code and agy also accept max.
    #[arg(long, add = ArgValueCandidates::new(completion::efforts))]
    effort: Option<String>,
}

impl From<ModelArgs> for crate::ipc::HostedModel {
    fn from(args: ModelArgs) -> Self {
        Self::new(args.model, args.effort)
    }
}

#[derive(Args)]
struct DisplayScope {
    /// Show all committees on this machine.
    #[arg(long, conflicts_with = "committee")]
    global: bool,
    /// Show this committee. Defaults to the current worktree's committee.
    #[arg(long, add = ArgValueCandidates::new(completion::committees))]
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
    /// Show how one event was recorded and delivered on this machine.
    Receipt {
        /// The event's seq.
        seq: u64,
        /// The committee (default: the one `senate where` shows for this directory).
        #[arg(long, add = ArgValueCandidates::new(completion::committees))]
        committee: Option<CommitteeId>,
        /// Print the daemon's result as JSON.
        #[arg(long)]
        json: bool,
    },
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
enum SessionCmd {
    /// Repair two agy conversation records for one hosted terminal.
    Rebind {
        /// The original senator by name or role, or the terminal's host ID.
        #[arg(add = ArgValueCandidates::new(completion::sessions))]
        host: String,
        /// The live senator that should own this terminal, by name or role.
        #[arg(long, add = ArgValueCandidates::new(completion::sessions))]
        to: String,
    },
    /// List past sessions or search their conversations.
    History {
        /// Restrict history to this committee.
        #[arg(long, add = ArgValueCandidates::new(completion::committees))]
        committee: Option<CommitteeId>,
        /// Search conversation text, commands and paths.
        #[arg(long)]
        search: Option<String>,
    },
    /// Read a past conversation.
    Show {
        /// Session ID, name, alias or role (chair, committee::chair, secretary, speaker).
        #[arg(add = ArgValueCandidates::new(completion::history))]
        session: String,
    },
    /// List this machine's hosted sessions.
    ///
    /// Each session's name (`committee::alias`, else its senator),
    /// committee, runtime, model, state, uptime, who is at its terminal,
    /// and directory.
    List {
        /// List hosted sessions on this paired machine.
        #[arg(long, conflicts_with = "all_machines", add = ArgValueCandidates::new(remote_cli::complete_machines))]
        machine: Option<Name>,
        /// List this machine and every paired machine.
        #[arg(long)]
        all_machines: bool,
        /// Include stopped hosted sessions.
        #[arg(long)]
        all: bool,
    },
    /// Watch hosted terminals side by side, hiding those with an interactive viewer.
    ///
    /// A full-screen grid of read-only views of the sessions hosted on this
    /// machine, one tile a session, in pages when they do not all fit.
    /// Nothing typed here, in the grid or the finder, reaches a session.
    ///
    /// / or Ctrl-P finds a session by name: type to narrow the list
    /// (fzf-style), Ctrl-N/P, Ctrl-J/K or the arrows move, Enter attaches,
    /// Tab shows its tile, Ctrl-U clears, Ctrl-W deletes a word, Esc or
    /// Ctrl-C closes. In the grid: arrows or h j k l move between tiles (a
    /// count repeats: 3j), gg and G go to the first and the last (5G: the
    /// fifth), H L, [ ] or PgUp PgDn between pages, n and N to the next
    /// and previous match of the last find; Enter or o attaches to the
    /// tile's session (detaching comes back to the grid); ? lists the keys;
    /// q or Ctrl-] leaves.
    Watch {
        /// Watch hosted sessions on this paired machine.
        #[arg(long, add = ArgValueCandidates::new(remote_cli::complete_machines))]
        machine: Option<Name>,
        /// Include stopped sessions and those with an interactive viewer attached.
        #[arg(long)]
        all: bool,
        /// Watch only these committees' sessions (repeat for several).
        #[arg(
            long,
            conflicts_with = "machine",
            add = ArgValueCandidates::new(completion::committees)
        )]
        committee: Vec<CommitteeId>,
    },
    /// End hosted sessions' terminals (`senate session restore` reopens one).
    ///
    /// Each TUI is asked to end, then killed. A session someone typed into
    /// in the last 5 minutes is skipped unless --force.
    #[command(after_help = "Examples:
  senate session stop reviewer       Stop one, by alias or committee::alias
  senate session stop --idle         Stop each one nobody is attached to
  senate session stop --all --force  Stop all, even if typed into lately")]
    Stop {
        /// Sessions by name, alias or role (chair, committee::chair, secretary, speaker).
        #[arg(required_unless_present_any = ["idle", "all"], add = ArgValueCandidates::new(completion::sessions))]
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
    /// Make the terminal a hosted session is attached in ring and flash its
    /// title, to find the panel that holds it.
    ///
    /// Only that terminal's title and bell are touched, for a few seconds;
    /// nothing reaches the agent's screen or input. A refused `senate
    /// attach` does the same.
    #[command(after_help = "Examples:
  senate session find senate::main    Which panel holds it?")]
    Find {
        /// The senator's alias (`committee::alias` for one on one
        /// committee) or primary name.
        #[arg(add = ArgValueCandidates::new(completion::sessions))]
        senator: AliasName,
    },
    /// Answer the permission prompt waiting in a hosted session's terminal,
    /// without attaching.
    ///
    /// Answered as you would in the terminal: Codex through its
    /// app-server, Claude Code by choosing the option of its permission
    /// dialog. A prompt Senate only saw on the screen is not answered this
    /// way: attach to it. The answer is noted in the committee log. Refused
    /// from inside a coding session (an agent cannot approve prompts).
    #[command(after_help = "Examples:
  senate session approve reviewer            Allow it once
  senate session approve reviewer --always   Allow it for the session
  senate session approve reviewer --deny     Refuse it
  senate session approve --once              The only prompt waiting")]
    Approve {
        /// Name, alias or role (chair, committee::chair, secretary, speaker).
        /// Without one, answer the only session with a prompt waiting. Give
        /// --once, --always or --deny, and the command prints the prompt first.
        #[arg(add = ArgValueCandidates::new(completion::sessions))]
        senator: Option<String>,
        /// Allow it this once (the default).
        #[arg(long, conflicts_with_all = ["always", "deny"])]
        once: bool,
        /// Allow it, and stop asking for the same thing this session.
        #[arg(long, conflicts_with = "deny")]
        always: bool,
        /// Refuse it.
        #[arg(long)]
        deny: bool,
    },
    /// Pause hosted sessions, to resume them later where they were.
    ///
    /// Each is sent a Senate note asking it to finish its current step and
    /// write a checkpoint. The pause waits up to 90 s for the checkpoint
    /// (--timeout changes that), or until the session has been quiet for
    /// 10 s after writing it. Then its terminal is stopped as `session
    /// stop` stops one, and it is recorded paused: its presence shows
    /// paused, and what is sent to it waits. A session someone typed into
    /// in the last 5 minutes is skipped unless --force. With no name and
    /// no --all or --committee, choose one from a list (in a terminal).
    #[command(after_help = "Examples:
  senate session pause reviewer          Pause one, by alias or committee::alias
  senate session pause --committee refund-v2
  senate session pause --all --dry-run   Show what would be paused
  senate session pause --all --timeout 30")]
    Pause {
        #[command(flatten)]
        which: PauseSelection,
        /// Pause even a session typed into in the last 5 minutes, or with a
        /// viewer attending it.
        #[arg(long)]
        force: bool,
        /// List what would be paused, and pause nothing.
        #[arg(long)]
        dry_run: bool,
        /// Seconds to wait for each session's checkpoint (default 90).
        #[arg(long, value_name = "SECS")]
        timeout: Option<u64>,
    },
    /// Resume paused hosted sessions in the same conversation.
    ///
    /// Each session recorded paused is restored as `session restore` does,
    /// then sent a note pointing at its last checkpoint. A session stopped
    /// on purpose is not paused, and is not resumed. With no name and no
    /// --all or --committee, choose one from a list (in a terminal).
    #[command(after_help = "Examples:
  senate session resume reviewer
  senate session resume --all")]
    Resume {
        #[command(flatten)]
        which: PauseSelection,
        /// List what would be resumed, and resume nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Resume a stopped hosted senator in the background.
    ///
    /// The recorded session reopens in a detached Senate-owned terminal. A
    /// hosted Claude session with no saved conversation opens with its
    /// original ID.
    Restore {
        /// Name, alias or role (chair, committee::chair, secretary, speaker).
        #[arg(add = ArgValueCandidates::new(completion::sessions))]
        senator: String,
        /// Attach to the new terminal after it is ready. Requires a TTY.
        #[arg(short = 'a', long)]
        attach: bool,
        #[command(flatten)]
        model: ModelArgs,
    },
    /// Start a fresh hosted session in a senator's place and hand it the
    /// seat.
    ///
    /// The new session has the same runtime, directory and committee. Once
    /// it is seated and connected it takes the old seat's alias on the
    /// committee, its duty, its place among the desk workers, the chair if
    /// the old seat held it, and its open questions and requests (each
    /// handed over in its own thread); then the old session is stopped and
    /// unseated. Each step is reported. If one fails, the command says
    /// where; run it again to resume from there. If starting the new session
    /// fails, nothing is handed over.
    #[command(after_help = "Examples:
  senate session replace reviewer
  senate session replace reviewer --prompt 'Pick up the gateway review'
  senate session replace refund-v2::reviewer --model opus --effort high")]
    Replace {
        /// Name, alias or role (chair, committee::chair, secretary, speaker).
        #[arg(add = ArgValueCandidates::new(completion::sessions))]
        senator: String,
        /// A first instruction for the new session, sent once the seat is
        /// handed over, as an ordinary Senate request from you.
        #[arg(long, conflicts_with = "prompt_file")]
        prompt: Option<String>,
        /// Like --prompt, read from a file.
        #[arg(long, value_name = "PATH")]
        prompt_file: Option<PathBuf>,
        /// Each given replaces the old session's for the new one.
        /// Attach to the new terminal after it is ready. Requires a TTY.
        #[arg(short = 'a', long)]
        attach: bool,
        #[command(flatten)]
        model: ModelArgs,
    },
    /// Stop terminal hosts that no session owns, and clear ended hosts'
    /// leftovers.
    Prune,
}

#[derive(Subcommand)]
enum SenatorCmd {
    /// List senators by committee and display name.
    List {
        /// Include inactive senators.
        #[arg(long)]
        all: bool,
    },
    /// Give one coding session an alias on its committee.
    ///
    /// The alias is bound to the session's primary ID and persists. To
    /// have every session started in a worktree offered an alias, use
    /// `senate join --alias`.
    #[command(after_help = "Examples:
  senate senator alias reviewer     Inside a coding session: name it reviewer
  senate senator alias reviewer --session <id>
                                    Name a session from another terminal
                                    (`senate status --full-ids` prints the ids)")]
    Alias {
        /// The alias, e.g. reviewer.
        alias: SenatorName,
        /// The committee (default: the session's current committee).
        #[arg(long, add = ArgValueCandidates::new(completion::committees))]
        committee: Option<CommitteeId>,
        /// Full session ID. Defaults to the calling coding session.
        #[arg(long)]
        session: Option<String>,
    },
    /// Remove an alias. A bare alias on several committees asks which;
    /// `committee::alias` names one.
    #[command(after_help = "Examples:
  senate senator unalias smoke       Remove the alias smoke
  senate senator unalias nous::main  Remove main from the committee nous")]
    Unalias {
        /// The alias, or `committee::alias`.
        alias: AliasName,
    },
    /// Make a senator the chair of a committee, as you.
    ///
    /// The chair picks the agent for a message to whichever agent there,
    /// covers for an absent member and receives the members' escalations.
    /// The server decides who may: a person of this Senate (or the
    /// Speaker's office). The old chair stays a member; `senate senator
    /// unseat` removes it. Prints who chaired before and who chairs now.
    #[command(after_help = "Examples:
  senate senator chair main                  On this directory's committee
  senate senator chair agy-f2a97944 --committee ibot")]
    Chair {
        /// The senator's alias (`committee::alias`) or primary name.
        #[arg(add = ArgValueCandidates::new(completion::sessions))]
        senator: AliasName,
        /// The committee (default: the one `senate where` shows for this directory).
        #[arg(long, add = ArgValueCandidates::new(completion::committees))]
        committee: Option<CommitteeId>,
    },
    /// Seat a removed senator on the committee again, under the same id.
    Seat(SeatArgs),
    /// Remove a senator's seat and end its connection on a committee.
    Unseat(SeatArgs),
    /// Set a session's duty on its committee: one line on what it does.
    #[command(after_help = "Examples:
  senate senator duty 'reviews the server crate'
  senate senator duty ''                     Clear it
  senate senator duty 'triage' --session <id> --committee refund-v2")]
    Duty {
        /// The duty. An empty string clears it.
        duty: String,
        /// The committee (default: the session's current committee).
        #[arg(long, add = ArgValueCandidates::new(completion::committees))]
        committee: Option<CommitteeId>,
        /// Full session ID. Defaults to the calling coding session.
        #[arg(long)]
        session: Option<String>,
    },
}

/// Which sessions `session pause` and `resume` are for.
#[derive(Args)]
struct PauseSelection {
    /// Sessions by name, alias or role (chair, committee::chair, secretary, speaker).
    #[arg(add = ArgValueCandidates::new(completion::sessions))]
    names: Vec<String>,
    /// Every one.
    #[arg(long, conflicts_with_all = ["names", "committee"])]
    all: bool,
    /// Every one on this committee.
    #[arg(long, conflicts_with = "names", add = ArgValueCandidates::new(completion::committees))]
    committee: Option<CommitteeId>,
}

#[derive(Args)]
struct SeatArgs {
    /// A primary senator id or alias in this Senate.
    #[arg(value_parser = parse_seat_name)]
    senator: SenatorName,
    /// The committee the seat is on.
    #[arg(long)]
    committee: CommitteeId,
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

#[derive(Subcommand)]
enum SetupCmd {
    /// Pair and administer direct remote machines.
    Remote {
        #[command(subcommand)]
        command: remote_cli::Command,
    },
    /// Print shell completion registration for sourcing.
    Completions {
        /// Shell to register with (zsh, bash or fish).
        #[arg(value_enum)]
        shell: completion::Shell,
    },
    /// Write the profile for using Senate alone on this machine.
    ///
    /// Writes `senate.toml` in the Senate home, with a local top Senate
    /// whose Speaker is you or one of your agents. Writes nothing else and
    /// never overwrites an existing profile. Under a team Senate, write
    /// senate.toml from config/senate.example.toml instead.
    #[command(after_help = "Examples:
  senate setup init --local --person alice
  senate setup init --local --person alice --speaker claude --locale zh")]
    Init {
        /// The local profile (the only one `setup init` writes).
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
    /// Set up Claude Code, Codex and agy. Dry run without --yes.
    ///
    /// Registers the MCP server and the session hooks. Codex and the
    /// Antigravity CLI are each set up when they are on PATH. Without --yes it prints what it would change and
    /// writes nothing.
    #[command(after_help = "Examples:
  senate setup install          Show what would change
  senate setup install --yes    Write it")]
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
        /// Set up the Antigravity CLI even when `agy` is not on PATH.
        #[arg(long, conflicts_with = "no_agy")]
        agy: bool,
        /// Leave the Antigravity CLI alone.
        #[arg(long)]
        no_agy: bool,
        /// agy's directory holding config/ and antigravity-cli/ (default: ~/.gemini).
        #[arg(long, value_name = "DIR")]
        gemini_dir: Option<PathBuf>,
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
  senate setup service install --yes
  senate setup service status
  senate setup service uninstall --yes")]
    Service {
        #[command(subcommand)]
        command: ServiceCmd,
    },
    /// Check this machine, and say how to fix what is wrong.
    ///
    /// Each check prints ok, warn or fail, what it found and, where one
    /// command fixes it, that command: the daemon and its service file;
    /// whether the daemon finds claude, codex and agy; the MCP server and
    /// hooks of Claude Code, Codex and agy; hooks and MCP servers other
    /// bridges left behind; the Senate this machine talks to and the one
    /// above it (reachable, registered, version, log epoch, cursors); the
    /// worktree bindings; hosted sessions; and, in WSL, systemd and linger.
    /// Exits non-zero when a check fails. Changes nothing.
    #[command(after_help = "Examples:
  senate setup doctor
  senate setup doctor --json")]
    Doctor {
        /// Print JSON: {checks: [{id, status, found, fix?}], summary}.
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
    Clean {
        /// Do it.
        #[arg(long)]
        yes: bool,
        /// With --yes, also delete the data of hosted Senates that
        /// `senate.toml` no longer lists: logs, which nothing else removes.
        #[arg(long, requires = "yes")]
        delete_logs: bool,
    },
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
/// `--runtime` on the plumbing commands (`hook`, `mcp`): the coding
/// runtimes that run Senate's hooks and MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum RuntimeFlag {
    Claude,
    Codex,
    Agy,
}

impl RuntimeFlag {
    /// The runtime `--runtime`, or one of the old `--codex` / `--agy`
    /// flags, names. `None`: none was given.
    fn chosen(runtime: Option<Self>, codex: bool, agy: bool) -> Option<Runtime> {
        let flag = runtime
            .or_else(|| codex.then_some(Self::Codex))
            .or_else(|| agy.then_some(Self::Agy))?;
        Some(match flag {
            Self::Claude => Runtime::Claude,
            Self::Codex => Runtime::Codex,
            Self::Agy => Runtime::Agy,
        })
    }
}

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
        HookEnv::default()
    } else {
        HookEnv::from_process()
    };
    let hosted_claude_start = kind == HookKind::SessionStart
        && match runtime_override {
            None | Some(Runtime::Claude) => {
                std::env::var_os(crate::host::HOSTED_CLAUDE_ENV).is_some()
            }
            // A new hosted agy's first hook waits for its launch to publish.
            Some(Runtime::Agy) => std::env::var_os(crate::host::HOSTED_AGY_ENV).is_some(),
            Some(_) => false,
        };
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
    match binding::committee_here(&paths, named, &settings).await {
        Ok(committee) => Ok(committee),
        // Nothing binds one here: name the committees there are to choose.
        Err(error) => {
            match daemon_call::<Vec<ipc::RosterCommittee>>(&paths, &Request::Roster).await {
                Ok(rows) if !rows.is_empty() => {
                    let list = rows
                        .iter()
                        .map(|row| row.committee.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    Err(anyhow::anyhow!(cli_locale().pick(
                        format!("{error:#}。委员会：{list}"),
                        format!("{error:#}. Committees: {list}"),
                    )))
                }
                _ => Err(error),
            }
        }
    }
}

/// The committee `senate spawn` seats on: `--committee`; else, with
/// `--dir` elsewhere, the one bound there; else this directory's. When
/// `--dir` binds none, says on stderr which committee was used instead.
async fn spawn_committee(
    named: Option<CommitteeId>,
    dir: &std::path::Path,
) -> anyhow::Result<CommitteeId> {
    if named.is_some() {
        return committee_here(named).await;
    }
    let same = |a: &std::path::Path, b: &std::path::Path| {
        a.canonicalize()
            .ok()
            .is_some_and(|a| b.canonicalize().ok() == Some(a))
    };
    let cwd = std::env::current_dir()?;
    if same(dir, &cwd) {
        return committee_here(None).await;
    }
    let (paths, settings) = settings_here();
    if let Some(committee) = binding::directory_committee(&paths, dir, &settings).await {
        return Ok(committee);
    }
    let committee = committee_here(None).await?;
    eprintln!(
        "{}",
        cli_locale().pick(
            format!(
                "{} 没有绑定委员会；按当前目录，坐到 {committee}（用 --committee 选别的）。",
                dir.display()
            ),
            format!(
                "{} is bound to no committee; seated on {committee}, this directory's \
                 (--committee chooses another).",
                dir.display()
            ),
        )
    );
    Ok(committee)
}

/// The committee a message to `to` goes to: `--committee`, else this
/// directory's; with neither, the one committee where `to` is an alias,
/// said on stderr so it is never a surprise. Otherwise the error names the
/// committees to choose from. Whether it was found that way, so the
/// commands suggested after name it.
async fn committee_for(
    paths: &Paths,
    named: Option<CommitteeId>,
    to: &str,
    locale: Locale,
) -> anyhow::Result<(CommitteeId, bool)> {
    let unbound = match committee_here(named).await {
        Ok(committee) => return Ok((committee, false)),
        Err(error) => error,
    };
    let candidates = match to.parse::<SenatorName>() {
        Ok(alias) => match daemon_call::<KnownAliases>(paths, &Request::Aliases).await {
            Ok(KnownAliases::Scoped(table)) => {
                let mut committees: Vec<CommitteeId> = table
                    .matches(&alias)
                    .into_iter()
                    .map(|q| q.committee)
                    .collect();
                committees.dedup();
                committees
            }
            _ => Vec::new(),
        },
        // `*`, `speaker`, a qualified name: not an alias to look up.
        Err(_) => Vec::new(),
    };
    match candidates.as_slice() {
        [committee] => {
            eprintln!(
                "{}",
                locale.pick(
                    format!("在委员会 {committee} 上（{to} 是那里的别名）"),
                    format!("on committee {committee} (where {to} is an alias)"),
                )
            );
            Ok((committee.clone(), true))
        }
        [] => Err(unbound),
        several => {
            let list = several
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            Err(anyhow::anyhow!(locale.pick(
                format!("{to} 是多个委员会上的别名：{list}。用 --committee 选一个。"),
                format!("{to} is an alias on more than one committee: {list}. Choose one with --committee."),
            )))
        }
    }
}

fn settings_here() -> (Paths, Settings) {
    let paths = Paths::from_env().unwrap_or_else(|_| Paths::new("."));
    let settings = Settings::load(&paths.config());
    (paths, settings)
}

/// The hosted sessions of this machine, as
/// `senate session watch` shows them: `senate session list`' rows, each named as
/// a command takes it.
async fn hosted_sessions(
    paths: &Paths,
    all: bool,
    committees: &[CommitteeId],
    verbose: bool,
    locale: Locale,
) -> anyhow::Result<Vec<crate::host::view::Session>> {
    let reply: crate::ipc::SessionsReply = daemon_call(paths, &Request::Sessions).await?;
    Ok(watch_tiles(reply.rows, all, committees, verbose, locale))
}

fn watch_tiles(
    mut rows: Vec<crate::ipc::SessionRow>,
    all: bool,
    committees: &[CommitteeId],
    verbose: bool,
    locale: Locale,
) -> Vec<crate::host::view::Session> {
    // `--committee` narrows the grid to those committees' sessions.
    if !committees.is_empty() {
        rows.retain(|row| {
            row.committee
                .as_ref()
                .is_some_and(|c| committees.contains(c))
        });
    }
    // Only a running host has a terminal to watch. A paused or stopped one
    // is left out unless `--all` asks for it, and is then shown greyed out.
    // Group by committee and put its chair first, keeping the daemon order
    // within each group of chairs or members.
    rows.retain(|row| row.is_running() || all);
    rows.sort_by(|a, b| {
        a.committee
            .cmp(&b.committee)
            .then_with(|| b.chair.cmp(&a.chair))
    });
    rows.into_iter()
        .map(|row| crate::host::view::Session {
            key: row.session_id.clone(),
            title: format!(
                "{}{} · {} · {} · {}",
                if row.chair { "[chair] " } else { "" },
                if !verbose && row.alias.is_some() {
                    row.label()
                } else {
                    row.display_name(verbose)
                },
                row.runtime.as_str(),
                row.model.label(),
                if row.paused {
                    locale.pick("已暂停", "paused")
                } else {
                    row.state.as_str()
                }
            ),
            // What a command takes: `committee::alias`, so it names one
            // session whatever other committees call theirs.
            alias: row.label(),
            attached: row.is_running() && row.attached,
            dim: !row.is_running(),
            tty: row.tty,
        })
        .collect()
}

#[test]
fn watch_tiles_group_each_committee_with_its_chair_first() {
    let row = |committee: &str, alias: &str, chair: bool| {
        serde_json::from_value::<crate::ipc::SessionRow>(serde_json::json!({
            "host_id": senate_core::EventId::new(),
            "session_id": format!("session-{committee}-{alias}"),
            "primary": alias,
            "alias": alias,
            "committee": committee,
            "chair": chair,
            "runtime": "claude",
            "state": "running",
            "dir": "/fixture",
            "tty": "/fixture/tty"
        }))
        .expect("hosted session row")
    };
    let tiles = watch_tiles(
        vec![
            row("west", "builder", false),
            row("east", "builder", false),
            row("west", "lead", true),
            row("east", "lead", true),
        ],
        false,
        &[],
        false,
        Locale::En,
    );
    let labels: Vec<_> = tiles.iter().map(|tile| tile.alias.as_str()).collect();
    assert_eq!(
        labels,
        ["east::lead", "east::builder", "west::lead", "west::builder"]
    );
    assert!(tiles[0].title.starts_with("[chair] "));
    assert!(tiles[2].title.starts_with("[chair] "));
    assert!(!tiles[1].title.starts_with("[chair] "));

    // `--committee west` leaves the other committees out.
    let west = [CommitteeId::new("west").expect("committee")];
    let only: Vec<_> = watch_tiles(
        vec![
            row("west", "builder", false),
            row("east", "builder", false),
            row("west", "lead", true),
        ],
        false,
        &west,
        false,
        Locale::En,
    )
    .into_iter()
    .map(|tile| tile.alias)
    .collect();
    assert_eq!(only, ["west::lead", "west::builder"]);
}

#[test]
fn watch_leaves_out_paused_and_stopped_sessions_unless_all() {
    let row = |alias: &str, state: &str, paused: bool| {
        serde_json::from_value::<crate::ipc::SessionRow>(serde_json::json!({
            "host_id": senate_core::EventId::new(),
            "session_id": format!("session-{alias}"),
            "primary": alias,
            "alias": alias,
            "committee": "west",
            "runtime": "claude",
            "state": state,
            "paused": paused,
            "dir": "/fixture",
            "tty": "/fixture/tty"
        }))
        .expect("hosted session row")
    };
    let rows = || {
        vec![
            row("running", "running", false),
            row("paused", "stopped", true),
            row("stopped", "stopped", false),
        ]
    };
    let labels = |all| -> Vec<String> {
        watch_tiles(rows(), all, &[], false, Locale::En)
            .into_iter()
            .map(|tile| tile.alias)
            .collect()
    };
    assert_eq!(labels(false), ["west::running"]);
    assert_eq!(
        labels(true),
        ["west::running", "west::paused", "west::stopped"]
    );
    let all = watch_tiles(rows(), true, &[], false, Locale::En);
    assert!(all[1].dim && all[1].title.ends_with("paused"));
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
        // Refused, with the reason to show (an archived committee).
        Ok(Err(ipc::ClientError::Remote(error))) if error.code == ipc::codes::BAD_REQUEST => {
            error.message
        }
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
    let known: KnownAliases = runtime(false)?
        .block_on(ipc::request(
            &paths.socket(),
            &Request::Aliases,
            Duration::from_secs(1),
            Duration::from_secs(2),
        ))
        .with_context(|| format!("look up alias {name}"))?;
    match known.resolve(&name, None) {
        AliasMatch::Ambiguous { candidates, .. } => {
            let chosen = choose_alias(&name, candidates)?;
            Ok(AliasName::bare(chosen.primary))
        }
        _ => Ok(name),
    }
}

fn session_senator(paths: &Paths, written: &str) -> anyhow::Result<AliasName> {
    if let Some(holder) = session_role::resolve(paths, written, None)? {
        return Ok(AliasName::bare(holder.primary));
    }
    pick_alias(written.parse()?)
}

/// `senate session stop`: stops each chosen session as the daemon's `Stop`
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
            Ok(reply) => {
                println!(
                    "{}",
                    locale.pick(
                        format!("已停止 {}", row.label()),
                        format!("stopped {}", row.label())
                    )
                );
                if reply.audit_pending {
                    eprintln!(
                        "{}",
                        locale.pick(
                            "停止记录已排队，稍后写入委员会日志。",
                            "The stop note is queued for the committee log."
                        )
                    );
                }
            }
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
    // A session asked for by name and left running is a failure; `--idle`
    // and `--all` only report what they skipped.
    if matches!(selector, crate::sessions::StopSelector::Named(_)) {
        failed += kept.len();
    }
    anyhow::ensure!(failed == 0, "{failed} session(s) could not be stopped");
    Ok(())
}

/// What `session pause` (or, `paused`, `resume`) is for: the names given,
/// each resolved as `attach` resolves one (an alias several agents have is
/// the person's to pick), a committee, or all; with none, the session the
/// person picks from the eligible ones. `None` when the person cancels.
fn pause_selector(
    which: PauseSelection,
    paused: bool,
) -> anyhow::Result<Option<crate::sessions::StopSelector>> {
    use crate::sessions::StopSelector;
    if which.all {
        return Ok(Some(StopSelector::All));
    }
    if let Some(committee) = which.committee {
        return Ok(Some(StopSelector::Committee(committee)));
    }
    if which.names.is_empty() {
        let paths = Paths::from_env()?;
        let rt = runtime(false)?;
        let reply: crate::ipc::SessionsReply =
            rt.block_on(daemon_call(&paths, &Request::Sessions))?;
        let eligible: Vec<crate::ipc::SessionRow> = reply
            .rows
            .into_iter()
            .filter(|r| if paused { r.paused } else { r.is_running() })
            .collect();
        let locale = cli_locale();
        anyhow::ensure!(
            !eligible.is_empty(),
            "{}",
            if paused {
                locale.pick(
                    "没有暂停的托管会话；`senate session list` 列出它们。",
                    "no hosted session is paused; `senate session list` lists them",
                )
            } else {
                locale.pick(
                    "没有运行中的托管会话；`senate session list` 列出它们。",
                    "no hosted session is running; `senate session list` lists them",
                )
            }
        );
        let command = if paused {
            "senate session resume"
        } else {
            "senate session pause"
        };
        require_picker_terminal(command, &eligible, true)?;
        let prompt = if paused {
            "Resume which session?"
        } else {
            "Pause which session?"
        };
        return Ok(rt
            .block_on(crate::host::pick(&session_choices(&eligible), prompt))?
            .map(|primary| StopSelector::Named(vec![primary.to_string()])));
    }
    let mut names = Vec::new();
    let paths = Paths::from_env()?;
    for name in which.names {
        names.push(session_senator(&paths, &name)?.to_string());
    }
    Ok(Some(StopSelector::Named(names)))
}

/// How long one pause may take the daemon: its checkpoint wait, the stop,
/// and room for the notes.
fn pause_request_timeout(timeout: Option<u64>) -> Duration {
    Duration::from_secs(timeout.unwrap_or(crate::ipc::PAUSE_TIMEOUT_SECS) + 60)
}

/// `senate session pause`: pauses each chosen session, all at once, and
/// says what it left and why. Fails when any pause failed, or a session
/// named was skipped.
async fn pause_sessions(
    paths: &Paths,
    selector: &crate::sessions::StopSelector,
    force: bool,
    dry_run: bool,
    timeout: Option<u64>,
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
                    "跳过 {}：{} 秒前有人在它的终端输入（加 --force 也暂停）",
                    row.label(),
                    ago
                ),
                format!(
                    "skipped {}: someone typed into it {ago}s ago (--force pauses it too)",
                    row.label()
                ),
            )
        );
    }
    if stop.is_empty() && kept.is_empty() {
        println!(
            "{}",
            locale.pick("没有要暂停的会话。", "No session to pause.")
        );
    }
    if dry_run {
        for row in &stop {
            println!(
                "{}",
                locale.pick(
                    format!("将暂停 {}", row.label()),
                    format!("would pause {}", row.label())
                )
            );
        }
        return Ok(());
    }
    let pauses = stop.iter().map(|row| async move {
        let request = Request::Pause {
            senator: AliasName::bare(row.primary.clone()),
            force,
            timeout_secs: timeout,
        };
        let result: Result<crate::ipc::PauseReply, _> = ipc::request(
            &paths.socket(),
            &request,
            Duration::from_secs(1),
            pause_request_timeout(timeout),
        )
        .await;
        (row, result)
    });
    let mut failed = 0;
    for (row, result) in futures::future::join_all(pauses).await {
        match result {
            Ok(reply) => println!(
                "{}",
                match (reply.checkpoint, locale) {
                    (Some(seq), Locale::Zh) =>
                        format!("已暂停 {}（检查点 seq {seq}）", row.label()),
                    (Some(seq), Locale::En) =>
                        format!("paused {} (checkpoint seq {seq})", row.label()),
                    (None, Locale::Zh) => format!(
                        "已暂停 {}（{} 秒内没有新的检查点）",
                        row.label(),
                        reply.waited_secs
                    ),
                    (None, Locale::En) => format!(
                        "paused {} (no new checkpoint in {}s)",
                        row.label(),
                        reply.waited_secs
                    ),
                }
            ),
            Err(error) => {
                failed += 1;
                eprintln!("{}: {error}", row.label());
            }
        }
    }
    if matches!(selector, crate::sessions::StopSelector::Named(_)) {
        failed += kept.len();
    }
    anyhow::ensure!(failed == 0, "{failed} session(s) could not be paused");
    Ok(())
}

/// `senate session resume`: resumes each chosen paused session, one after
/// another. Fails when any could not be resumed.
async fn resume_sessions(
    paths: &Paths,
    selector: &crate::sessions::StopSelector,
    dry_run: bool,
    locale: Locale,
) -> anyhow::Result<()> {
    let reply: crate::ipc::SessionsReply = daemon_call(paths, &Request::Sessions).await?;
    let chosen =
        crate::sessions::select_for_resume(&reply.rows, selector).map_err(anyhow::Error::msg)?;
    if chosen.is_empty() {
        println!(
            "{}",
            locale.pick("没有要恢复的会话。", "No session to resume.")
        );
    }
    let mut failed = 0;
    for row in chosen {
        if dry_run {
            println!(
                "{}",
                locale.pick(
                    format!("将恢复 {}", row.label()),
                    format!("would resume {}", row.label())
                )
            );
            continue;
        }
        let request = Request::Resume {
            senator: AliasName::bare(row.primary.clone()),
        };
        let result: Result<crate::ipc::ResumeReply, _> = ipc::request(
            &paths.socket(),
            &request,
            Duration::from_secs(1),
            REPLACE_TIMEOUT,
        )
        .await;
        match result {
            Ok(reply) => println!(
                "{}",
                match (reply.checkpoint, locale) {
                    (Some(seq), Locale::Zh) =>
                        format!("已恢复 {}（检查点 seq {seq}）", row.label()),
                    (Some(seq), Locale::En) =>
                        format!("resumed {} (checkpoint seq {seq})", row.label()),
                    (None, Locale::Zh) => format!("已恢复 {}", row.label()),
                    (None, Locale::En) => format!("resumed {}", row.label()),
                }
            ),
            Err(error) => {
                failed += 1;
                eprintln!("{}: {error}", row.label());
            }
        }
    }
    anyhow::ensure!(failed == 0, "{failed} session(s) could not be resumed");
    Ok(())
}

/// Return the picker choice's native session, including paused and stopped rows.
fn pick_attach_session() -> anyhow::Result<Option<ipc::AttachTarget>> {
    let paths = Paths::from_env()?;
    let rt = runtime(false)?;
    let reply = rt.block_on(attach_choices(&paths))?;
    anyhow::ensure!(
        !reply.rows.is_empty(),
        "{}",
        cli_locale().pick(
            "没有托管会话；用 `senate spawn` 启动一个。",
            "no hosted sessions; use senate spawn to start one",
        )
    );
    require_picker_terminal("senate attach", &reply.rows, false)?;
    let chosen = rt.block_on(crate::host::pick(
        &session_choices(&reply.rows),
        "Attach to which session?",
    ))?;
    Ok(chosen
        .and_then(|primary| reply.rows.into_iter().find(|row| row.primary == primary))
        .map(|row| ipc::AttachTarget::Session {
            session_id: row.session_id,
        }))
}

async fn attach_choices(paths: &Paths) -> anyhow::Result<ipc::SessionsReply> {
    Ok(ipc::request(
        &paths.socket(),
        &Request::AttachChoices,
        Duration::from_secs(1),
        Duration::from_secs(2),
    )
    .await?)
}

/// `senate session approve` with no name: the one session whose
/// permission prompt Senate can answer, and the line saying which (and what
/// it asks about). Refused unless the answer was given, when no prompt or
/// several wait.
fn the_waiting_prompt(
    rows: &[ipc::SessionRow],
    answer_given: bool,
    locale: Locale,
) -> Result<(SenatorName, String), String> {
    let waiting: Vec<&ipc::SessionRow> = rows
        .iter()
        .filter(|row| row.prompt.as_ref().is_some_and(|p| !p.screen))
        .collect();
    let about = |row: &ipc::SessionRow| {
        row.prompt
            .as_ref()
            .and_then(|p| p.tool.as_deref())
            .map(crate::host_approval::tool_label)
            .unwrap_or_else(|| locale.pick("（未说明）", "(not said)").to_owned())
    };
    match waiting.as_slice() {
        [] => Err(locale
            .pick(
                "没有托管会话在等权限请求的回答；`senate session list` 列出会话。",
                "No hosted session has a permission prompt waiting; `senate session list` lists them.",
            )
            .to_owned()),
        [row] if answer_given => {
            let name = row.display_name(false);
            let about = about(row);
            Ok((
                row.primary.clone(),
                match locale {
                    Locale::Zh => format!("回答 {name} 的权限请求：{about}"),
                    Locale::En => format!("Answering {name}'s prompt: {about}"),
                },
            ))
        }
        [row] => {
            let name = row.display_name(false);
            let about = about(row);
            Err(match locale {
                Locale::Zh => format!(
                    "{name} 在等权限请求：{about}。不写名字时要写明回答：`senate session approve --once`、`--always` 或 `--deny`（或 `senate session approve {name}`）。"
                ),
                Locale::En => format!(
                    "{name} has a prompt waiting: {about}. Without a name, give the answer: \
                     `senate session approve --once`, `--always` or `--deny` (or `senate session approve {name}`)."
                ),
            })
        }
        several => {
            let list = several
                .iter()
                .map(|row| format!("\n  {}  {}", row.display_name(false), about(row)))
                .collect::<String>();
            Err(match locale {
                Locale::Zh => format!("有 {} 个会话在等权限请求；写出名字：{list}", several.len()),
                Locale::En => format!(
                    "{} sessions have a prompt waiting; name one (`senate session approve <name>`):{list}",
                    several.len()
                ),
            })
        }
    }
}

/// How to add a launch profile, for `senate profiles` with none.
fn profiles_example(config: &std::path::Path, locale: Locale) -> String {
    let example = "[profiles.review]\nruntime = \"codex\"\neffort = \"high\"\nnote = \"Independent diff review.\"";
    match locale {
        Locale::Zh => format!(
            "在 {} 里加一个，例如：\n\n{example}\n\n然后重启 daemon，用 `senate spawn --profile review` 启动。",
            config.display()
        ),
        Locale::En => format!(
            "Add one to {}, for example:\n\n{example}\n\nthen restart the daemon and start it \
             with `senate spawn --profile review`.",
            config.display()
        ),
    }
}

/// What one committee being archived hides, for the confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ArchivePlan {
    committee: CommitteeId,
    members: Vec<String>,
    pending: usize,
    /// Hosted sessions here on it, `(name, running)`.
    sessions: Vec<(String, bool)>,
}

/// `senate committees archive`: what will be hidden, one block each.
fn render_archive_plan(plans: &[ArchivePlan], locale: Locale) -> String {
    let mut out = String::new();
    for plan in plans {
        let members = if plan.members.is_empty() {
            "-".to_owned()
        } else {
            plan.members.join(", ")
        };
        let stopped = plan
            .sessions
            .iter()
            .filter(|(_, running)| !running)
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        let sessions = if stopped.is_empty() {
            "-".to_owned()
        } else {
            stopped.join(", ")
        };
        out.push_str(&match locale {
            Locale::Zh => format!(
                "{}\n  成员：{members}\n  待处理的 thread：{}\n  已停止的托管会话（记录保留）：{sessions}\n",
                plan.committee, plan.pending
            ),
            Locale::En => format!(
                "{}\n  members: {members}\n  open threads: {}\n  stopped hosted sessions (their records stay): {sessions}\n",
                plan.committee, plan.pending
            ),
        });
    }
    out
}

/// The running hosted sessions that keep `plans` from being archived, with
/// how to stop each; `None` when there are none.
fn archive_blockers(plans: &[ArchivePlan], locale: Locale) -> Option<String> {
    let running: Vec<String> = plans
        .iter()
        .flat_map(|plan| plan.sessions.iter().filter(|(_, running)| *running))
        .map(|(name, _)| format!("`senate session stop {name}`"))
        .collect();
    (!running.is_empty()).then(|| {
        locale.pick(
            format!("还有托管会话在运行；先停止它们：{}。", running.join(", ")),
            format!(
                "Hosted sessions are still running on them; stop them first: {}.",
                running.join(", ")
            ),
        )
    })
}

fn render_archived(info: &senate_core::api::CommitteeArchiveInfo, locale: Locale) -> String {
    let c = &info.committee;
    match (info.archived, info.seq, locale) {
        (true, Some(seq), Locale::Zh) => format!("已归档 {c}（seq {seq}）"),
        (true, Some(seq), Locale::En) => format!("archived {c} (seq {seq})"),
        (true, None, Locale::Zh) => format!("{c} 已经是归档的"),
        (true, None, Locale::En) => format!("{c} was archived already"),
        (false, Some(seq), Locale::Zh) => format!("已取消归档 {c}（seq {seq}）"),
        (false, Some(seq), Locale::En) => format!("unarchived {c} (seq {seq})"),
        (false, None, Locale::Zh) => format!("{c} 没有归档"),
        (false, None, Locale::En) => format!("{c} was not archived"),
    }
}

/// `senate committees archive <c>...`: check each, show what is hidden,
/// ask once on a terminal (unless `--yes`), then archive each.
fn archive_committees(
    paths: &Paths,
    committees: Vec<CommitteeId>,
    yes: bool,
    locale: Locale,
) -> anyhow::Result<()> {
    let rt = runtime(false)?;
    let roster: Vec<ipc::RosterCommittee> = rt.block_on(daemon_call(paths, &Request::Roster))?;
    let archived: Vec<CommitteeId> =
        rt.block_on(daemon_call(paths, &Request::ArchivedCommittees))?;
    let sessions: ipc::SessionsReply = rt.block_on(daemon_call(paths, &Request::Sessions))?;
    let mut plans = Vec::new();
    for committee in committees {
        if archived.contains(&committee) {
            println!(
                "{}",
                locale.pick(
                    format!("{committee} 已经是归档的"),
                    format!("{committee} was archived already")
                )
            );
            continue;
        }
        let Some(row) = roster.iter().find(|row| row.committee == committee) else {
            anyhow::bail!(
                "{}",
                locale.pick(
                    format!("没有委员会 {committee}；`senate committees` 列出它们。"),
                    format!("no committee {committee}; `senate committees` lists them")
                )
            );
        };
        plans.push(ArchivePlan {
            committee: committee.clone(),
            members: row.members.iter().map(|m| m.display_name.clone()).collect(),
            pending: row.pending,
            sessions: sessions
                .rows
                .iter()
                .filter(|s| s.committee.as_ref() == Some(&committee))
                .map(|s| (s.label(), s.is_running()))
                .collect(),
        });
    }
    if plans.is_empty() {
        return Ok(());
    }
    if let Some(blockers) = archive_blockers(&plans, locale) {
        anyhow::bail!("{blockers}");
    }
    if !yes {
        anyhow::ensure!(
            on_a_terminal(),
            "{}",
            locale.pick(
                "没有终端可以确认；加上 --yes 直接归档。",
                "There is no terminal to confirm on; add --yes to archive them."
            )
        );
        print!("{}", render_archive_plan(&plans, locale));
        print!(
            "{}",
            locale.pick(
                format!("归档这 {} 个委员会？[y/N] ", plans.len()),
                format!("Archive {} committee(s)? [y/N] ", plans.len())
            )
        );
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("{}", locale.pick("没有归档。", "Nothing archived."));
            return Ok(());
        }
    }
    for plan in plans {
        let info: senate_core::api::CommitteeArchiveInfo = rt.block_on(daemon_call(
            paths,
            &Request::ArchiveCommittee {
                committee: plan.committee,
                archived: true,
                id: None,
            },
        ))?;
        println!("{}", render_archived(&info, locale));
    }
    Ok(())
}

/// What `senate senator chair` did: who chaired `committee` before, who now.
fn render_chair_change(
    committee: &CommitteeId,
    before: Option<&str>,
    now: Option<&str>,
    locale: Locale,
) -> String {
    let now = now.unwrap_or("-");
    match (before, locale) {
        (Some(before), Locale::Zh) if before == now => {
            format!("{committee} 的主席仍是 {now}。")
        }
        (Some(before), Locale::En) if before == now => {
            format!("{now} already chairs {committee}.")
        }
        (Some(before), Locale::Zh) => {
            format!("{committee} 的主席：{before} → {now}（{before} 仍是成员）。")
        }
        (Some(before), Locale::En) => format!(
            "Chair of {committee}: {before} → {now} ({before} remains a member; \
             `senate senator unseat {before} --committee {committee}` removes it)."
        ),
        (None, Locale::Zh) => format!("{committee} 的主席：（无）→ {now}。"),
        (None, Locale::En) => format!("Chair of {committee}: (none) → {now}."),
    }
}

/// What `senate session find` did.
fn render_found(found: &ipc::FindReply, locale: Locale) -> String {
    let name = &found.name;
    let device = found
        .tty
        .as_deref()
        .map(|t| t.trim_start_matches("/dev/").to_owned());
    match (found.attached, device, locale) {
        (true, Some(d), Locale::Zh) => {
            format!("{name} 在 {d} 打开；那个终端现在会响铃，标题闪烁「Senate: this terminal」。")
        }
        (true, Some(d), Locale::En) => format!(
            "{name} is attached in {d}: that terminal now rings and shows \"Senate: this terminal\" in its title."
        ),
        (true, None, Locale::Zh) => {
            format!(
                "{name} 已在一个终端里打开；那个终端现在会响铃，标题闪烁「Senate: this terminal」。"
            )
        }
        (true, None, Locale::En) => format!(
            "{name} is attached in a terminal: it now rings and shows \"Senate: this terminal\" in its title."
        ),
        (false, _, Locale::Zh) => {
            format!("{name} 没有在任何终端里打开；`senate attach {name}` 在这里打开它。")
        }
        (false, _, Locale::En) => {
            format!("{name} is not attached in any terminal; `senate attach {name}` opens it here.")
        }
    }
}

/// `senate attach` refused: another terminal holds the session. It names
/// that terminal (its device, and when someone last typed in it), says the
/// terminal now rings and flashes its title, and what to do.
fn attached_elsewhere(
    name: &str,
    tty: Option<&str>,
    typed_secs_ago: Option<u64>,
    locale: Locale,
) -> String {
    let device = tty.map(|t| t.trim_start_matches("/dev/").to_owned());
    let typed = typed_secs_ago.map(crate::sessions::short);
    let details = match (device, typed, locale) {
        (Some(d), Some(t), Locale::Zh) => format!("（{d}，{t}前有人输入）"),
        (Some(d), Some(t), Locale::En) => format!(" ({d}, last typed {t} ago)"),
        (Some(d), None, Locale::Zh) => format!("（{d}）"),
        (Some(d), None, Locale::En) => format!(" ({d})"),
        (None, Some(t), Locale::Zh) => format!("（{t}前有人输入）"),
        (None, Some(t), Locale::En) => format!(" (last typed {t} ago)"),
        (None, None, _) => String::new(),
    };
    match locale {
        Locale::Zh => format!(
            "{name} 已经在另一个终端里打开{details}；那个终端现在会响铃，标题闪烁「Senate: this terminal」。在那里按 Ctrl-] 断开，或用 `senate attach {name} --read-only` 在这里旁观。"
        ),
        Locale::En => format!(
            "{name} is already attached in another terminal{details}; that terminal now rings and shows \
             \"Senate: this terminal\" in its title. Detach it there with Ctrl-], or watch it here with \
             `senate attach {name} --read-only`."
        ),
    }
}

/// `senate spawn` with neither a runtime nor --profile: what to add.
fn no_runtime(profiles: &[String], locale: Locale) -> String {
    let first = match locale {
        Locale::Zh => {
            "要启动哪个 agent？写出运行时：`senate spawn claude`（或 codex、agy），".to_owned()
        }
        Locale::En => {
            "Which agent? Name a runtime: `senate spawn claude` (or codex, agy), ".to_owned()
        }
    };
    match (profiles.first(), locale) {
        (None, Locale::Zh) => {
            format!("{first}或用 --profile 选一个启动配置（`senate profiles`，目前没有）。")
        }
        (None, Locale::En) => format!(
            "{first}or a launch profile with --profile (`senate profiles`; none is configured yet)."
        ),
        (Some(example), Locale::Zh) => format!(
            "{first}或 `senate spawn --profile {example}`。启动配置：{}。",
            profiles.join(", ")
        ),
        (Some(example), Locale::En) => format!(
            "{first}or `senate spawn --profile {example}`. Launch profiles: {}.",
            profiles.join(", ")
        ),
    }
}

/// The picker's lines for `rows`, each named as `senate session list` names
/// it (its alias, or the short senator name), its committee leading the
/// rest of the line.
fn session_choices(rows: &[ipc::SessionRow]) -> Vec<crate::host::Choice> {
    rows.iter()
        .map(|row| {
            let mut choice = crate::host::Choice::of_row(row);
            choice.label = row.display_name(false);
            if let Some(committee) = &row.committee
                && !choice.label.contains("::")
            {
                choice.detail = format!("{committee} · {}", choice.detail);
            }
            choice
        })
        .collect()
}

fn on_a_terminal() -> bool {
    use std::io::IsTerminal as _;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

fn cli_locale() -> Locale {
    Paths::from_env().map_or(Locale::En, |paths| BridgeConfig::locale_at(&paths.config()))
}

/// `--attach` on spawn, restore or replace: refused before anything starts
/// when there is no terminal to attach, saying how to open it later.
fn require_attach_terminal() -> anyhow::Result<()> {
    anyhow::ensure!(
        on_a_terminal(),
        "{}",
        cli_locale().pick(
            "--attach 需要 stdin 和 stdout 都是终端。去掉 --attach，之后在终端里用 `senate attach <名字>` 打开它。",
            "--attach needs a terminal on stdin and stdout. Leave it out, then open the session \
             from a terminal with `senate attach <name>`.",
        )
    );
    Ok(())
}

/// `command` (`senate attach`, `senate session watch`) needs a terminal;
/// `instead` says what works without one.
fn require_terminal(command: &str, instead: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        on_a_terminal(),
        "{}",
        cli_locale().pick(
            format!("`{command}` 需要 stdin 和 stdout 都是终端。{instead}"),
            format!("`{command}` needs a terminal on stdin and stdout. {instead}"),
        )
    );
    Ok(())
}

/// Before a list to choose from: without a terminal, say which sessions
/// `command` could take and how to name one.
fn require_picker_terminal(
    command: &str,
    rows: &[ipc::SessionRow],
    all_flag: bool,
) -> anyhow::Result<()> {
    if on_a_terminal() {
        return Ok(());
    }
    anyhow::bail!("{}", picker_refusal(command, rows, all_flag, cli_locale()))
}

fn picker_refusal(
    command: &str,
    rows: &[ipc::SessionRow],
    all_flag: bool,
    locale: Locale,
) -> String {
    let names: Vec<String> = rows.iter().map(|row| row.display_name(false)).collect();
    let example = names.first().map_or("<name>", String::as_str);
    let all = if all_flag {
        locale.pick("（或 --all）", " (or --all)")
    } else {
        ""
    };
    let list = names.join(", ");
    match locale {
        Locale::Zh => format!(
            "不给名字时，`{command}` 在终端里列出会话供选择，而这里没有终端。请写出名字：`{command} {example}`{all}。可选：{list}。"
        ),
        Locale::En => format!(
            "Without a name, `{command}` lets you choose from a list, which needs a terminal. \
             Name one instead: `{command} {example}`{all}. To choose from: {list}."
        ),
    }
}

async fn attach_launched(paths: &Paths, session_id: String, label: &str) -> anyhow::Result<()> {
    let info: ipc::AttachInfo = daemon_call(
        paths,
        &Request::AttachInfo {
            target: ipc::AttachTarget::Session { session_id },
        },
    )
    .await?;
    let tty = info
        .tty
        .context("the session launched but its terminal is no longer running")?;
    crate::host::attach(&tty, paths, label).await
}

async fn attach_target(
    paths: &Paths,
    target: ipc::AttachTarget,
    label: Option<String>,
    read_only: bool,
) -> anyhow::Result<()> {
    use std::io::Write as _;
    let selector = match &target {
        ipc::AttachTarget::Senator { senator } => senator.to_string(),
        ipc::AttachTarget::Session { session_id } => session_id.clone(),
    };
    let info: ipc::AttachInfo = ipc::request(
        &paths.socket(),
        &Request::AttachInfo { target },
        Duration::from_secs(1),
        Duration::from_secs(6),
    )
    .await
    .with_context(|| format!("look up terminal for {selector}"))?;
    let label = label.unwrap_or_else(|| info.primary.to_string());
    if let Some(tty) = info.tty {
        if read_only {
            return crate::host::view::watch_one(crate::host::view::Session {
                title: label.clone(),
                key: label.clone(),
                alias: label,
                attached: false,
                tty,
                dim: false,
            })
            .await;
        }
        return match crate::host::attach(&tty, paths, &label).await {
            Err(error) if error.downcast_ref::<crate::host::Busy>().is_some() => {
                let rows: ipc::SessionsReply = daemon_call(paths, &Request::Sessions).await?;
                let row = rows.rows.iter().find(|r| r.session_id == info.session_id);
                let name = row.map_or_else(|| label.clone(), ipc::SessionRow::label);
                Err(anyhow::anyhow!(
                    "{}",
                    attached_elsewhere(
                        &name,
                        row.and_then(|r| r.viewer_tty.as_deref()),
                        row.and_then(|r| r.typed_secs_ago),
                        BridgeConfig::locale_at(&paths.config()),
                    )
                ))
            }
            other => other,
        };
    }
    anyhow::ensure!(
        !read_only,
        "{label}'s terminal is not running; use senate attach {label} to bring it back"
    );
    require_terminal(
        "senate attach",
        cli_locale().pick(
            "在终端里运行它；`senate session list` 不需要终端也能显示它的状态。",
            "Run it in a terminal; `senate session list` shows its state without one.",
        ),
    )?;
    let senator = AliasName::bare(info.primary);
    let locale = BridgeConfig::locale_at(&paths.config());
    let action = if info.paused {
        locale.pick(
            format!("正在恢复暂停的 {label}…"),
            format!("Resuming {label}…"),
        )
    } else {
        locale.pick(
            format!("正在重新打开 {label}…"),
            format!("Restoring {label}…"),
        )
    };
    eprintln!("{action}");
    let native = if info.paused {
        let reply: ipc::ResumeReply = ipc::request(
            &paths.socket(),
            &Request::Resume { senator },
            Duration::from_secs(1),
            REPLACE_TIMEOUT,
        )
        .await?;
        reply
            .session_id
            .context("the daemon did not return the resumed session ID")?
    } else {
        let reply: SpawnReply = ipc::request(
            &paths.socket(),
            &Request::Restore {
                senator,
                session: SessionSelector::default(),
                model: ipc::HostedModel::default(),
            },
            Duration::from_secs(1),
            crate::host::REQUEST_TIMEOUT,
        )
        .await?;
        if let Some(warning) = &reply.model_warning {
            eprintln!("{warning}");
        }
        reply.session_id
    };
    std::io::stdout().flush()?;
    attach_launched(paths, native, &label).await
}

/// Let the person pick one of an alias's `candidates`: on a terminal,
/// in the picker, each line with what `senate session list` knows of that
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
        .block_on(attach_choices(&paths))
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

fn launch(request: &Request, output: Output, attach: bool) -> anyhow::Result<()> {
    let reply: SpawnReply = if matches!(request, Request::Spawn { .. }) {
        let paths = Paths::from_env()?;
        runtime(false)?.block_on(ipc::request_spawn(
            &paths.socket(),
            request,
            crate::host::REQUEST_TIMEOUT,
        ))?
    } else {
        host_call(request)?
    };
    let name = if output.verbose {
        reply.senator.to_string()
    } else {
        reply
            .display_name
            .clone()
            .unwrap_or_else(|| reply.senator.to_string())
    };
    println!("{name}");
    if let Some(resolved) = &reply.resolved_model
        && let Request::Spawn { model, .. } | Request::Restore { model, .. } = request
        && let Some(requested) = &model.model
    {
        eprintln!("{requested} → {resolved}");
    }
    if let Some(warning) = &reply.model_warning {
        eprintln!("{warning}");
    }
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
    if attach {
        use std::io::Write as _;
        std::io::stdout().flush()?;
        let paths = Paths::from_env()?;
        return runtime(false)?.block_on(attach_launched(&paths, reply.session_id, &name));
    }
    if output.terminal.is_some() {
        eprintln!(
            "{}",
            locale.pick(
                format!(
                    "已在后台运行。用 `senate attach {}` 打开它的终端（Ctrl-] 脱离）。",
                    name
                ),
                format!(
                    "Running in the background. Open its terminal with `senate attach {}` (Ctrl-] detaches).",
                    name
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
    let aliases = if verbose {
        None
    } else {
        daemon_call::<KnownAliases>(paths, &Request::Aliases)
            .await
            .ok()
    };
    let mut out = if verbose {
        person::render_inbox(&threads, locale, mcp::ShowAll::Flag)
    } else {
        person::render_named_inbox(&threads, locale, aliases.as_ref())
    };
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
            committee: Some(committee.clone()),
        },
    )
    .await?;
    let awaiting: usize = docket
        .senates
        .iter()
        .flat_map(|senate| &senate.pages)
        .map(|page| page.items.len())
        .sum();
    if !docket.senates.is_empty() && !all && awaiting == 0 {
        // The roster is available via `senate committees`.
        out.push('\n');
        out.push_str(locale.pick("没有等待 Speaker 的事项。", "Nothing awaits the Speaker."));
    } else if !docket.senates.is_empty() {
        let idle = if all { 0 } else { drop_idle_seats(&mut docket) };
        out.push_str("\n\n");
        out.push_str(&preside::render_docket(&docket, locale));
        if idle > 0 {
            out.push_str(&locale.pick(
                format!("另有 {idle} 个不活跃席位；--all 会列出。\n"),
                format!("{idle} inactive seat(s) hidden. Use --all to list them.\n"),
            ));
        }
    }
    if !verbose {
        out = crate::format::display_bylines(&out, aliases.as_ref(), Some(&committee));
    }
    Ok(out)
}

/// Hide inactive seats while retaining the docket's open tasks.
fn drop_idle_seats(docket: &mut DocketReply) -> usize {
    let mut idle = 0;
    for page in docket.senates.iter_mut().flat_map(|s| s.pages.iter_mut()) {
        let before = page.seats.len();
        page.seats.retain(|seat| {
            senate_core::continuity::roster_active(
                if seat.connected {
                    Some(senate_core::PresenceState::Online)
                } else {
                    seat.presence
                },
                false,
                seat.paused,
            )
        });
        idle += before - page.seats.len();
    }
    idle
}

/// `senate inbox --follow`: what is addressed to the person, as it arrives.
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
        person::render_watching(
            committee.as_ref(),
            pong.locale,
            crate::format::Clock::for_stdout()
        )
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
        let line = if json {
            serde_json::to_string(&event)?
        } else {
            let markdown = crate::format::terminal_markdown();
            let text = person::render_news_with(
                news,
                *escalated,
                text,
                pong.locale,
                markdown.as_ref().map(|m| m as &dyn Fn(&str) -> String),
                crate::format::Clock::for_stdout(),
            );
            let text = if output.verbose {
                text
            } else {
                let aliases: Option<KnownAliases> =
                    daemon_call(paths, &Request::Aliases).await.ok();
                crate::format::display_bylines(&text, aliases.as_ref(), Some(&news.committee))
            };
            format!("{}\n", output.text(&text))
        };
        let mut out = std::io::stdout().lock();
        writeln!(out, "{line}")?;
        out.flush()?;
    }
}

/// The words of a message, or stdin when they are a lone `-`.
pub(super) fn message_text(words: Vec<String>) -> anyhow::Result<String> {
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
        let (committee, elsewhere) = committee_for(&paths, committee, &to, cli_locale()).await?;
        let socket = paths.socket();
        let mut sent = speak::send(&socket, committee, kind, &to, text).await?;
        sent.elsewhere = elsewhere;
        let aliases = if !json && !output.verbose {
            daemon_call::<KnownAliases>(&paths, &Request::Aliases)
                .await
                .ok()
        } else {
            None
        };
        let display = |line: &str| {
            if output.verbose {
                line.to_owned()
            } else {
                senate_core::alias::display_mentions(
                    line,
                    aliases.as_ref(),
                    Some(&sent.posted.committee),
                )
            }
        };
        let Some(secs) = wait else {
            if json {
                return print_json(&sent.posted);
            }
            output.print(&display(&sent.render(false)));
            return Ok(());
        };
        if !json {
            output.print(&display(&sent.render(true)));
            std::io::stdout().flush()?;
        }
        match speak::wait_answer(&socket, &sent, Duration::from_secs(secs)).await? {
            Some(answer) if json => {
                print_json(&serde_json::json!({ "posted": sent.posted, "answer": answer }))
            }
            Some(answer) => {
                let text = speak::render_arrival(&answer, sent.locale);
                if let Some((heading, body)) = text.split_once('\n') {
                    println!("{}\n{body}", display(heading));
                } else {
                    println!("{}", display(&text));
                }
                Ok(())
            }
            None => anyhow::bail!(speak::render_timeout(sent.posted.seq, secs, sent.locale)),
        }
    })
}

fn service_spec() -> anyhow::Result<(service::Spec, Locale)> {
    let platform = service::Platform::current()
        .context("`senate setup service` supports macOS (launchd) and Linux (systemd)")?;
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
            // In WSL, without the Windows directories: the daemon must find
            // the Linux `git`, `claude` and `codex`, never a Windows one.
            path: crate::wsl::setup_path(std::env::var("PATH").ok().filter(|p| !p.is_empty())),
            config: std::path::absolute(paths.config()).context("resolving the Senate config")?,
        },
        locale,
    ))
}

/// `senate setup doctor`: every check, printed; a failed one fails the
/// command.
fn run_doctor(json: bool) -> anyhow::Result<()> {
    let paths = Paths::from_env()?;
    let locale = BridgeConfig::locale_at(&paths.config());
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("locating this binary")?;
    let install = install::InstallOptions::resolve(
        exe.clone(),
        None,
        None,
        install::Choice::Auto,
        None,
        install::Choice::Auto,
        None,
    )
    .map(|o| o.with_locale(locale))
    .map_err(|e| format!("{e:#}"));
    let spec = service_spec().ok().map(|(spec, _)| spec);
    let senated = service::locate_senated(&exe, std::env::var_os("PATH").as_deref());
    let wsl = crate::doctor::Wsl::here();
    let mut checks = crate::doctor::local(&crate::doctor::Local {
        paths: &paths,
        locale,
        install: install.as_ref().map_err(Clone::clone),
        service: spec.as_ref(),
        senated: senated.as_deref(),
        manager: &service::SystemManager,
        wsl: wsl.as_ref(),
    });
    checks.extend(completion::doctor(locale));
    checks.extend(crate::remote::authority::installation_checks());
    if let Some(line) = crate::remote::authority::latest_history(&paths) {
        checks.push(crate::doctor::Check::ok("remote.last_administration", line));
    }
    let live = runtime(false)?.block_on(crate::doctor::gather(&paths));
    checks.extend(crate::doctor::live(&live, locale));
    checks.extend(crate::doctor::worktree_checks(
        &live,
        install.as_ref().ok(),
        locale,
    ));
    let report = crate::doctor::Report::new(checks);
    if json {
        print_json(&report)?;
    } else {
        print!("{}", report.render(locale));
    }
    if report.failed() {
        std::process::exit(1);
    }
    Ok(())
}

async fn wait_for_service(paths: &Paths, limit: Duration) -> bool {
    let socket = paths.socket();
    let attempt = Duration::from_millis(500);
    tokio::time::timeout(limit, async {
        loop {
            if ipc::request::<ipc::Pong>(&socket, &Request::Ping, attempt, attempt)
                .await
                .is_ok()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .is_ok()
}

#[test]
#[allow(clippy::unwrap_used)]
fn hosted_launch_commands_parse_attach_in_both_spellings() {
    let spawn = Cli::try_parse_from([
        "senate", "spawn", "codex", "--attach", "--prompt", "fixture",
    ])
    .unwrap();
    assert!(matches!(
        spawn.command,
        Some(Cmd::Spawn { attach: true, .. })
    ));
    for operation in ["replace", "restore"] {
        let cmd =
            Cli::try_parse_from(["senate", "session", operation, "refund-v2::reviewer", "-a"])
                .unwrap();
        assert!(matches!(
            cmd.command,
            Some(Cmd::Session {
                command: SessionCmd::Restore { attach: true, .. }
                    | SessionCmd::Replace { attach: true, .. }
            })
        ));
    }
}

#[cfg(test)]
#[tokio::test]
async fn service_readiness_requires_a_pong_before_the_deadline() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    let home = tempfile::tempdir_in("/tmp").expect("test home");
    let paths = Paths::new(home.path());
    let server = async {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let listener = UnixListener::bind(paths.socket()).expect("bind socket");
        let (socket, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
            .await
            .expect("install must ping the socket")
            .expect("client");
        let mut socket = BufReader::new(socket);
        let mut line = String::new();
        socket.read_line(&mut line).await.expect("read request");
        assert!(matches!(serde_json::from_str(&line), Ok(Request::Ping)));
        let pong = ipc::Reply::ok(
            &serde_json::json!({"protocol":1,"pid":1,"person":"alice","locale":"en","version":"test"}),
        );
        let response = format!("{}\n", serde_json::to_string(&pong).expect("encode reply"));
        socket
            .get_mut()
            .write_all(response.as_bytes())
            .await
            .expect("send pong");
        listener
    };
    let (ready, listener) = tokio::join!(wait_for_service(&paths, Duration::from_secs(3)), server);
    assert!(ready);
    // A listening socket without a responding daemon still times out.
    assert!(!wait_for_service(&paths, Duration::from_millis(100)).await);
    drop(listener);
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
                let paths = Paths::new(&spec.senate_home);
                if !runtime(false)?.block_on(wait_for_service(&paths, Duration::from_secs(5))) {
                    anyhow::bail!(
                        "{} {}",
                        locale.pick(
                            "服务已安装，但守护进程未在 5 秒内响应。套接字：",
                            "Service installed, but the daemon did not answer within 5 seconds on"
                        ),
                        paths.socket().display()
                    );
                }
            }
            print!(
                "{}",
                service::render_install(&plan, spec.platform, yes, locale)
            );
            if let Some(distro) = crate::wsl::distro() {
                print!("{}", crate::wsl::notes(&distro, locale));
            }
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
                    "请重新运行 senate setup service install --yes，以便托管终端在 systemd 重启 Senate 守护进程时继续运行。",
                    "Run senate setup service install --yes to update the systemd unit so hosted terminals survive a daemon restart.",
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
        Cmd::Session { command } => run_session(command, output),
        Cmd::Senator { command } => run_senator(command, output),
        Cmd::Secretary(args) => cli_secretary::run(args),
        Cmd::Desk(args) => cli_desk::run(args),
        Cmd::Chat(args) => cli_chat::run(args),
        Cmd::Observe(args) => cli_observe::run(args),
        Cmd::Setup { command } => run_setup(command, output),
        Cmd::Join {
            committee,
            alias,
            duty,
        } => {
            let (paths, settings) = settings_here();
            let cwd = std::env::current_dir()?;
            let alias = alias.as_deref().map(binding::parse_pin).transpose()?;
            if duty.is_some() {
                cli_legacy::point("join --duty", "senator duty");
            }
            // The session is found before anything is written, so a duty
            // that cannot be set leaves the worktree as it was. An alias
            // goes to the calling session only when there is one.
            let session = match (&duty, &alias) {
                (Some(_), _) => Some(calling_session(None, settings.locale)?),
                (None, Some(_)) => session_from_env(),
                (None, None) => None,
            };
            let rt = runtime(false)?;
            // With an alias and no name, a worktree that has a committee
            // keeps it: only the alias is new.
            let kept = match (&alias, &committee) {
                (Some(_), None) => rt
                    .block_on(binding::resolve(&paths, &cwd, &settings))
                    .committee
                    .map(|(committee, _)| committee),
                _ => None,
            };
            // A committee archived is not joined: it takes no new seats.
            if let Some(named) = committee.as_ref()
                && rt
                    .block_on(daemon_call::<Vec<CommitteeId>>(
                        &paths,
                        &Request::ArchivedCommittees,
                    ))
                    .is_ok_and(|archived| archived.iter().any(|c| c.as_str() == named.as_str()))
            {
                anyhow::bail!(
                    "{}",
                    crate::daemon::archived_refusal(
                        &CommitteeId::new(named.as_str())?,
                        settings.locale
                    )
                );
            }
            let committee = match kept {
                Some(committee) => Some(committee),
                None => {
                    let (change, choice) =
                        rt.block_on(binding::join_choosing(&paths, &cwd, committee, &settings))?;
                    if let crate::joinname::Choice::Derived(d) = &choice {
                        println!("{}", binding::render_derived(d, settings.locale));
                    }
                    println!("{}", binding::render_join(&change, settings.locale));
                    let committee = change.value.as_deref().map(CommitteeId::new).transpose()?;
                    if let Some(committee) = &committee {
                        println!(
                            "{}",
                            rt.block_on(open_committee(&paths, committee.clone(), settings.locale))
                        );
                    }
                    committee
                }
            };
            if let Some(alias) = &alias {
                let pinned = rt.block_on(binding::change(
                    &paths,
                    &cwd,
                    binding::Item::Agent,
                    Some(alias.to_string()),
                    &settings,
                ))?;
                println!("{}", binding::render_pin(&pinned, settings.locale));
                if let (Some(session), Some(committee)) = (&session, &committee) {
                    let line = rt.block_on(alias_session(
                        &paths,
                        session.clone(),
                        committee.clone(),
                        alias.clone(),
                        settings.locale,
                    ))?;
                    output.print(&line);
                }
            }
            if let (Some(duty), Some(session), Some(committee)) = (duty, session, committee) {
                let line =
                    rt.block_on(set_duty(&paths, session, committee, duty, settings.locale))?;
                println!("{line}");
            }
            Ok(())
        }
        Cmd::Leave { alias } => {
            let (paths, settings) = settings_here();
            let cwd = std::env::current_dir()?;
            let item = if alias {
                binding::Item::Agent
            } else {
                binding::Item::Committee
            };
            let change =
                runtime(false)?.block_on(binding::change(&paths, &cwd, item, None, &settings))?;
            println!(
                "{}",
                if alias {
                    binding::render_unpin(&change, settings.locale)
                } else {
                    binding::render_leave(&change, settings.locale)
                }
            );
            Ok(())
        }
        Cmd::Where { all: true, json } => {
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
        Cmd::Where { all: false, .. } => {
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
            // Bound to a committee since archived: said, with the way out.
            if let Some((committee, _)) = &resolved.committee
                && rt
                    .block_on(daemon_call::<Vec<CommitteeId>>(
                        &paths,
                        &Request::ArchivedCommittees,
                    ))
                    .is_ok_and(|archived| archived.contains(committee))
            {
                println!(
                    "{}",
                    settings.locale.pick(
                        format!(
                            "这里绑定的委员会 {committee} 已归档：用 `senate committees unarchive {committee}` 恢复，或 `senate join <另一个>`。"
                        ),
                        format!(
                            "This worktree is bound to {committee}, which is archived: \
                             `senate committees unarchive {committee}` restores it, or `senate join <another>`."
                        ),
                    )
                );
            }
            Ok(())
        }
        Cmd::Inbox {
            committee,
            follow: true,
            global,
            json,
            ..
        } => {
            let paths = Paths::from_env()?;
            runtime(false)?.block_on(async {
                let committee = DisplayScope { global, committee }.resolve(&paths).await?;
                watch(&paths, committee, json, output).await
            })
        }
        Cmd::Inbox {
            committee,
            all,
            mark_read,
            ..
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
                    let mut text = speak::render_replied(Seq(thread), &posted, locale);
                    if !verbose {
                        let aliases: Option<KnownAliases> =
                            daemon_call(&paths, &Request::Aliases).await.ok();
                        text = senate_core::alias::display_mentions(
                            &text,
                            aliases.as_ref(),
                            Some(&posted.committee),
                        );
                    }
                    output.print(&text);
                    Ok(())
                }
            })
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
                    let mut text = match crate::format::terminal_markdown() {
                        Some(markdown) => speak::render_thread_with(
                            &reply,
                            locale,
                            &markdown,
                            crate::format::Clock::for_stdout(),
                        ),
                        None => speak::render_thread(&reply, locale),
                    };
                    if !verbose {
                        let aliases: Option<KnownAliases> =
                            daemon_call(&paths, &Request::Aliases).await.ok();
                        text = crate::format::display_bylines(
                            &text,
                            aliases.as_ref(),
                            Some(&reply.committee),
                        );
                    }
                    output.print(&text);
                    Ok(())
                }
            })
        }
        Cmd::Memory {
            action,
            words,
            all,
            committee,
            json,
        } => {
            let paths = Paths::from_env()?;
            let locale = BridgeConfig::locale_at(&paths.config());
            let edit = match action {
                None => None,
                Some(MemoryCmd::Forget { seqs, committee }) => Some((
                    committee,
                    seqs.into_iter().map(Seq).collect::<Vec<_>>(),
                    None,
                )),
                Some(MemoryCmd::Correct {
                    seq,
                    text,
                    committee,
                }) => Some((committee, vec![Seq(seq)], Some(text.join(" ")))),
            };
            if let Some((at, targets, text)) = edit {
                let posted: Posted = runtime(false)?.block_on(async {
                    let committee = committee_here(at.or(committee)).await?;
                    daemon_call(
                        &paths,
                        &Request::MemoryEdit {
                            session: SessionSelector::default(),
                            committee: Some(committee),
                            targets,
                            text,
                            id: Some(EventId::new()),
                        },
                    )
                    .await
                })?;
                output.print(&match locale {
                    Locale::Zh => {
                        format!("已更新 {} 的记忆（seq {}）。", posted.committee, posted.seq)
                    }
                    Locale::En => format!(
                        "Memory updated on {} (seq {}).",
                        posted.committee, posted.seq
                    ),
                });
                return Ok(());
            }
            let reply: ipc::MemoryReply = runtime(false)?.block_on(async {
                let committee = committee_here(committee).await?;
                daemon_call(
                    &paths,
                    &Request::Memory {
                        session: SessionSelector::default(),
                        query: words.join(" "),
                        limit: None,
                        all,
                        committee: Some(committee),
                    },
                )
                .await
            })?;
            if json {
                print_json(&reply)
            } else {
                output.print(&mcp::render_memory(&reply, locale));
                Ok(())
            }
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
        Cmd::Profiles => {
            let paths = Paths::from_env()?;
            let config = BridgeConfig::load_unchecked(&paths.config())?;
            if config.profiles.is_empty() {
                println!("No launch profiles configured.");
                // The how-to goes to stderr: stdout stays as scripts read it.
                eprintln!(
                    "{}",
                    profiles_example(&paths.config(), BridgeConfig::locale_at(&paths.config()))
                );
            }
            for (name, profile) in config.profiles {
                println!(
                    "{name}\t{}\t{}\t{}\t{}",
                    crate::session::Runtime::from(profile.runtime).as_str(),
                    profile.model.model.as_deref().unwrap_or("default"),
                    profile.model.effort.as_deref().unwrap_or("default"),
                    profile.note
                );
            }
            Ok(())
        }
        Cmd::Spawn {
            attempt_id,
            runtime: harness,
            profile,
            committee,
            alias,
            chair,
            secretary,
            dir,
            prompt,
            prompt_file,
            model,
            attach,
        } => {
            if harness.is_none() && profile.is_none() {
                let profiles = Paths::from_env()
                    .ok()
                    .and_then(|paths| BridgeConfig::load_unchecked(&paths.config()).ok())
                    .map(|config| config.profiles.into_keys().collect::<Vec<_>>())
                    .unwrap_or_default();
                anyhow::bail!("{}", no_runtime(&profiles, cli_locale()));
            }
            if attach {
                require_attach_terminal()?;
            }
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
            let dir = std::path::absolute(dir)?;
            let committee = runtime(false)?.block_on(spawn_committee(committee, &dir))?;
            let (runtime, profile) = match profile {
                Some(name) => (
                    None,
                    Some(ipc::SpawnProfile {
                        name,
                        runtime: harness,
                    }),
                ),
                None => (harness, None),
            };
            launch(
                &Request::Spawn {
                    attempt_id: Some(attempt_id.unwrap_or_else(ipc::new_spawn_attempt_id)),
                    runtime,
                    profile,
                    committee,
                    chair,
                    secretary,
                    alias,
                    cwd: dir,
                    session: SessionSelector::default(),
                    prompt,
                    model: model.into(),
                },
                output,
                attach,
            )
        }
        Cmd::Attach {
            machine,
            senator,
            session,
            read_only,
        } => {
            let paths = Paths::from_env()?;
            if let Some(machine) = machine {
                use remote_cli::AttachSelector;
                let target = if let Some(written) = senator {
                    session_role::resolve(&paths, &written, Some(&machine))?
                        .map_or(AttachSelector::Name(written), |holder| {
                            AttachSelector::Role(holder.candidates)
                        })
                } else {
                    AttachSelector::Name(session.context("remote attach needs a session name")?)
                };
                return runtime(false)?
                    .block_on(remote_cli::attach(&paths, &machine, target, read_only));
            }
            let (target, label) = match (session, senator) {
                (Some(session_id), _) => (ipc::AttachTarget::Session { session_id }, None),
                (None, Some(name)) => {
                    let target = match session_role::resolve(&paths, &name, None)? {
                        Some(holder) => ipc::AttachTarget::Session {
                            session_id: holder
                                .session_id
                                .context("the role has no local hosted session")?,
                        },
                        None => ipc::AttachTarget::Senator {
                            senator: pick_alias(name.parse()?)?,
                        },
                    };
                    (target, Some(name))
                }
                (None, None) => match pick_attach_session()? {
                    Some(target) => (target, None),
                    None => return Ok(()),
                },
            };
            runtime(false)?.block_on(attach_target(&paths, target, label, read_only))
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
            prompt_quiet_ms,
        } => {
            let paths = Paths::from_env()?;
            let model = crate::ipc::HostedModel::new(model, effort);
            let prompt_quiet = prompt_quiet_ms
                .map_or(crate::host::prompt::QUIET, std::time::Duration::from_millis);
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
                    prompt_quiet,
                },
            ))
        }
        Cmd::Mcp {
            runtime: flag,
            agy,
            acp_session,
        } => {
            init_logging("senate_bridge=warn");
            let paths = Paths::from_env()?;
            let locale = BridgeConfig::locale_at(&paths.config());
            let agy = RuntimeFlag::chosen(flag, false, agy) == Some(Runtime::Agy);
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
        Cmd::Committees { archived, action } => {
            let paths = Paths::from_env()?;
            let locale = BridgeConfig::locale_at(&paths.config());
            match action {
                Some(CommitteesCmd::Archive { committees, yes }) => {
                    archive_committees(&paths, committees, yes, locale)
                }
                Some(CommitteesCmd::Unarchive { committees }) => {
                    let rt = runtime(false)?;
                    for committee in committees {
                        let info: senate_core::api::CommitteeArchiveInfo =
                            rt.block_on(daemon_call(
                                &paths,
                                &Request::ArchiveCommittee {
                                    committee: committee.clone(),
                                    archived: false,
                                    id: None,
                                },
                            ))?;
                        println!("{}", render_archived(&info, locale));
                    }
                    Ok(())
                }
                None if archived => {
                    let list: Vec<CommitteeId> = runtime(false)?
                        .block_on(daemon_call(&paths, &Request::ArchivedCommittees))?;
                    if list.is_empty() {
                        println!(
                            "{}",
                            locale.pick("没有已归档的委员会。", "No archived committees.")
                        );
                    }
                    for committee in list {
                        println!("{committee}");
                    }
                    Ok(())
                }
                None => {
                    let roster: Vec<ipc::RosterCommittee> =
                        runtime(false)?.block_on(daemon_call(&paths, &Request::Roster))?;
                    println!("{}", status::render_committees(&roster, locale, verbose));
                    Ok(())
                }
            }
        }
        Cmd::Alias {
            senator,
            alias,
            committee,
        } => {
            let paths = Paths::from_env()?;
            let alias: SenatorName = runtime(false)?.block_on(daemon_call(
                &paths,
                &Request::SetAlias {
                    senator,
                    alias,
                    committee,
                },
            ))?;
            println!("{alias}");
            Ok(())
        }
        Cmd::Stats { since, json } => {
            let days = since
                .strip_suffix('d')
                .and_then(|n| n.parse::<i64>().ok())
                .filter(|days| (1..=crate::stats::MAX_DAYS).contains(days))
                .context("--since must be 1d through 366d")?;
            let zone = iana_time_zone::get_timezone()
                .ok()
                .and_then(|tz| tz.parse::<chrono_tz::Tz>().ok())
                .unwrap_or(chrono_tz::UTC);
            let to = chrono::Utc::now().with_timezone(&zone).date_naive();
            let from = to - chrono::Duration::days(days - 1);
            let paths = Paths::from_env()?;
            let reply: crate::stats::StatsReply = runtime(false)?.block_on(daemon_call(
                &paths,
                &Request::Stats {
                    from,
                    to,
                    tz: zone.name().into(),
                },
            ))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&reply)?);
            } else {
                print!("{}", crate::stats::summary(&reply));
            }
            Ok(())
        }
        Cmd::Status {
            scope,
            full_ids,
            all,
        } => {
            let paths = Paths::from_env()?;
            let (view, committee) = runtime(false)?.block_on(async {
                let scope = scope.resolve(&paths).await?;
                let mut view = status::collect_for(&paths, scope.as_ref()).await?;
                if !all {
                    view.retain_active();
                }
                Ok::<_, anyhow::Error>((view, scope))
            })?;
            let now = chrono::Utc::now();
            let full_ids = full_ids || verbose;
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
            if let Some(note) = crate::remote::experimental_notice(&paths) {
                println!("{note}");
            }
            if let Some(line) = crate::remote::authority::latest_history(&paths) {
                println!("{line}");
            }
            Ok(())
        }
        Cmd::Debug {
            command:
                DebugCmd::Receipt {
                    seq,
                    committee,
                    json,
                },
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

/// `senate session …`: this machine's hosted sessions.
/// A replacement starts a session, waits for it to connect, and stops
/// another: longer than one host request.
const REPLACE_TIMEOUT: Duration = Duration::from_secs(300);

fn run_session(command: SessionCmd, output: Output) -> anyhow::Result<()> {
    match command {
        SessionCmd::History { committee, search } => cli_history::history(committee, search),
        SessionCmd::Show { session } => cli_history::show(&session),
        SessionCmd::List {
            all,
            machine,
            all_machines,
        } => {
            let paths = Paths::from_env()?;
            if let Some(machine) = machine {
                return runtime(false)?.block_on(remote_cli::print_machine(&paths, &machine, all));
            }
            if all_machines {
                println!("local:");
            }
            let locale = BridgeConfig::locale_at(&paths.config());
            let rt = runtime(false)?;
            let local: anyhow::Result<crate::ipc::SessionsReply> =
                rt.block_on(daemon_call(&paths, &Request::Sessions));
            match local {
                Ok(mut reply) => {
                    if !all {
                        reply.rows.retain(ipc::SessionRow::is_active);
                    }
                    println!(
                        "{}",
                        crate::sessions::render_with_ids(
                            &reply,
                            chrono::Utc::now(),
                            locale,
                            output.verbose
                        )
                    );
                }
                Err(error) if all_machines => println!("  unreachable: {error}"),
                Err(error) => return Err(error),
            }
            if all_machines {
                rt.block_on(remote_cli::print_all(&paths, all));
            }
            Ok(())
        }
        SessionCmd::Watch {
            all,
            machine,
            committee,
        } => {
            let paths = Paths::from_env()?;
            if let Some(machine) = machine {
                return runtime(false)?.block_on(remote_cli::watch(&paths, machine, all));
            }
            let locale = BridgeConfig::locale_at(&paths.config());
            runtime(false)?.block_on(crate::host::view::watch_all(&paths, all, || {
                let paths = paths.clone();
                let committee = committee.clone();
                Box::pin(async move {
                    hosted_sessions(&paths, all, &committee, output.verbose, locale).await
                })
            }))
        }
        SessionCmd::Stop {
            names,
            idle,
            all,
            force,
        } => {
            let paths = Paths::from_env()?;
            let locale = BridgeConfig::locale_at(&paths.config());
            let selector = if all {
                crate::sessions::StopSelector::All
            } else if idle {
                crate::sessions::StopSelector::Idle
            } else {
                let names = names
                    .into_iter()
                    .map(|name| {
                        Ok(session_role::resolve(&paths, &name, None)?
                            .map_or(name, |holder| holder.primary.to_string()))
                    })
                    .collect::<anyhow::Result<_>>()?;
                crate::sessions::StopSelector::Named(names)
            };
            runtime(false)?.block_on(stop_sessions(&paths, &selector, force, locale))
        }
        SessionCmd::Pause {
            which,
            force,
            dry_run,
            timeout,
        } => {
            let paths = Paths::from_env()?;
            let locale = BridgeConfig::locale_at(&paths.config());
            let Some(selector) = pause_selector(which, false)? else {
                return Ok(());
            };
            runtime(false)?.block_on(pause_sessions(
                &paths, &selector, force, dry_run, timeout, locale,
            ))
        }
        SessionCmd::Rebind { host, to } => {
            let paths = Paths::from_env()?;
            let host = session_role::resolve(&paths, &host, None)?
                .map_or(host, |holder| holder.primary.to_string());
            let to = match session_role::resolve(&paths, &to, None)? {
                Some(holder) => AliasName::bare(holder.primary),
                None => to.parse()?,
            };
            let reply: ipc::RebindHostReply =
                runtime(false)?.block_on(daemon_call(&paths, &Request::RebindHost { host, to }))?;
            println!(
                "Terminal now follows {} ({})",
                reply.current, reply.session_id
            );
            println!("Undelivered for {}: {}", reply.retired, reply.undelivered);
            for thread in reply.threads {
                println!("  {} #{}", thread.committee, thread.seq);
            }
            Ok(())
        }
        SessionCmd::Resume { which, dry_run } => {
            let paths = Paths::from_env()?;
            let locale = BridgeConfig::locale_at(&paths.config());
            let Some(selector) = pause_selector(which, true)? else {
                return Ok(());
            };
            runtime(false)?.block_on(resume_sessions(&paths, &selector, dry_run, locale))
        }
        SessionCmd::Restore {
            senator,
            model,
            attach,
        } => {
            if attach {
                require_attach_terminal()?;
            }
            let senator = session_senator(&Paths::from_env()?, &senator)?;
            launch(
                &Request::Restore {
                    senator,
                    session: SessionSelector::default(),
                    model: model.into(),
                },
                output,
                attach,
            )
        }
        SessionCmd::Replace {
            senator,
            prompt,
            prompt_file,
            model,
            attach,
        } => {
            if attach {
                require_attach_terminal()?;
            }
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
            let paths = Paths::from_env()?;
            let senator = session_senator(&paths, &senator)?;
            let locale = BridgeConfig::locale_at(&paths.config());
            let reply: crate::ipc::ReplaceReply = runtime(false)?.block_on(ipc::request(
                &paths.socket(),
                &Request::Replace {
                    senator,
                    prompt,
                    model: model.into(),
                },
                Duration::from_secs(1),
                REPLACE_TIMEOUT,
            ))?;
            println!(
                "{}",
                output.text(&crate::sessions::render_replace(&reply, locale))
            );
            if attach {
                use std::io::Write as _;
                std::io::stdout().flush()?;
                return runtime(false)?.block_on(attach_launched(
                    &paths,
                    reply.session_id,
                    &reply.new.to_string(),
                ));
            }
            Ok(())
        }
        SessionCmd::Find { senator } => {
            let paths = Paths::from_env()?;
            let locale = BridgeConfig::locale_at(&paths.config());
            let senator = pick_alias(senator)?;
            let found: ipc::FindReply =
                runtime(false)?.block_on(daemon_call(&paths, &Request::FindSession { senator }))?;
            println!("{}", render_found(&found, locale));
            Ok(())
        }
        SessionCmd::Approve {
            senator,
            once,
            always,
            deny,
        } => {
            use crate::approvals::Decision;
            let paths = Paths::from_env()?;
            let locale = BridgeConfig::locale_at(&paths.config());
            let senator = match senator {
                Some(senator) => session_senator(&paths, &senator)?,
                None => {
                    let rows = runtime(false)?
                        .block_on(daemon_call::<ipc::SessionsReply>(
                            &paths,
                            &Request::Sessions,
                        ))?
                        .rows;
                    let (senator, announcement) =
                        the_waiting_prompt(&rows, once || always || deny, locale)
                            .map_err(|message| anyhow::anyhow!(message))?;
                    // Said before the answer is sent, so it is never a surprise.
                    eprintln!("{announcement}");
                    AliasName::bare(senator)
                }
            };
            let decision = if deny {
                Decision::Deny
            } else if always {
                Decision::AllowForSession
            } else {
                Decision::Allow
            };
            let reply: crate::ipc::ApproveSessionReply = runtime(false)?.block_on(ipc::request(
                &paths.socket(),
                &Request::ApproveSession { senator, decision },
                Duration::from_secs(1),
                crate::host::REQUEST_TIMEOUT,
            ))?;
            let who = senate_core::Address::senator(&reply.senator).in_text();
            let verb = match (decision, locale) {
                (Decision::Allow, senate_core::Locale::En) => "allowed once",
                (Decision::AllowForSession, senate_core::Locale::En) => "always allowed",
                (_, senate_core::Locale::En) => "denied",
                (Decision::Allow, senate_core::Locale::Zh) => "已允许一次",
                (Decision::AllowForSession, senate_core::Locale::Zh) => "已始终允许",
                (_, senate_core::Locale::Zh) => "已拒绝",
            };
            let tool = reply
                .tool
                .as_deref()
                .map(|tool| format!(": {}", crate::host_approval::tool_label(tool)))
                .unwrap_or_default();
            match locale {
                senate_core::Locale::En => println!("{verb} {who}'s prompt{tool}"),
                senate_core::Locale::Zh => println!("{verb} {who} 的权限请求{tool}"),
            }
            if reply.audit_pending {
                eprintln!(
                    "{}",
                    locale.pick(
                        "记录已排队，稍后写入委员会日志。",
                        "The note is queued for the committee log."
                    )
                );
            }
            Ok(())
        }
        SessionCmd::Prune => {
            let paths = Paths::from_env()?;
            let locale = BridgeConfig::locale_at(&paths.config());
            let rt = runtime(false)?;
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

/// `senate senator …`: names, seats and duties.
fn run_senator(command: SenatorCmd, output: Output) -> anyhow::Result<()> {
    match command {
        SenatorCmd::List { all } => {
            let paths = Paths::from_env()?;
            let roster: Vec<ipc::RosterCommittee> =
                runtime(false)?.block_on(daemon_call(&paths, &Request::Roster))?;
            for committee in roster {
                for member in committee.members.into_iter().filter(|m| all || m.active) {
                    let name = if output.verbose {
                        member.senator.to_string()
                    } else {
                        member.display_name
                    };
                    println!("{}  {name}", committee.committee);
                }
            }
            Ok(())
        }
        SenatorCmd::Chair { senator, committee } => {
            let paths = Paths::from_env()?;
            let locale = BridgeConfig::locale_at(&paths.config());
            let rt = runtime(false)?;
            let committee = rt.block_on(committee_here(committee))?;
            let known: KnownAliases = rt.block_on(daemon_call(&paths, &Request::Aliases))?;
            // An alias names its senator on that committee.
            let written = AliasName {
                committee: senator
                    .committee
                    .clone()
                    .or_else(|| Some(committee.clone())),
                alias: senator.alias.clone(),
            };
            let primary = match known.resolve(&written, Some(&committee)) {
                AliasMatch::One(primary) => primary,
                _ => senator.alias.clone(),
            };
            let roster: Vec<ipc::RosterCommittee> =
                rt.block_on(daemon_call(&paths, &Request::Roster))?;
            let before = roster
                .iter()
                .find(|row| row.committee == committee)
                .and_then(|row| row.chair.clone());
            let info: senate_core::api::ChairInfo = rt.block_on(daemon_call(
                &paths,
                &Request::SetChair {
                    session: SessionSelector::default(),
                    committee: Some(committee.clone()),
                    senator: primary,
                },
            ))?;
            let shown = |name: &SenatorName| {
                senate_core::alias::display_name(Some(&known), Some(&committee), name)
            };
            println!(
                "{}",
                render_chair_change(
                    &committee,
                    before.as_ref().map(shown).as_deref(),
                    info.chair.as_ref().map(shown).as_deref(),
                    locale,
                )
            );
            Ok(())
        }
        SenatorCmd::Unalias { alias } => {
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
        SenatorCmd::Alias {
            alias,
            committee,
            session,
        } => {
            let paths = Paths::from_env()?;
            let locale = BridgeConfig::locale_at(&paths.config());
            let session = calling_session(session, locale)?;
            runtime(false)?.block_on(async {
                let committee = session_committee(&paths, &session, committee, locale).await?;
                let line = alias_session(&paths, session, committee, alias, locale).await?;
                output.print(&line);
                Ok(())
            })
        }
        SenatorCmd::Seat(args) => seat(CommitteeSeatAction::Join, args, output),
        SenatorCmd::Unseat(args) => seat(CommitteeSeatAction::Unseat, args, output),
        SenatorCmd::Duty {
            duty,
            committee,
            session,
        } => {
            let paths = Paths::from_env()?;
            let locale = BridgeConfig::locale_at(&paths.config());
            let session = calling_session(session, locale)?;
            runtime(false)?.block_on(async {
                let committee = session_committee(&paths, &session, committee, locale).await?;
                let line = set_duty(&paths, session, committee, duty, locale).await?;
                println!("{line}");
                Ok(())
            })
        }
    }
}

/// `senate senator seat|unseat`.
fn seat(action: CommitteeSeatAction, args: SeatArgs, output: Output) -> anyhow::Result<()> {
    let paths = Paths::from_env()?;
    let request = senate_core::api::SetCommitteeSeat {
        senator: args.senator,
        action,
        id: args.id.unwrap_or_default(),
        acted_by: args.acted_by,
    };
    let reply: senate_core::api::CommitteeSeatInfo = runtime(false)?.block_on(daemon_call(
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

/// `senate setup …`: this machine's profile, agents and daemon service.
fn run_setup(command: SetupCmd, output: Output) -> anyhow::Result<()> {
    let Output { terminal, verbose } = output;
    match command {
        SetupCmd::Completions { shell } => completion::register(shell),
        SetupCmd::Remote { command } => remote_cli::run(command),
        SetupCmd::Init {
            local,
            person,
            speaker,
            locale,
            listen,
        } => {
            if !local {
                anyhow::bail!(
                    "`senate setup init` writes the local profile only: run `senate setup init --local`. \
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
            if let Some(distro) = crate::wsl::distro() {
                let locale = BridgeConfig::locale_at(&paths.config());
                print!("{}", crate::wsl::notes(&distro, locale));
            }
            Ok(())
        }
        SetupCmd::Install {
            codex,
            no_codex,
            codex_home,
            agy,
            no_agy,
            gemini_dir,
            yes,
            claude_dir,
            claude_bin,
        } => {
            let exe = std::env::current_exe()
                .and_then(|p| p.canonicalize())
                .context("locating this binary")?;
            let choice = |no: bool, yes: bool| {
                if no {
                    install::Choice::No
                } else if yes {
                    install::Choice::Yes
                } else {
                    install::Choice::Auto
                }
            };
            // The report is in the bridge's language; before senate.toml
            // exists, the default.
            let locale = Paths::from_env()
                .map(|p| BridgeConfig::locale_at(&p.config()))
                .unwrap_or_default();
            let opts = install::InstallOptions::resolve(
                exe,
                claude_dir,
                claude_bin,
                choice(no_codex, codex),
                codex_home,
                choice(no_agy, agy),
                gemini_dir,
            )?
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
            println!("{}", completion::install_hint(locale));
            // An upgrade does not reach a service that runs its own copy.
            if let Ok((spec, _)) = service_spec()
                && let Some(senated) = std::env::current_exe().ok().and_then(|exe| {
                    service::locate_senated(&exe, std::env::var_os("PATH").as_deref())
                })
                && let Some(note) = service::upgrade_note(&spec, &senated, locale)
            {
                println!("{note}");
            }
            Ok(())
        }
        SetupCmd::Service { command } => run_service(command, output),
        SetupCmd::Doctor { json } => run_doctor(json),
        SetupCmd::Clean { yes, delete_logs } => {
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
                     保持运行：senate setup service install --yes\n  \
                     查看：senate status、senate setup service status"
                ),
                format!(
                    "the Senate daemon is not running (nothing listens on {socket}).\n  \
                     Start it:        senated --machine\n  \
                     Keep it running: senate setup service install --yes\n  \
                     Check:           senate status, senate setup service status"
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
                "{error:#}\n  `senate status` 显示 daemon 的状态；`senate setup service status` 给出日志位置。"
            ),
            format!(
                "{error:#}\n  `senate status` shows the daemon's state; `senate setup service status` says where its log is."
            ),
        ),
        _ => format!("{error:#}"),
    }
}

/// Run the `senate` command with the process's arguments. This is all of
/// the command: the `senate-bridge` and `senate` packages both install a
/// binary that only calls it.
pub fn main() -> ExitCode {
    completion::complete();
    let terminal = Terminal::stdout();
    let cli = cli_help::parse(terminal);
    let command = cli.command.unwrap_or(Cmd::Status {
        scope: DisplayScope {
            global: false,
            committee: None,
        },
        full_ids: false,
        all: false,
    });
    if let Cmd::Hook {
        event,
        runtime,
        codex,
        agy,
    } = command
    {
        run_hook(event, RuntimeFlag::chosen(runtime, codex, agy));
        return ExitCode::SUCCESS;
    }
    let output = Output {
        terminal,
        verbose: cli.verbose,
    };
    match run(command, output) {
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
    match explicit {
        Some(id) => Ok(SessionSelector {
            session_id: Some(id),
            claude_pid: None,
        }),
        None => session_from_env().ok_or_else(|| {
            anyhow::anyhow!(
                "{}",
                locale.pick(
                    "请用 --session 指定完整的会话 ID。",
                    "Specify the full session ID with --session."
                )
            )
        }),
    }
}

/// The coding session this process runs inside, when its environment
/// names exactly one (`CODEX_THREAD_ID` or `CLAUDE_CODE_SESSION_ID`).
fn session_from_env() -> Option<SessionSelector> {
    let env_id = |key| std::env::var(key).ok().filter(|id: &String| !id.is_empty());
    let id = match (env_id("CODEX_THREAD_ID"), env_id("CLAUDE_CODE_SESSION_ID")) {
        (Some(id), None) | (None, Some(id)) => id,
        _ => return None,
    };
    Some(SessionSelector {
        session_id: Some(id),
        claude_pid: None,
    })
}

/// `committee`, else the one `session` is on now.
async fn session_committee(
    paths: &Paths,
    session: &SessionSelector,
    committee: Option<CommitteeId>,
    locale: Locale,
) -> anyhow::Result<CommitteeId> {
    if let Some(committee) = committee {
        return Ok(committee);
    }
    daemon_call::<ipc::BoundReply>(
        paths,
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
                "请用 --committee 指定这个会话所在的委员会。",
                "Specify the session's committee with --committee."
            )
        )
    })
}

/// Gives `session` the alias `alias` on `committee` through the daemon,
/// and says so.
async fn alias_session(
    paths: &Paths,
    session: SessionSelector,
    committee: CommitteeId,
    alias: SenatorName,
    locale: Locale,
) -> anyhow::Result<String> {
    let reply: ipc::JoinReply = daemon_call(
        paths,
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
    Ok(format!(
        "{} -> {} ({})",
        senate_core::Address::senator(&alias).in_text(),
        senate_core::Address::senator(&reply.agent).in_text(),
        reply.committee
    ))
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

#[cfg(test)]
#[test]
fn a_list_without_a_terminal_names_the_command_and_the_sessions() {
    use crate::ipc::HostState;
    use crate::sessions::tests::row;
    let rows = [
        row("claude-1", Some("reviewer"), HostState::Running),
        row("codex-2", Some("builder"), HostState::Running),
    ];
    let en = picker_refusal("senate session resume", &rows, true, Locale::En);
    assert!(
        en.contains("`senate session resume reviewer` (or --all)")
            && en.contains("To choose from: reviewer, builder.")
            && !en.contains("attach"),
        "{en}"
    );
    let attach = picker_refusal("senate attach", &rows, false, Locale::En);
    assert!(
        attach.contains("`senate attach reviewer`.") && !attach.contains("--all"),
        "{attach}"
    );
    let zh = picker_refusal("senate session pause", &rows, true, Locale::Zh);
    assert!(
        zh.contains("`senate session pause reviewer`（或 --all）"),
        "{zh}"
    );
}

#[cfg(test)]
#[test]
fn a_spawn_without_a_runtime_names_the_runtimes_and_profiles() {
    let none = no_runtime(&[], Locale::En);
    assert!(
        none.contains("`senate spawn claude` (or codex, agy)")
            && none.contains("none is configured yet"),
        "{none}"
    );
    let some = no_runtime(&["review".to_owned(), "fast".to_owned()], Locale::En);
    assert!(
        some.ends_with("or `senate spawn --profile review`. Launch profiles: review, fast."),
        "{some}"
    );
    let zh = no_runtime(&["review".to_owned()], Locale::Zh);
    assert!(zh.contains("`senate spawn --profile review`"), "{zh}");
}

#[cfg(test)]
#[test]
fn no_profiles_shows_one_to_copy_and_where() {
    let text = profiles_example(
        std::path::Path::new("/home/a/.senate/senate.toml"),
        Locale::En,
    );
    assert!(
        text.starts_with("Add one to /home/a/.senate/senate.toml"),
        "{text}"
    );
    assert!(
        text.contains("[profiles.review]\nruntime = \"codex\""),
        "{text}"
    );
    assert!(text.ends_with("`senate spawn --profile review`."), "{text}");
    // What it shows parses as a profile.
    let toml = text.split("\n\n").nth(1).unwrap();
    let parsed: toml::Table = toml.parse().unwrap();
    assert_eq!(
        parsed["profiles"]["review"]["runtime"].as_str(),
        Some("codex")
    );
}

#[cfg(test)]
#[test]
fn approve_without_a_name_takes_only_the_one_waiting_prompt_and_says_so() {
    use crate::ipc::{HostState, HostedPrompt};
    use crate::sessions::tests::row;
    let asking = |name: &str, alias: &str, screen: bool| {
        let mut row = row(name, Some(alias), HostState::Running);
        row.prompt = Some(HostedPrompt {
            tool: Some("Bash".into()),
            since: chrono::Utc::now(),
            screen,
        });
        row
    };
    let quiet = row("codex-9", Some("quiet"), HostState::Running);
    let one = [asking("claude-1", "reviewer", false), quiet.clone()];
    let (senator, said) = the_waiting_prompt(&one, true, Locale::En).unwrap();
    assert_eq!(senator.as_str(), "claude-1");
    assert!(said.starts_with("Answering reviewer's prompt: "), "{said}");
    // The answer is never assumed when the session is.
    let refused = the_waiting_prompt(&one, false, Locale::En).unwrap_err();
    assert!(
        refused.contains("give the answer: `senate session approve --once`"),
        "{refused}"
    );
    // A choice only seen on the screen is not one approve can answer.
    let screen = [asking("claude-1", "reviewer", true), quiet.clone()];
    assert!(
        the_waiting_prompt(&screen, true, Locale::En)
            .unwrap_err()
            .starts_with("No hosted session")
    );
    let two = [
        asking("claude-1", "reviewer", false),
        asking("claude-2", "builder", false),
    ];
    let listed = the_waiting_prompt(&two, true, Locale::En).unwrap_err();
    assert!(
        listed.starts_with("2 sessions have a prompt waiting") && listed.contains("\n  builder  "),
        "{listed}"
    );
}

#[cfg(test)]
#[test]
fn the_picker_names_sessions_as_the_list_does() {
    use crate::ipc::HostState;
    use crate::sessions::tests::row;
    let named = row("claude-1", Some("reviewer"), HostState::Running);
    let unnamed = row(
        "claude-5bde5f64-db8f-4881-bd49-7c2dd8107ed3",
        None,
        HostState::Stopped,
    );
    let choices = session_choices(&[named.clone(), unnamed.clone()]);
    assert_eq!(choices[0].label, named.display_name(false));
    assert_eq!(choices[0].label, "reviewer");
    assert!(
        choices[0].detail.starts_with("refund-v2 · codex · running"),
        "{:?}",
        choices[0]
    );
    assert_eq!(choices[1].label, "claude-5bde5f64");
    assert!(choices[1].dim);
    assert_eq!(choices[1].primary, unnamed.primary);
}

#[cfg(test)]
#[test]
fn a_chair_change_says_who_chaired_before_and_who_now() {
    let ibot = CommitteeId::new("ibot").unwrap();
    assert_eq!(
        render_chair_change(&ibot, Some("codex1"), Some("main"), Locale::En),
        "Chair of ibot: codex1 → main (codex1 remains a member; \
         `senate senator unseat codex1 --committee ibot` removes it)."
    );
    assert_eq!(
        render_chair_change(&ibot, None, Some("main"), Locale::En),
        "Chair of ibot: (none) → main."
    );
    assert_eq!(
        render_chair_change(&ibot, Some("main"), Some("main"), Locale::En),
        "main already chairs ibot."
    );
    assert!(render_chair_change(&ibot, Some("a"), Some("b"), Locale::Zh).contains("a → b"));
}

#[cfg(test)]
#[test]
fn archiving_lists_what_it_hides_and_names_what_must_stop_first() {
    let plan = |committee: &str, sessions: Vec<(&str, bool)>| ArchivePlan {
        committee: CommitteeId::new(committee).unwrap(),
        members: vec!["claude1".into()],
        pending: 2,
        sessions: sessions
            .into_iter()
            .map(|(n, r)| (n.to_owned(), r))
            .collect(),
    };
    let quiet = [
        plan("api-test", vec![("api-test::claude1", false)]),
        plan("sim-relay", vec![]),
    ];
    let text = render_archive_plan(&quiet, Locale::En);
    assert!(
        text.starts_with("api-test\n  members: claude1\n  open threads: 2\n"),
        "{text}"
    );
    assert!(
        text.contains("stopped hosted sessions (their records stay): api-test::claude1"),
        "{text}"
    );
    assert!(text.contains("sim-relay\n"), "{text}");
    assert_eq!(archive_blockers(&quiet, Locale::En), None);
    let busy = [plan("api-test", vec![("api-test::claude1", true)])];
    assert_eq!(
        archive_blockers(&busy, Locale::En).unwrap(),
        "Hosted sessions are still running on them; stop them first: `senate session stop api-test::claude1`."
    );
    let info = |archived, seq| senate_core::api::CommitteeArchiveInfo {
        committee: CommitteeId::new("api-test").unwrap(),
        archived,
        seq,
    };
    assert_eq!(
        render_archived(&info(true, Some(Seq(9))), Locale::En),
        "archived api-test (seq 9)"
    );
    assert_eq!(
        render_archived(&info(false, None), Locale::En),
        "api-test was not archived"
    );
}

#[cfg(test)]
#[test]
fn an_attach_held_elsewhere_says_where_and_what_to_do() {
    assert_eq!(
        attached_elsewhere("alpha::main", Some("/dev/ttys012"), Some(18), Locale::En),
        "alpha::main is already attached in another terminal (ttys012, last typed 18s ago); \
         that terminal now rings and shows \"Senate: this terminal\" in its title. Detach it there \
         with Ctrl-], or watch it here with `senate attach alpha::main --read-only`."
    );
    assert!(
        attached_elsewhere("alpha::main", None, None, Locale::En)
            .starts_with("alpha::main is already attached in another terminal; ")
    );
    assert!(
        attached_elsewhere("alpha::main", Some("/dev/ttys3"), None, Locale::Zh)
            .contains("alpha::main 已经在另一个终端里打开（ttys3）")
    );
}

#[cfg(test)]
#[test]
fn find_says_which_terminal_rings_or_that_none_holds_it() {
    let found = |attached, tty: Option<&str>| ipc::FindReply {
        name: "senate::main".into(),
        attached,
        tty: tty.map(str::to_owned),
    };
    assert_eq!(
        render_found(&found(true, Some("/dev/ttys012")), Locale::En),
        "senate::main is attached in ttys012: that terminal now rings and shows \"Senate: this terminal\" in its title."
    );
    assert_eq!(
        render_found(&found(false, None), Locale::En),
        "senate::main is not attached in any terminal; `senate attach senate::main` opens it here."
    );
}
