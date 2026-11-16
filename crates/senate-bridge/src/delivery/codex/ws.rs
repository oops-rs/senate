//! The least of RFC 6455 a client of the Codex app-server needs: the
//! upgrade, masked text frames out, text frames in (fragmented or not),
//! pings answered, close honoured. Over any byte stream, so the same code
//! runs on a unix socket and through `codex app-server proxy`'s stdio.
//!
//! The peer is a local process the user runs, so `Sec-WebSocket-Accept` is
//! not checked and the masking key need not be unpredictable.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};

/// Largest message accepted from the server (Codex announces 16 MiB).
pub const MAX_MESSAGE: usize = 16 << 20;
/// Largest upgrade response accepted.
const MAX_HEAD: usize = 16 << 10;

pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

pub struct WebSocket {
    reader: WsReader,
    writer: WsWriter,
}

fn seed() -> u64 {
    let mut h = RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
    );
    h.finish() | 1
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(B64[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A client frame: FIN set, masked.
pub fn client_frame(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | (opcode & 0x0f));
    let len = payload.len();
    if len < 126 {
        out.push(0x80 | len as u8);
    } else if let Ok(len) = u16::try_from(len) {
        out.push(0x80 | 126);
        out.extend_from_slice(&len.to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    out
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

impl WebSocket {
    /// Send the upgrade request and wait for `101`.
    pub async fn handshake(mut stream: Box<dyn Stream>) -> io::Result<Self> {
        let mut rng = seed();
        let mut key = [0u8; 16];
        for chunk in key.chunks_mut(8) {
            rng = xorshift(rng);
            chunk.copy_from_slice(&rng.to_le_bytes()[..chunk.len()]);
        }
        let request = format!(
            "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\r\n",
            base64(&key)
        );
        stream.write_all(request.as_bytes()).await?;
        stream.flush().await?;
        let mut buf = Vec::new();
        let head_end = loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            if buf.len() > MAX_HEAD {
                return Err(bad("upgrade response too long"));
            }
            let mut chunk = [0u8; 4096];
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "closed before the WebSocket upgrade",
                ));
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let status = head.lines().next().unwrap_or("");
        if status.split_whitespace().nth(1) != Some("101") {
            return Err(bad(format!("upgrade refused: {status}")));
        }
        buf.drain(..head_end);
        let (read, write) = tokio::io::split(stream);
        Ok(Self {
            reader: WsReader {
                read,
                buf,
                message: Vec::new(),
                in_message: false,
            },
            writer: WsWriter { write, rng },
        })
    }

    pub async fn send_text(&mut self, text: &str) -> io::Result<()> {
        self.writer.send_text(text).await
    }

    /// The next text message; pings are answered on the way.
    pub async fn recv_text(&mut self) -> io::Result<String> {
        loop {
            match self.reader.next().await? {
                Incoming::Text(text) => return Ok(text),
                Incoming::Ping(payload) => self.writer.pong(&payload).await?,
                Incoming::Close(payload) => {
                    // Echo the close; the connection is over either way.
                    let _ = self.writer.close(&payload).await;
                    return Err(closed());
                }
            }
        }
    }

    /// The two directions apart, so one task can read while others write.
    pub fn into_split(self) -> (WsReader, WsWriter) {
        (self.reader, self.writer)
    }
}

fn closed() -> io::Error {
    io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "the app-server closed the WebSocket",
    )
}

/// What [`WsReader::next`] read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    Text(String),
    /// Answer with [`WsWriter::pong`].
    Ping(Vec<u8>),
    /// The server is closing; echo with [`WsWriter::close`] and stop.
    Close(Vec<u8>),
}

/// The receiving half. [`WsReader::next`] keeps a partly read message in
/// the reader, so a read that is dropped half-way loses nothing.
pub struct WsReader {
    read: ReadHalf<Box<dyn Stream>>,
    /// Bytes read past what has been parsed.
    buf: Vec<u8>,
    /// A fragmented message so far.
    message: Vec<u8>,
    in_message: bool,
}

