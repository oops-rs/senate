use std::io::{self, Write as _};
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, ensure};
use clap::Subcommand;
use clap_complete::engine::CompletionCandidate;

use crate::host::{self, view::Session};
use crate::ipc::Request;
use crate::paths::Paths;
use crate::remote::{self, CandidateDetails, Client, Control, Device, Row, Scopes};
use senate_core::{Name, SenatorName};

#[derive(Subcommand)]
pub(super) enum Command {
    /// Show how to install the separately built administrator helper.
    InstallHelper {
        /// Path to the helper artifact from the trusted build or release.
        artifact: PathBuf,
    },
    /// Prepare the exact host key and address for administrator approval.
    Enable {
        /// Exact IP address and port to authorize on the target.
        #[arg(long)]
        listen: SocketAddr,
    },
    /// Print the administrator command to disable the listener and its grants.
    Disable,
    /// Create an expiring offer and prepare the administrator enrollment command.
    Pair {
        /// Give the device its own interactive attach grant.
        #[arg(long)]
        attach: bool,
        /// Only grant metadata listing, with no terminal streams.
        #[arg(long, conflicts_with = "attach")]
        list_only: bool,
        /// Maximum grant lifetime in seconds from the start of enrollment.
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
    /// Print the administrator command to revoke a device and end its streams.
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
        Command::InstallHelper { artifact } => {
            use sha2::{Digest, Sha256};
            remote::authority::check_install_location()?;
            ensure!(
                artifact.symlink_metadata()?.is_file(),
                "helper source must be a regular file, not a symlink"
            );
            let artifact = artifact.canonicalize()?;
            let source = artifact.to_str().context("helper path must be UTF-8")?;
            ensure!(
                !source.chars().any(remote::authority::unsafe_display),
                "helper path contains unsafe display characters"
            );
            let digest = hex::encode(Sha256::digest(std::fs::read(&artifact)?));
            println!(
                "Source SHA-256 {digest}. Use the checksum from a trusted release channel as your reference."
            );
            println!("Run these commands in your native target terminal:");
            if cfg!(target_os = "macos") {
                println!(
                    "sudo -k /usr/bin/stat -f '%N %Su %Sp' / /usr /usr/local /usr/local/libexec"
                );
            } else {
                println!(
                    "sudo -k /usr/bin/stat -c '%n %U %A' / /usr /usr/local /usr/local/libexec"
                );
            }
            println!(
                "Check that every existing directory is root-owned and has no group or other write access."
            );
            println!(
                "sudo -k /usr/bin/install -d -o root -m 0755 /usr/local/libexec {}",
                remote::authority::SYSTEM_DIR
            );
            println!(
                r#"stage=$(sudo -k /usr/bin/mktemp -d /usr/local/libexec/.senate-remote-admin.XXXXXXXX)"#
            );
            println!(
                r#"sudo -k /usr/bin/install -m 0700 /dev/null "${{stage:?create the root staging directory first}}/senate-remote-admin""#
            );
            // The shell opens the source as the ordinary user, before sudo.
            // Root never resolves any pathname from the download directory.
            println!(
                r#"test ! -L '{0}' && sudo -k /bin/sh -c 'exec /bin/cat > "$1"' sh "${{stage:?create the root staging directory first}}/senate-remote-admin" < '{0}'"#,
                source.replace('\'', "'\"'\"'")
            );
            println!(
                r#"sudo -k /usr/bin/shasum -a 256 "${{stage:?create the root staging directory first}}/senate-remote-admin""#
            );
            if cfg!(target_os = "macos")
                && let Some(team) = option_env!("SENATE_REMOTE_ADMIN_TEAM_ID")
            {
                ensure!(
                    team.len() == 10
                        && team
                            .bytes()
                            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit()),
                    "invalid helper signing team"
                );
                println!(
                    r#"sudo -k /usr/bin/codesign --verify --strict -R '=anchor apple generic and identifier "dev.senate.remote-admin" and certificate leaf[subject.OU] = "{team}"' "${{stage:?create the root staging directory first}}/senate-remote-admin""#
                );
            }
            println!(
                "Stop here unless the staged SHA-256 matches the independently obtained reference."
            );
            println!(
                r#"sudo -k /usr/bin/install -m 0440 /dev/null "${{stage:?create the root staging directory first}}/sudoers""#
            );
            println!(
                r#"printf '%s\n' '{}' | sudo -k /bin/sh -c 'exec /bin/cat > "$1"' sh "${{stage:?create the root staging directory first}}/sudoers""#,
                remote::authority::SUDOERS_RULE
            );
            println!(
                r#"sudo -k /usr/sbin/visudo -cf "${{stage:?create the root staging directory first}}/sudoers" && sudo -k /bin/mv -f "$stage/sudoers" {}"#,
                remote::authority::SUDOERS_PATH
            );
            println!("sudo -k /usr/sbin/visudo -c");
            println!(
                "Verify the parsed configuration lists /etc/sudoers.d/99-senate-remote-admin before activating the helper."
            );
            println!(
                r#"sudo -k /bin/mv -f "${{stage:?create the root staging directory first}}/senate-remote-admin" {}"#,
                remote::authority::HELPER
            );
            println!("sudo -k /bin/chmod 0755 {}", remote::authority::HELPER);
            println!(r#"sudo -k /bin/rmdir "${{stage:?create the root staging directory first}}""#);
        }
        Command::Enable { listen } => {
            let request: String = local(&rt, &paths, Control::PrepareEnable { listen })?;
            print_admin("enable", Some(&request))?;
            println!(
                "Set [remote] experimental = true and restart the daemon after authorization."
            );
        }
        Command::Disable => print_admin("disable", None)?,
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
            let candidate = rt.block_on(async {
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
                loop {
                    let pending: Option<CandidateDetails> = super::daemon_call(
                        &paths,
                        &Request::RemoteControl {
                            command: Control::Candidate,
                        },
                    )
                    .await?;
                    if let Some(candidate) = pending {
                        break Ok::<_, anyhow::Error>(candidate);
                    }
                    ensure!(tokio::time::Instant::now() < deadline, "pairing expired");
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
            })?;
            println!(
                "Candidate {} · key {} · watch {} · attach {} · valid {}s",
                candidate.name,
                candidate.key_prefix,
                candidate.scopes.watch,
                candidate.scopes.attach,
                candidate.valid_secs
            );
            let request: String = local(&rt, &paths, Control::PrepareEnroll)?;
            print_admin("enroll", Some(&request))?;
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
            ensure!(
                remote::authority::pin(&device),
                "expected the full device SPKI"
            );
            print_admin("revoke", Some(&device))?;
        }
    }
    Ok(())
}

