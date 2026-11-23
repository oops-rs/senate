//! Only the terminal input frames deliberately supported by remote attach
//! can cross to the local host. Watch has no input path.

use std::time::Duration;

use anyhow::{Context, bail, ensure};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, split};
use tokio::net::UnixStream;
use tokio::sync::watch;
use tokio::time::timeout;

use crate::host::{COLORS, IO_CHUNK, KEYS, RESIZE};

const IDLE: Duration = Duration::from_secs(30 * 60);
const IO_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) async fn relay(
    wire: impl AsyncRead + AsyncWrite + Unpin,
    mut host: UnixStream,
    attach: bool,
) -> anyhow::Result<()> {
    let (mut incoming, mut outgoing) = split(wire);
    let (activity, mut events) = watch::channel(());
    let idle = async {
        while let Ok(Ok(())) = timeout(IDLE, events.changed()).await {}
        bail!("remote terminal idle timeout")
    };
    if attach {
        let (mut display, mut input) = host.split();
        tokio::select! {
            result = attach_input(&mut incoming, &mut input, &activity) => result,
            result = output(&mut display, &mut outgoing, &activity) => result,
            result = idle => result,
        }
    } else {
        tokio::select! {
            _ = incoming.read_u8() => Ok(()),
            result = output(&mut host, &mut outgoing, &activity) => result,
            result = idle => result,
        }
    }
}

async fn send(to: &mut (impl AsyncWrite + Unpin), bytes: &[u8]) -> anyhow::Result<()> {
    timeout(IO_TIMEOUT, async {
        to.write_all(bytes).await?;
        to.flush().await
    })
    .await
    .context("remote terminal write stalled")??;
    Ok(())
}

async fn output(
    from: &mut (impl AsyncRead + Unpin),
    to: &mut (impl AsyncWrite + Unpin),
    activity: &watch::Sender<()>,
) -> anyhow::Result<()> {
    let mut bytes = [0; IO_CHUNK];
    loop {
        let len = from.read(&mut bytes).await?;
        if len == 0 {
            return Ok(());
        }
        send(to, &bytes[..len]).await?;
        activity.send_replace(());
    }
}

async fn attach_input(
    from: &mut (impl AsyncRead + Unpin),
    to: &mut (impl AsyncWrite + Unpin),
    activity: &watch::Sender<()>,
) -> anyhow::Result<()> {
    let mut frame = [0; IO_CHUNK + 3];
    loop {
        if from.read(&mut frame[..1]).await? == 0 {
            return Ok(());
        }
        let len = timeout(IO_TIMEOUT, async {
            match frame[0] {
                RESIZE => {
                    from.read_exact(&mut frame[1..5]).await?;
                    Ok(5)
                }
                KEYS | COLORS => {
                    let length = from.read_u16().await?;
                    let n = usize::from(length);
                    ensure!(n <= IO_CHUNK, "oversized terminal input");
                    frame[1..3].copy_from_slice(&length.to_be_bytes());
                    from.read_exact(&mut frame[3..3 + n]).await?;
                    Ok(3 + n)
                }
                _ => bail!("unknown remote terminal input"),
            }
        })
        .await
        .context("incomplete remote terminal input")??;
        send(to, &frame[..len]).await?;
        activity.send_replace(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn attach_forwards_only_complete_known_frames() -> anyhow::Result<()> {
        let input = [RESIZE, 0, 24, 0, 80, KEYS, 0, 2, b'a', b'b', 99];
        let mut from = input.as_slice();
        let mut host = Vec::new();
        assert!(
            attach_input(&mut from, &mut host, &watch::channel(()).0)
                .await
                .is_err()
        );
        assert_eq!(host, &input[..input.len() - 1]);
        let mut from = [KEYS, 0, 2, b'a'].as_slice();
        assert!(
            attach_input(&mut from, &mut host, &watch::channel(()).0)
                .await
                .is_err()
        );
        assert_eq!(host, &input[..input.len() - 1]);
        Ok(())
    }
}