impl WsReader {
    async fn need(&mut self, n: usize) -> io::Result<()> {
        while self.buf.len() < n {
            let mut chunk = [0u8; 8192];
            let got = self.read.read(&mut chunk).await?;
            if got == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the app-server closed the connection",
                ));
            }
            self.buf.extend_from_slice(&chunk[..got]);
        }
        Ok(())
    }

    /// `(fin, opcode, payload)`.
    async fn frame(&mut self) -> io::Result<(bool, u8, Vec<u8>)> {
        self.need(2).await?;
        let (b0, b1) = (self.buf[0], self.buf[1]);
        let masked = b1 & 0x80 != 0;
        let mut header = 2;
        let len = match b1 & 0x7f {
            126 => {
                self.need(4).await?;
                header = 4;
                usize::from(u16::from_be_bytes([self.buf[2], self.buf[3]]))
            }
            127 => {
                self.need(10).await?;
                header = 10;
                let mut b = [0u8; 8];
                b.copy_from_slice(&self.buf[2..10]);
                usize::try_from(u64::from_be_bytes(b)).unwrap_or(usize::MAX)
            }
            n => usize::from(n),
        };
        if len > MAX_MESSAGE {
            return Err(bad(format!("frame of {len} bytes")));
        }
        let mask_len = if masked { 4 } else { 0 };
        self.need(header + mask_len + len).await?;
        let mut mask = [0u8; 4];
        if masked {
            mask.copy_from_slice(&self.buf[header..header + 4]);
        }
        let start = header + mask_len;
        let mut payload: Vec<u8> = self.buf[start..start + len].to_vec();
        if masked {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask[i % 4];
            }
        }
        self.buf.drain(..start + len);
        Ok((b0 & 0x80 != 0, b0 & 0x0f, payload))
    }

    /// The next text message, ping or close.
    pub async fn next(&mut self) -> io::Result<Incoming> {
        loop {
            let (fin, opcode, payload) = self.frame().await?;
            match opcode {
                0x0..=0x2 => {
                    if opcode != 0x0 {
                        self.message.clear();
                    } else if !self.in_message {
                        return Err(bad("continuation without a message"));
                    }
                    self.in_message = true;
                    self.message.extend_from_slice(&payload);
                    if self.message.len() > MAX_MESSAGE {
                        return Err(bad("message too long"));
                    }
                    if fin {
                        self.in_message = false;
                        let message = std::mem::take(&mut self.message);
                        return String::from_utf8(message)
                            .map(Incoming::Text)
                            .map_err(|_| bad("text is not UTF-8"));
                    }
                }
                0x8 => return Ok(Incoming::Close(payload)),
                0x9 => return Ok(Incoming::Ping(payload)),
                _ => {}
            }
        }
    }
}

/// The sending half: masked frames out.
pub struct WsWriter {
    write: WriteHalf<Box<dyn Stream>>,
    rng: u64,
}

impl WsWriter {
    fn mask(&mut self) -> [u8; 4] {
        self.rng = xorshift(self.rng);
        let b = self.rng.to_le_bytes();
        [b[0], b[1], b[2], b[3]]
    }

    async fn send(&mut self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mask = self.mask();
        self.write
            .write_all(&client_frame(opcode, payload, mask))
            .await?;
        self.write.flush().await
    }

    pub async fn send_text(&mut self, text: &str) -> io::Result<()> {
        self.send(0x1, text.as_bytes()).await
    }

    pub async fn pong(&mut self, payload: &[u8]) -> io::Result<()> {
        self.send(0xA, payload).await
    }

    pub async fn close(&mut self, payload: &[u8]) -> io::Result<()> {
        self.send(0x8, payload).await
    }
}

fn xorshift(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_examples() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0u8; 16]).len(), 24);
    }

    #[test]
    fn frames_are_masked_with_the_right_length() {
        let f = client_frame(1, b"Hello", [0x37, 0xfa, 0x21, 0x3d]);
        // RFC 6455 §5.7: a masked "Hello".
        assert_eq!(
            f,
            [
                0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58
            ]
        );
        let long = client_frame(1, &[0u8; 300], [0; 4]);
        assert_eq!(&long[..4], &[0x81, 0x80 | 126, 0x01, 0x2c]);
        let huge = client_frame(1, &vec![0u8; 70_000], [0; 4]);
        assert_eq!(huge[1], 0x80 | 127);
    }
}
