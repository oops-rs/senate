use std::io;

use anyhow::{Context, Result, ensure};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

pub(super) const MAX_FRAME: usize = 1024 * 1024;

/// Keep partially consumed bytes outside the future. The session can service
/// an outbound call while only the beginning of the adapter's frame is here.
pub(super) async fn read<R, T>(reader: &mut R, partial: &mut Vec<u8>) -> Result<T>
where
    R: AsyncBufRead + Unpin,
    T: DeserializeOwned,
{
    loop {
        let chunk = reader.fill_buf().await?;
        ensure!(!chunk.is_empty(), "gateway disconnected before a frame");
        let end = chunk.iter().position(|byte| *byte == b'\n');
        let len = end.unwrap_or(chunk.len());
        ensure!(len <= MAX_FRAME - partial.len(), "gateway frame too large");
        partial.extend_from_slice(&chunk[..len]);
        reader.consume(len + usize::from(end.is_some()));
        if end.is_some() {
            let frame = serde_json::from_slice(partial).context("invalid gateway frame")?;
            partial.clear();
            return Ok(frame);
        }
    }
}

pub(super) async fn write<W: AsyncWrite + Unpin>(writer: &mut W, frame: Value) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(&frame)?;
    if bytes.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "gateway frame too large",
        ));
    }
    bytes.push(b'\n');
    writer.write_all(&bytes).await
}
