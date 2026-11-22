//! `senate chat`: simulated chat, so a chat flow is a script instead of
//! clicks in the chat platform ([`senate_core::chat_sim`]).
//!
//! `send` injects a message into a bound group as if the person had
//! written it there; the Speaker interprets it and answers as it would on
//! the platform. `thread` reads the conversation: what was injected and
//! what the Speaker posted, kept off the platform unless `--live`.

use std::time::Duration;

use clap::{Args, Subcommand};
use senate_core::chat_sim::{SimTranscript, SimulateChat, SimulatedChat};
use senate_core::{Address, CommitteeId, Name, SenatorName};

use crate::Paths;
use crate::ipc::Request;

use super::{committee_here, daemon_call, print_json, runtime, session_from_env};

#[derive(Args)]
pub(super) struct Chat {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Say something in a bound group as a person would on the chat
    /// platform. The Speaker answers as it would there; its answers are
    /// kept off the platform unless --live. Prints the message's ref.
    #[command(after_help = "Examples:
  senate chat send --committee refund-v2 \"ask bob's agent how long refunds take\"
  senate chat send --committee refund-v2 --in sim:01j… \"yes, record that\"
  senate chat send --committee refund-v2 --as bob --in sim:01j… \"agreed\"
  senate chat send --committee refund-v2 --office --as alice \"/bind refund-v2\"")]
    Send {
        /// The message.
        #[arg(required = true)]
        text: Vec<String>,
        /// The committee (default: the one `senate where` shows for this directory).
        #[arg(long)]
        committee: Option<CommitteeId>,
        /// The bound group (default: the committee's only one).
        #[arg(long)]
        group: Option<String>,
        /// Whom it is from: you (default), or any person on the roster. It
        /// shows as simulated by you.
        #[arg(long = "as")]
        as_person: Option<Name>,
        /// Act for the Speaker's office.
        #[arg(long)]
        office: bool,
        /// Who acts for the office (default: you, or the one secretary you run).
        #[arg(long, requires = "office")]
        acted_by: Option<SenatorName>,
        /// Reply in the thread of this message of a simulated conversation.
        #[arg(long = "in")]
        reply_to: Option<String>,
        /// Plain group chat that does not mention the Speaker.
        #[arg(long)]
        ambient: bool,
        /// Post it, and everything answering it, in the real group too.
        #[arg(long)]
        live: bool,
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print a simulated conversation: what was said and what the Speaker
    /// posted for it.
    #[command(after_help = "Examples:
  senate chat thread sim:01j… --committee refund-v2
  senate chat thread sim:01j… --wait --timeout 60
  senate chat thread sim:01j… --wait --since 2 --json")]
    Thread {
        /// Any message of the conversation, as `chat send` printed it.
        message_ref: String,
        /// The committee (default: the one `senate where` shows for this directory).
        #[arg(long)]
        committee: Option<CommitteeId>,
        /// Wait until the Speaker has posted something new: after the
        /// latest message said, or after the first --since entries.
        #[arg(long)]
        wait: bool,
        /// With --wait: the entries already seen.
        #[arg(long, requires = "wait")]
        since: Option<usize>,
        /// With --wait: give up after this many seconds (exit status 1).
        #[arg(long, default_value_t = 60, requires = "wait")]
        timeout: u64,
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
    },
}