fn validate_admin_argument(argument: Option<&str>) -> anyhow::Result<()> {
    if let Some(argument) = argument {
        ensure!(
            !argument.is_empty()
                && argument.len() <= remote::authority::REQUEST_LIMIT
                && argument
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
            "invalid administrator request argument"
        );
    }
    Ok(())
}

fn print_admin(action: &str, argument: Option<&str>) -> anyhow::Result<()> {
    validate_admin_argument(argument)?;
    let helper = remote::authority::HELPER;
    print!("sudo -k {helper} {action}");
    if let Some(argument) = argument {
        print!(" {argument}");
    }
    println!();
    Ok(())
}

fn label(row: &Row, machine: &Name) -> String {
    crate::terminal::printable(&format!("{machine} · {} · {}", row.label, row.runtime))
        .chars()
        .filter(|ch| !remote::authority::unsafe_display(*ch))
        .collect()
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

pub(super) async fn find_senators(paths: &Paths, senators: &[SenatorName]) -> Vec<Name> {
    let names = remote::names(paths);
    let mut locations = futures::future::join_all(names.into_iter().map(|machine| async move {
        let client = Client::open(paths, &machine).ok()?;
        let rows = client.list().await.ok()?;
        rows.iter()
            .any(|row| {
                senators
                    .iter()
                    .any(|senator| row.primary == senator.as_str())
            })
            .then_some(machine)
    }))
    .await
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    locations.sort();
    locations
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

pub(super) enum AttachSelector {
    Name(String),
    Role(Vec<SenatorName>),
}

pub(super) async fn attach(
    paths: &Paths,
    machine: &Name,
    selector: AttachSelector,
    read_only: bool,
) -> anyhow::Result<()> {
    let client = Client::open(paths, machine)?;
    let rows = client
        .list()
        .await
        .with_context(|| format!("{machine} is unreachable"))?;
    let row = match selector {
        AttachSelector::Name(selector) => {
            let mut matches = rows.into_iter().filter(|row| {
                row.session == selector
                    || row.label == selector
                    || row.primary == selector
                    || row.alias.as_deref() == Some(selector.as_str())
            });
            let row = matches
                .next()
                .context("unknown hosted session on paired machine")?;
            ensure!(
                matches.next().is_none(),
                "ambiguous remote session, use its committee-qualified name"
            );
            row
        }
        AttachSelector::Role(candidates) => candidates
            .iter()
            .flat_map(|senator| {
                rows.iter()
                    .filter(move |row| row.primary == senator.as_str())
            })
            .min_by_key(|row| !row.running)
            .cloned()
            .with_context(|| format!("no session for that role on {machine}"))?,
    };
    ensure!(
        row.running,
        "remote session is not running; restore it on its host"
    );
    let tile = tile(row, machine);
    if read_only {
        return host::view::watch_remote_one(tile, client).await;
    }
    super::require_attach_terminal()?;
    eprintln!("Authenticate writable attach on the hosting Mac.");
    host::attach_stream(client.stream(tile.key, true).await?, &tile.title, None).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn administrator_argument_has_a_bounded_shell_safe_alphabet() -> anyhow::Result<()> {
        validate_admin_argument(None)?;
        validate_admin_argument(Some("Ab9_-"))?;
        for invalid in ["", "has space", "has\nnewline", "'", "a;b", "=padding"] {
            assert!(validate_admin_argument(Some(invalid)).is_err());
        }
        assert!(
            validate_admin_argument(Some(&"a".repeat(remote::authority::REQUEST_LIMIT + 1)))
                .is_err()
        );
        Ok(())
    }
}
