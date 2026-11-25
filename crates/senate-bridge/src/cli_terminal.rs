//! `senate terminal`: terminal use from a shell. An agent opens a scratch
//! terminal here, in its own shell, because the program must run with that
//! shell's rights and no others ([`crate::host::scratch`]); everything
//! after that it can also do through its `terminal` tool. A person lists
//! every scratch terminal, looks at one, or takes it over.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context as _;
use clap::{Args, Subcommand};
use senate_core::EventId;

use super::{BridgeConfig, print_json, require_terminal, runtime, session_from_env};
use crate::Paths;
use crate::ipc::{
    self, Request, SessionSelector, TerminalAction, TerminalReply, TerminalScreen, TerminalTarget,
};

#[derive(Args)]
pub(super) struct Terminal {
    /// The coding session to act as. By default the one this shell runs
    /// in; outside a coding session, you.
    #[arg(long, global = true, value_name = "SESSION-ID")]
    session: Option<String>,
    /// Print JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Run a program in a new scratch terminal and print the terminal's id.
    ///
    /// The program runs with this command's own environment, directory and
    /// sandbox: Senate's daemon starts nothing. It keeps running after this
    /// command returns, until `senate terminal close` or until the coding
    /// session that opened it ends. A program that exits leaves its last
    /// screen to read.
    #[command(after_help = "Examples:
  senate terminal open -- python3 -i
  senate terminal open --rows 40 --cols 120 --cwd ../app -- npm run dev")]
    Open {
        #[arg(long, default_value_t = 24)]
        rows: u16,
        #[arg(long, default_value_t = 80)]
        cols: u16,
        /// The program's directory. Defaults to this one.
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
        /// The program and its arguments, after `--`.
        #[arg(required = true, last = true)]
        command: Vec<OsString>,
    },
    /// The scratch terminals you opened; every one, from your own terminal.
    List,
    /// Print a terminal's screen as text.
    #[command(after_help = "Examples:
  senate terminal screen 01J…                The screen, with its size and cursor
  senate terminal screen 01J… --scrollback 100
  senate terminal screen --of reviewer       A hosted session's screen: its committee's chair, or you")]
    Screen {
        /// The scratch terminal.
        #[arg(required_unless_present = "of")]
        id: Option<EventId>,
        /// A hosted session instead, by alias: read only.
        #[arg(long, value_name = "ALIAS", conflicts_with = "id")]
        of: Option<String>,
        /// Rows above the screen to print as well.
        #[arg(long, default_value_t = 0, value_name = "ROWS")]
        scrollback: u16,
    },
    /// Type text into a scratch terminal, then print its screen.
    ///
    /// The text is typed as it is; a line end in it is Enter. Use `key` for
    /// Enter on its own and for every other key.
    Type {
        id: EventId,
        /// The text; `-` reads it from stdin.
        #[arg(required = true)]
        text: Vec<String>,
    },
    /// Press keys in a scratch terminal, then print its screen.
    #[command(after_help = "Examples:
  senate terminal key 01J… enter
  senate terminal key 01J… down down enter
  senate terminal key 01J… ctrl-c")]
    Key {
        id: EventId,
        /// enter, tab, esc, space, backspace, delete, up, down, left, right,
        /// home, end, pageup, pagedown, f1 to f12, or one character; with
        /// ctrl-, alt- or shift- in front.
        #[arg(required = true)]
        keys: Vec<String>,
    },
    /// Wait for a scratch terminal's screen, then print it.
    ///
    /// Without --text or --still, waits until the screen is other than you
    /// were last shown. Returns at once when the program exits. Exits with
    /// a failure when the time runs out.
    Wait {
        id: EventId,
        /// Until the screen contains this.
        #[arg(long, conflicts_with = "still")]
        text: Option<String>,
        /// Until nothing was written for this many milliseconds.
        #[arg(long, value_name = "MS")]
        still: Option<u64>,
        /// The longest to wait.
        #[arg(long, default_value_t = 30, value_name = "SECONDS")]
        timeout: u64,
    },
    /// Set a scratch terminal's size, then print its screen.
    Resize { id: EventId, rows: u16, cols: u16 },
    /// End a scratch terminal's program and forget the terminal.
    Close { id: EventId },
    /// Open a scratch terminal in this one: yours to type in until Ctrl-].
    ///
    /// For a person, from their own terminal. While you are attached, the
    /// session that opened the terminal can read it but not type into it.
    /// With --read-only nothing you type reaches it.
    Attach {
        id: EventId,
        /// Look without typing.
        #[arg(long)]
        read_only: bool,
    },
}