pub(super) fn run(args: Chat) -> anyhow::Result<()> {
    let paths = Paths::from_env()?;
    // Run inside a coding session, the command is that session's agent's:
    // the daemon names it, and the server refuses it.
    let session = session_from_env().unwrap_or_default();
    match args.action {
        Action::Send {
            text,
            committee,
            group,
            as_person,
            office,
            acted_by,
            reply_to,
            ambient,
            live,
            json,
        } => runtime(false)?.block_on(async {
            let committee = committee_here(committee).await?;
            let sent: SimulatedChat = daemon_call(
                &paths,
                &Request::ChatSend {
                    session,
                    committee: Some(committee),
                    request: SimulateChat {
                        text: text.join(" "),
                        by: None,
                        office,
                        acted_by,
                        as_person,
                        group,
                        reply_to,
                        ambient,
                        live,
                    },
                },
            )
            .await?;
            if json {
                return print_json(&sent);
            }
            println!("{}", sent.message_ref);
            eprintln!(
                "{} in {}{}{}; read it with `senate chat thread {}`",
                if sent.live {
                    "posted (live)"
                } else {
                    "simulated (log only)"
                },
                sent.group_id,
                sent.seq
                    .map(|s| format!(", chat seq {s}"))
                    .unwrap_or_default(),
                if sent.conversation == sent.message_ref {
                    String::new()
                } else {
                    format!(", conversation {}", sent.conversation)
                },
                sent.message_ref
            );
            Ok(())
        }),
        Action::Thread {
            message_ref,
            committee,
            wait,
            since,
            timeout,
            json,
        } => runtime(false)?.block_on(async {
            let committee = committee_here(committee).await?;
            let (transcript, arrived) = crate::speak::chat_thread(
                &paths.socket(),
                committee,
                message_ref,
                wait.then(|| (since, Duration::from_secs(timeout))),
            )
            .await?;
            if !arrived {
                if json {
                    print_json(&transcript)?;
                } else {
                    print!("{}", render(&transcript));
                }
                anyhow::bail!("the Speaker posted nothing new within {timeout}s");
            }
            if json {
                print_json(&transcript)
            } else {
                print!("{}", render(&transcript));
                Ok(())
            }
        }),
    }
}

/// A conversation as the terminal shows it: numbered entries, the injected
/// ones marked "(simulated by <person>)".
pub fn render(transcript: &SimTranscript) -> String {
    let mut out = format!(
        "conversation {} · {} · group {} · {}\n",
        transcript.conversation,
        transcript.committee,
        transcript.group_id,
        if transcript.live { "live" } else { "log only" }
    );
    for (i, entry) in transcript.entries.iter().enumerate() {
        let who = if entry.from == Address::Speaker {
            "speaker".to_owned()
        } else {
            match &entry.simulated_by {
                Some(by) => format!("{} (simulated by {by})", entry.from),
                None => format!("{} (simulated)", entry.from),
            }
        };
        let mut notes = vec![entry.message_ref.clone()];
        if let Some(seq) = entry.seq {
            notes.push(format!("seq {seq}"));
        }
        if let Some(to) = &entry.reply_to {
            notes.push(format!("in reply to {to}"));
        }
        if let Some(group) = &entry.group_id {
            notes.push(format!("in group {group}"));
        }
        if entry.updates.is_some() {
            notes.push("card updated".to_owned());
        } else if entry.card {
            notes.push("card".to_owned());
        }
        out.push_str(&format!("{}  {who} · {}\n", i + 1, notes.join(" · ")));
        for line in entry.text.lines() {
            out.push_str(&format!("   {line}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use senate_core::chat_sim::SimEntry;
    use senate_core::{CommitteeId, Name};

    use super::*;

    #[test]
    fn an_injected_message_shows_who_simulated_it() {
        let entry = |from: &str, by: Option<&str>| SimEntry {
            ts: Utc::now(),
            from: Address::senior(&Name::new(from).unwrap()),
            simulated_by: by.map(|b| Name::new(b).unwrap()),
            text: "agreed".into(),
            message_ref: "sim:1".into(),
            reply_to: None,
            group_id: None,
            card: false,
            updates: None,
            seq: None,
        };
        let shown = render(&SimTranscript {
            conversation: "sim:1".into(),
            committee: CommitteeId::new("refund-v2").unwrap(),
            group_id: "g".into(),
            live: false,
            entries: vec![entry("alice", Some("bob")), entry("bob", None)],
        });
        assert!(shown.contains("alice (simulated by bob)"), "{shown}");
        assert!(shown.contains("bob (simulated)"), "{shown}");
    }
}
