use std::io::{self, Write as _};

use anyhow::{Context, bail, ensure};
use clap::Subcommand;
use clap_complete::engine::CompletionCandidate;

use crate::host::{self, view::Session};
use crate::ipc::Request;
use crate::paths::Paths;
use crate::remote::{self, Client, Control, Device, Row, Scopes};
use senate_core::Name;

#[derive(Subcommand)]
pub(super) enum Command {
    /// Create an expiring offer on this machine and confirm its comparison code.
    Pair {
        /// Give the device its own interactive attach grant.
        #[arg(long)]
        attach: bool,
        /// Only grant metadata listing, with no terminal streams.
        #[arg(long, conflicts_with = "attach")]
        list_only: bool,
        /// Number of seconds the grant stays valid after confirmation.
        #[arg(long, default_value_t = 30 * 86400)]
        valid_secs: u64,
    },
    /// Connect this machine to an offer. Read the pairing code from stdin.
    Connect {
        /// Local name for the remote machine.
        machine: Name,
        /// This device's label on the target machine.
        #[arg(long)]
        device: Name,
    },
    /// List the device grants on this machine.
    Devices,
    /// Revoke the grant and disconnect its active streams.
    Revoke {
        /// Device ID from the local device list.
        device: String,
    },
}

pub(super) fn complete_machines() -> Vec<CompletionCandidate> {
    Paths::from_env().map_or_else(
        |_| Vec::new(),
        |paths| {
            remote::names(&paths)
                .into_iter()
                .map(|name| CompletionCandidate::new(name.to_string()))
                .collect()
        },
    )
}

fn read() -> anyhow::Result<String> {
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    ensure!(!value.trim().is_empty(), "no code entered");
    Ok(value.trim().to_owned())
}

fn local<T: serde::de::DeserializeOwned>(
    rt: &tokio::runtime::Runtime,
    paths: &Paths,
    command: Control,
) -> anyhow::Result<T> {
    rt.block_on(super::daemon_call(
        paths,
        &Request::RemoteControl { command },
    ))
}

pub(super) fn run(command: Command) -> anyhow::Result<()> {
    let paths = Paths::from_env()?;
    let rt = super::runtime(false)?;
    match command {
        Command::Pair {
            attach,
            list_only,
            valid_secs,
        } => {
            let code: String = local(
                &rt,
                &paths,
                Control::Pair {
                    scopes: Scopes {
                        watch: !list_only,
                        attach,
                    },
                    valid_secs,
                },
            )?;
            println!("Pairing code, valid for two minutes:\n{code}");
            print!("Enter the comparison code shown on the client: ");
            io::stdout().flush()?;
            let device: Device = local(
                &rt,
                &paths,
                Control::Confirm {
                    comparison: read()?,
                },
            )?;
            println!("Paired {} ({})", device.name, device.id);
        }
        Command::Connect { machine, device } => {
            print!("Paste the target's pairing code: ");
            io::stdout().flush()?;
            let code = read()?;
            let id = rt.block_on(Client::pair(&paths, &machine, device, &code))?;
            println!("Paired {machine} as {}", id.name);
        }
        Command::Devices => {
            let devices: Vec<Device> = local(&rt, &paths, Control::Devices)?;
            for device in devices {
                let mut scopes = vec!["list"];
                if device.scopes.watch {
                    scopes.push("watch");
                }
                if device.scopes.attach {
                    scopes.push("attach");
                }
                let expiry = chrono::DateTime::from_timestamp(device.expires, 0)
                    .context("invalid expiry")?;
                println!(
                    "{}  {}  {}  {}",
                    device.name,
                    device.id,
                    scopes.join(","),
                    expiry
                );
            }
        }
        Command::Revoke { device } => {
            let _: serde_json::Value = local(&rt, &paths, Control::Revoke { device })?;
            println!("Revoked device");
        }
    }
    Ok(())
}

fn label(row: &Row, machine: &Name) -> String {
    crate::terminal::printable(&format!("{machine} · {} · {}", row.label, row.runtime)).into_owned()
}

fn state(row: &Row) -> &'static str {
    if row.running && row.attached {
        "attached"
    } else if row.running {
        "running"
    } else if row.paused {
        "paused"
    } else {
        "stopped"
    }
}

pub(super) async fn print_machine(paths: &Paths, machine: &Name, all: bool) -> anyhow::Result<()> {
    println!("{machine}:");
    let client = Client::open(paths, machine)?;
    let rows = client
        .list()
        .await
        .with_context(|| format!("{machine} is unreachable"))?;
    for row in rows.iter().filter(|row| all || row.running || row.paused) {
        println!("  {}  {}", label(row, machine), state(row));
    }
    Ok(())
}

pub(super) async fn print_all(paths: &Paths, all: bool) {
    let mut names = remote::names(paths);
    names.sort();
    for machine in names {
        if let Err(error) = print_machine(paths, &machine, all).await {
            println!("  unreachable: {error:#}");
        }
    }
}

fn tile(row: Row, machine: &Name) -> Session {
    let title = label(&row, machine);
    Session {
        title,
        key: row.session,
        alias: row.label,
        tty: Default::default(),
        dim: !row.running,
        attached: row.attached,
    }
}

pub(super) async fn watch(paths: &Paths, machine: Name, all: bool) -> anyhow::Result<()> {
    let client = Client::open(paths, &machine)?;
    host::view::watch_remote(
        paths,
        all,
        || {
            let client = client.clone();
            let machine = machine.clone();
            Box::pin(async move {
                let rows = client
                    .list()
                    .await
                    .with_context(|| format!("{machine} is unreachable"))?;
                Ok(rows
                    .into_iter()
                    .filter(|row| all || row.running)
                    .map(|row| tile(row, &machine))
                    .collect())
            })
        },
        client.clone(),
    )
    .await
}

pub(super) async fn attach(
    paths: &Paths,
    machine: &Name,
    selector: &str,
    read_only: bool,
) -> anyhow::Result<()> {
    let client = Client::open(paths, machine)?;
    let rows = client
        .list()
        .await
        .with_context(|| format!("{machine} is unreachable"))?;
    let mut matches = rows.into_iter().filter(|row| {
        row.session == selector
            || row.label == selector
            || row.primary == selector
            || row.alias.as_deref() == Some(selector)
    });
    let row = matches
        .next()
        .context("unknown hosted session on paired machine")?;
    if matches.next().is_some() {
        bail!("ambiguous remote session, use its committee-qualified name");
    }
    ensure!(
        row.running,
        "remote session is not running; restore it on its host"
    );
    let tile = tile(row, machine);
    if read_only {
        return host::view::watch_remote_one(tile, client).await;
    }
    super::require_attach_terminal()?;
    host::attach_stream(client.stream(tile.key, true).await?, &tile.title, None).await
}