/// The session this command acts as: the one named, else the one this
/// shell runs in, else nobody (the person).
fn caller(explicit: Option<String>) -> SessionSelector {
    explicit
        .map(|id| SessionSelector {
            session_id: Some(id),
            claude_pid: None,
        })
        .or_else(session_from_env)
        .unwrap_or_default()
}

async fn call(
    paths: &Paths,
    session: &SessionSelector,
    action: TerminalAction,
    timeout: Duration,
) -> anyhow::Result<TerminalReply> {
    let request = Request::Terminal {
        session: session.clone(),
        action,
    };
    let reply = ipc::request(&paths.socket(), &request, Duration::from_secs(1), timeout).await;
    match reply {
        // Without a session the request is the person's, and an agent's
        // shell is not a person: say what the agent should have said.
        Err(ipc::ClientError::Remote(error))
            if error.code == ipc::codes::NOT_A_PERSON && *session == SessionSelector::default() =>
        {
            anyhow::bail!(
                "{}\n  From inside a coding session, say which: senate terminal --session <session-id> …",
                error.message
            )
        }
        reply => Ok(reply?),
    }
}

fn screen_of(reply: TerminalReply) -> anyhow::Result<TerminalScreen> {
    match reply {
        TerminalReply::Screen(screen) => Ok(screen),
        other => anyhow::bail!("the daemon answered something else: {other:?}"),
    }
}

