use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, bail};
use base64::Engine as _;
use clap::Args;
use senate_observe::{CreateGrant, Recording, Registry, Scope};
use senate_observe_client::{Client, Frame, Update};
use senate_observe_protocol::recording::{self, Content, Format, Header, Row};
use senate_observe_protocol::{Cursor, V1};
use serde::Serialize;

const DEFAULT_MAX_BYTES: usize = 100 * 1024 * 1024;

#[derive(Args)]
pub(super) struct Record {
    /// Numeric loopback address printed when issuing the grant.
    #[arg(long)]
    endpoint: String,
    /// Owner-only file containing the bearer, without a prefix.
    #[arg(long)]
    token_file: PathBuf,
    /// New recording file to create.
    #[arg(long)]
    out: PathBuf,
    /// Preserve previews permitted by the current grant.
    #[arg(long)]
    include_lines: bool,
    /// Maximum file size in MiB.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=1024))]
    max_mib: u32,
}

#[derive(Args)]
pub(super) struct Replay {
    /// Observe recording to replay.
    #[arg(required_unless_present = "demo", conflicts_with = "demo")]
    file: Option<PathBuf>,
    /// Run the built-in synthetic scene.
    #[arg(long)]
    demo: bool,
    /// Optional exact browser origin authorized to connect.
    #[arg(long)]
    origin: Option<String>,
    /// Maximum accepted recording size in MiB.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=1024))]
    max_mib: u32,
}

pub(super) async fn record(args: Record) -> anyhow::Result<()> {
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;

    let token_file = std::fs::File::open(&args.token_file)?;
    if token_file.metadata()?.permissions().mode() & 0o077 != 0 {
        bail!("Use an owner-only token file");
    }
    let mut token = String::new();
    token_file.take(513).read_to_string(&mut token)?;
    if token.len() > 512 || token.trim().is_empty() {
        bail!("Invalid token file");
    }
    let client = Client::new(&args.endpoint, token.trim())?;
    let mut follow = client.follow();
    let Update::Snapshot(mut state) = follow.next().await? else {
        bail!("Observe did not return an initial snapshot");
    };
    let maximum = usize::try_from(args.max_mib)
        .ok()
        .and_then(|mib| mib.checked_mul(1024 * 1024))
        .unwrap_or(DEFAULT_MAX_BYTES);
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&args.out)?;
    let mut writer = RecordWriter {
        file: BufWriter::new(file),
        maximum,
        written: 0,
        rows: 0,
    };
    let at = Instant::now();
    recording::sanitize_snapshot(&mut state, args.include_lines);
    writer.append(&Header {
        format: Format::SenateObserveRecording,
        version: V1,
        capabilities: state.capabilities.clone(),
    })?;
    state.cursor = writer.cursor()?;
    writer.row(0, Content::Snapshot(state))?;
    writer.file.flush()?;

    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    loop {
        let update = tokio::select! {
            result = &mut interrupt => { result?; break; }
            update = follow.next() => update?,
        };
        let offset_ms = u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX);
        if offset_ms >= 7 * 24 * 60 * 60 * 1000 {
            break;
        }
        match update {
            Update::Snapshot(mut state) => {
                recording::sanitize_snapshot(&mut state, args.include_lines);
                state.cursor = writer.cursor_after(1)?;
                let row = encode_row(&Row {
                    offset_ms,
                    row: Content::Snapshot(state),
                })?;
                if writer.available() < row.len() + 512 {
                    break;
                }
                writer.row(offset_ms, Content::Discontinuity)?;
                writer.append_bytes(&row)?;
                writer.rows += 1;
            }
            Update::Frame(frame) => {
                let Frame::Event(mut frame) = *frame else {
                    continue;
                };
                if !recording::sanitize_frame(&mut frame, args.include_lines) {
                    continue;
                }
                frame.cursor = writer.cursor()?;
                let row = encode_row(&Row {
                    offset_ms,
                    row: Content::Frame(Box::new(frame)),
                })?;
                if writer.available() < row.len() + 256 {
                    break;
                }
                writer.append_bytes(&row)?;
                writer.rows += 1;
            }
        }
        writer.file.flush()?;
    }
    let offset = u64::try_from(at.elapsed().as_millis())
        .unwrap_or(u64::MAX)
        .min(7 * 24 * 60 * 60 * 1000);
    writer.row(offset, Content::End)?;
    writer.file.flush()?;
    writer.file.get_ref().sync_all()?;
    Ok(())
}

struct RecordWriter {
    file: BufWriter<std::fs::File>,
    maximum: usize,
    written: usize,
    rows: u64,
}

impl RecordWriter {
    fn available(&self) -> usize {
        self.maximum.saturating_sub(self.written)
    }

    fn cursor(&self) -> anyhow::Result<Cursor> {
        self.cursor_after(0)
    }

    fn cursor_after(&self, rows: u64) -> anyhow::Result<Cursor> {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(u128::from(self.rows + rows + 1).to_be_bytes())
            .parse::<Cursor>()
            .map_err(anyhow::Error::msg)
    }

    fn append(&mut self, value: &impl Serialize) -> anyhow::Result<()> {
        self.append_bytes(&encode_row(value)?)
    }

    fn append_bytes(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        if self.available() <= bytes.len() {
            bail!("Observe recording reached its byte limit");
        }
        self.file.write_all(bytes)?;
        self.file.write_all(b"\n")?;
        self.written += bytes.len() + 1;
        Ok(())
    }

    fn row(&mut self, offset_ms: u64, row: Content) -> anyhow::Result<()> {
        self.append(&Row { offset_ms, row })?;
        self.rows += 1;
        Ok(())
    }
}

fn encode_row(value: &impl Serialize) -> anyhow::Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() >= recording::MAX_ROW_BYTES {
        bail!("Observe recording row exceeds its byte limit");
    }
    Ok(bytes)
}

pub(super) async fn replay(args: Replay) -> anyhow::Result<()> {
    let recording = if args.demo {
        Recording::demo()?
    } else {
        Recording::read_with_limit(
            &args.file.context("choose a recording or --demo")?,
            u64::from(args.max_mib) * 1024 * 1024,
        )?
    };
    if recording.includes_lines() {
        bail!(
            "Replaying message lines requires daemon-verified person confirmation, which is unavailable. Use a metadata recording."
        );
    }
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let home = tempfile::tempdir()?;
    let registry = Registry::open(home.path(), listener.local_addr()?)?;
    recording.attach(&registry);
    let grant = registry.create(
        CreateGrant {
            name: "Observe replay".into(),
            committees: None,
            all_committees: true,
            scope: Scope::Metadata,
            lines: None,

            origins: args.origin.into_iter().collect(),
            topology: true,
            expires_in_days: Some(1),
        },
        Default::default(),
    )?;
    super::print_json(&grant)?;
    std::io::stdout().flush()?;
    let app = senate_observe::router(registry.clone())
        .into_make_service_with_connect_info::<SocketAddr>();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            registry.shutdown_observation();
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_writer_rejects_rows_replay_cannot_read() -> anyhow::Result<()> {
        let payload = "x".repeat(recording::MAX_ROW_BYTES - 3);
        assert_eq!(encode_row(&payload)?.len() + 1, recording::MAX_ROW_BYTES);
        assert!(encode_row(&(payload + "x")).is_err());
        Ok(())
    }
}