pub(super) fn run(args: Terminal) -> anyhow::Result<()> {
    let paths = Paths::from_env()?;
    let locale = BridgeConfig::locale_at(&paths.config());
    let session = caller(args.session);
    let json = args.json;
    let short = Duration::from_secs(20);
    let show = |screen: TerminalScreen| -> anyhow::Result<()> {
        if json {
            print_json(&screen)
        } else {
            println!("{}", crate::terminal_use::render_screen(&screen, locale));
            Ok(())
        }
    };
    let rt = runtime(false)?;
    match args.action {
        Action::Open {
            rows,
            cols,
            cwd,
            command,
        } => {
            let cwd = match cwd {
                Some(dir) => std::path::absolute(dir)?,
                None => std::env::current_dir()?,
            };
            anyhow::ensure!(cwd.is_dir(), "{} is not a directory", cwd.display());
            rt.block_on(async {
                let open = TerminalAction::Open {
                    command: command
                        .iter()
                        .map(|word| word.to_string_lossy().into_owned())
                        .collect(),
                    cwd: cwd.clone(),
                    rows,
                    cols,
                };
                let TerminalReply::Opened { id } = call(&paths, &session, open, short).await?
                else {
                    anyhow::bail!("the daemon did not reserve a terminal");
                };
                let program = crate::host::scratch::Program {
                    argv: &command,
                    cwd: &cwd,
                    rows,
                    cols,
                };
                if let Err(error) = crate::host::scratch::start(&paths, &id, &program).await {
                    // Give the reservation back: it counts against the limit.
                    let close = TerminalAction::Close { id };
                    let _ = call(&paths, &session, close, short).await;
                    return Err(error);
                }
                // The id alone: a script and a model both read it from here.
                if json {
                    print_json(&serde_json::json!({ "id": id }))
                } else {
                    println!("{id}");
                    Ok(())
                }
            })
        }
        Action::List => {
            let reply = rt.block_on(call(&paths, &session, TerminalAction::List, short))?;
            let TerminalReply::List { terminals } = reply else {
                anyhow::bail!("the daemon answered something else: {reply:?}");
            };
            if json {
                print_json(&terminals)
            } else {
                println!("{}", crate::terminal_use::render_list(&terminals, locale));
                Ok(())
            }
        }
        Action::Screen { id, of, scrollback } => {
            let target = match (id, of) {
                (Some(id), None) => TerminalTarget::Id(id),
                (None, Some(alias)) => TerminalTarget::Session(
                    crate::mcp::parse_alias_name(&alias).map_err(anyhow::Error::msg)?,
                ),
                _ => anyhow::bail!("name a scratch terminal, or a hosted session with --of"),
            };
            let action = TerminalAction::Screen { target, scrollback };
            show(screen_of(
                rt.block_on(call(&paths, &session, action, short))?,
            )?)
        }
        Action::Type { id, text } => {
            let text = if text == ["-"] {
                std::io::read_to_string(std::io::stdin()).context("read the text from stdin")?
            } else {
                text.join(" ")
            };
            let action = TerminalAction::Type { id, text };
            show(screen_of(
                rt.block_on(call(&paths, &session, action, short))?,
            )?)
        }
        Action::Key { id, keys } => {
            let action = TerminalAction::Key {
                id,
                keys: keys.join(" "),
            };
            show(screen_of(
                rt.block_on(call(&paths, &session, action, short))?,
            )?)
        }
        Action::Wait {
            id,
            text,
            still,
            timeout,
        } => {
            let action = TerminalAction::Wait {
                id,
                text,
                still_ms: still,
                timeout_ms: timeout.saturating_mul(1000),
            };
            let patience = Duration::from_secs(timeout.saturating_add(15));
            let screen = screen_of(rt.block_on(call(&paths, &session, action, patience))?)?;
            let timed_out = screen.timed_out;
            show(screen)?;
            anyhow::ensure!(
                !timed_out,
                "{}",
                locale.pick(
                    format!("等了 {timeout} 秒，等待的情况没有出现"),
                    format!("what was waited for did not happen in {timeout} s"),
                )
            );
            Ok(())
        }
        Action::Resize { id, rows, cols } => {
            let action = TerminalAction::Resize { id, rows, cols };
            show(screen_of(
                rt.block_on(call(&paths, &session, action, short))?,
            )?)
        }
        Action::Close { id } => {
            rt.block_on(call(
                &paths,
                &session,
                TerminalAction::Close { id: id.clone() },
                short,
            ))?;
            if json {
                print_json(&serde_json::json!({ "closed": id }))
            } else {
                println!(
                    "{}",
                    locale.pick(format!("已关闭 {id}。"), format!("Closed {id}."))
                );
                Ok(())
            }
        }
        Action::Attach { id, read_only } => {
            let tty = crate::host::socket(&paths, &id);
            anyhow::ensure!(
                tty.exists() && crate::host::scratch::is_scratch(&paths, &id),
                "{}",
                locale.pick(
                    format!("没有正在运行的终端 {id}；`senate terminal list` 会列出它们。"),
                    format!("no running scratch terminal {id}; `senate terminal list` shows them"),
                )
            );
            let label = format!("terminal {id}");
            if read_only {
                return rt.block_on(crate::host::view::watch_one(crate::host::view::Session {
                    title: label.clone(),
                    key: label.clone(),
                    alias: label,
                    attached: false,
                    tty,
                    dim: false,
                }));
            }
            require_terminal(
                "senate terminal attach",
                locale.pick(
                    "`senate terminal screen` 不需要终端也能显示它的屏幕。",
                    "`senate terminal screen` prints its screen without one.",
                ),
            )?;
            match rt.block_on(crate::host::attach(&tty, &paths, &label)) {
                Err(error) if error.downcast_ref::<crate::host::Busy>().is_some() => {
                    anyhow::bail!(
                        "{}",
                        locale.pick(
                            format!("{label} 已经有人接入；用 --read-only 只看不输入。"),
                            format!(
                                "someone is attached to {label} already; --read-only looks without typing"
                            ),
                        )
                    )
                }
                other => other,
            }
        }
    }
}
