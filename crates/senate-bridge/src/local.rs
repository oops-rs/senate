//! The local IPC transport between the daemon and its clients on the same
//! machine (the CLI, the MCP server, hooks): a Unix domain socket on Unix,
//! a named pipe on Windows. Both carry the same newline-delimited JSON
//! (`crate::ipc`), and on both the kernel names the peer process, which is
//! what the daemon's caller checks rest on.
//!
//! On Unix this module only re-exports tokio's Unix socket types, so
//! nothing there changes. On Windows a socket path such as
//! `~\.senate\bridge.sock` names a pipe (`\\.\pipe\senate-<hash of the
//! path>`), and the path itself holds a marker file while the listener is
//! bound, so the code that asks whether a daemon's socket exists works
//! unchanged. The pipe:
//!
//! - is created with a DACL that grants the person's SID alone, protected
//!   from inheritance (the 0600 of the Unix socket);
//! - refuses remote clients, and refuses to start when another process
//!   already holds the name (`first_pipe_instance`), so nothing can sit in
//!   front of the daemon;
//! - is checked by each client: the server's process must run as the same
//!   user, or the connection is dropped before a byte is sent;
//! - names its client to the server ([`Stream::peer_pid`], from
//!   `GetNamedPipeClientProcessId`), and the server checks that the client
//!   runs as the same user too.

#[cfg(unix)]
pub use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
#[cfg(unix)]
pub use tokio::net::{UnixListener as Listener, UnixStream as Stream};

#[cfg(windows)]
pub use windows::{Listener, OwnedReadHalf, OwnedWriteHalf, Stream, pipe_name};

#[cfg(windows)]
mod windows {
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use sha2::{Digest, Sha256};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY};

    use crate::win;

    /// How long a client waits for a busy or momentarily absent instance.
    const CONNECT_PATIENCE: Duration = Duration::from_secs(2);

    /// The pipe a socket path names. Windows paths are case-insensitive
    /// and take either separator, so the name is too.
    pub fn pipe_name(path: &Path) -> String {
        let normal = path
            .to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase();
        let digest = hex::encode(Sha256::digest(normal.as_bytes()));
        format!(r"\\.\pipe\senate-{}", &digest[..32])
    }

    fn denied(what: &str) -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, what.to_owned())
    }

    enum Inner {
        Server(NamedPipeServer),
        Client(NamedPipeClient),
    }

    /// One connected end of the pipe.
    pub struct Stream {
        inner: Inner,
        peer: u32,
    }

    pub type OwnedReadHalf = tokio::io::ReadHalf<Stream>;
    pub type OwnedWriteHalf = tokio::io::WriteHalf<Stream>;

    impl Stream {
        /// Connect to the listener bound at `path`, and check that it runs
        /// as this user.
        pub async fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
            let name = pipe_name(path.as_ref());
            let deadline = tokio::time::Instant::now() + CONNECT_PATIENCE;
            let client = loop {
                match ClientOptions::new().open(&name) {
                    Ok(client) => break client,
                    // Every instance is busy, or the listener is between
                    // instances: try again shortly while its marker is there.
                    Err(error)
                        if (error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                            || (error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32)
                                && path.as_ref().exists()))
                            && tokio::time::Instant::now() < deadline =>
                    {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(error) => return Err(error),
                }
            };
            let server = win::pipe_server(client.as_raw_handle())?;
            if win::process_user(server)? != win::current_user()? {
                return Err(denied("the pipe's server runs as another user"));
            }
            Ok(Self {
                inner: Inner::Client(client),
                peer: server,
            })
        }

        /// The process at the other end, as the kernel reported it when
        /// the connection was made.
        pub fn peer_pid(&self) -> u32 {
            self.peer
        }

        pub fn into_split(self) -> (OwnedReadHalf, OwnedWriteHalf) {
            tokio::io::split(self)
        }

        /// Both directions, borrowed.
        pub fn split(
            &mut self,
        ) -> (
            tokio::io::ReadHalf<&mut Self>,
            tokio::io::WriteHalf<&mut Self>,
        ) {
            tokio::io::split(self)
        }

        /// Read what is already there without waiting.
        pub fn try_read(&self, buf: &mut [u8]) -> io::Result<usize> {
            match &self.inner {
                Inner::Server(pipe) => pipe.try_read(buf),
                Inner::Client(pipe) => pipe.try_read(buf),
            }
        }
    }

    impl AsyncRead for Stream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            match &mut self.get_mut().inner {
                Inner::Server(pipe) => Pin::new(pipe).poll_read(cx, buf),
                Inner::Client(pipe) => Pin::new(pipe).poll_read(cx, buf),
            }
        }
    }

    impl AsyncWrite for Stream {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            match &mut self.get_mut().inner {
                Inner::Server(pipe) => Pin::new(pipe).poll_write(cx, buf),
                Inner::Client(pipe) => Pin::new(pipe).poll_write(cx, buf),
            }
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match &mut self.get_mut().inner {
                Inner::Server(pipe) => Pin::new(pipe).poll_flush(cx),
                Inner::Client(pipe) => Pin::new(pipe).poll_flush(cx),
            }
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match &mut self.get_mut().inner {
                Inner::Server(pipe) => Pin::new(pipe).poll_shutdown(cx),
                Inner::Client(pipe) => Pin::new(pipe).poll_shutdown(cx),
            }
        }
    }

    /// The server side: one pipe name, a fresh instance for each client.
    pub struct Listener {
        name: String,
        marker: PathBuf,
        security: win::OwnerOnly,
        me: win::Sid,
        next: tokio::sync::Mutex<NamedPipeServer>,
    }

    #[allow(unsafe_code)]
    fn instance(name: &str, security: &win::OwnerOnly, first: bool) -> io::Result<NamedPipeServer> {
        let mut attributes = security.attributes();
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(first)
            .reject_remote_clients(true);
        // SAFETY: `attributes` points into `security`, alive for the call.
        unsafe { options.create_with_security_attributes_raw(name, (&raw mut attributes).cast()) }
    }

    impl Listener {
        /// Create the pipe `path` names, owner-only, and mark `path`.
        /// Fails when any process already holds the name.
        pub fn bind(path: impl AsRef<Path>) -> io::Result<Self> {
            let path = path.as_ref();
            let name = pipe_name(path);
            let me = win::current_user()?;
            let security = win::OwnerOnly::new(&me)?;
            let first = instance(&name, &security, true)?;
            std::fs::write(path, format!("{name}\n"))?;
            Ok(Self {
                name,
                marker: path.to_path_buf(),
                security,
                me,
                next: tokio::sync::Mutex::new(first),
            })
        }

        /// The next client, checked to run as this user. A client of
        /// another user (which the DACL already keeps out) is dropped.
        pub async fn accept(&self) -> io::Result<(Stream, ())> {
            let mut next = self.next.lock().await;
            next.connect().await?;
            let fresh = instance(&self.name, &self.security, false)?;
            let server = std::mem::replace(&mut *next, fresh);
            drop(next);
            let peer = win::pipe_client(server.as_raw_handle())?;
            if win::process_user(peer)? != self.me {
                return Err(denied("a pipe client runs as another user"));
            }
            Ok((
                Stream {
                    inner: Inner::Server(server),
                    peer,
                },
                (),
            ))
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.marker);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        #[tokio::test]
        async fn a_client_and_the_listener_name_each_other() -> io::Result<()> {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("bridge.sock");
            let listener = Listener::bind(&path)?;
            assert!(path.exists(), "the marker names the pipe");
            let (client, accepted) = tokio::join!(Stream::connect(&path), listener.accept());
            let (mut client, (server, ())) = (client?, accepted?);
            assert_eq!(client.peer_pid(), std::process::id());
            assert_eq!(server.peer_pid(), std::process::id());
            client.write_all(b"ping\n").await?;
            let (read, mut write) = server.into_split();
            let mut line = String::new();
            BufReader::new(read).read_line(&mut line).await?;
            assert_eq!(line, "ping\n");
            write.write_all(b"pong\n").await?;
            let mut line = String::new();
            BufReader::new(client).read_line(&mut line).await?;
            assert_eq!(line, "pong\n");
            drop(listener);
            assert!(!path.exists(), "the marker goes with the listener");
            Ok(())
        }

        #[tokio::test]
        async fn many_clients_each_get_an_instance() -> io::Result<()> {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("bridge.sock");
            let listener = std::sync::Arc::new(Listener::bind(&path)?);
            let server = {
                let listener = listener.clone();
                tokio::spawn(async move {
                    for _ in 0..8 {
                        let (stream, ()) = listener.accept().await?;
                        tokio::spawn(async move {
                            let (read, mut write) = stream.into_split();
                            let mut line = String::new();
                            BufReader::new(read).read_line(&mut line).await?;
                            write.write_all(line.as_bytes()).await
                        });
                    }
                    io::Result::Ok(())
                })
            };
            let clients = (0..8).map(|i| {
                let path = path.clone();
                async move {
                    let mut stream = Stream::connect(&path).await?;
                    stream.write_all(format!("{i}\n").as_bytes()).await?;
                    let mut line = String::new();
                    BufReader::new(stream).read_line(&mut line).await?;
                    io::Result::Ok(line == format!("{i}\n"))
                }
            });
            for echoed in futures::future::join_all(clients).await {
                assert!(echoed?);
            }
            server.await.expect("server task")?;
            Ok(())
        }

        #[tokio::test]
        async fn a_held_name_cannot_be_bound_again() -> io::Result<()> {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("bridge.sock");
            let _first = Listener::bind(&path)?;
            assert!(Listener::bind(&path).is_err());
            // Nor can anything create a first instance in front of it.
            let squat = ServerOptions::new()
                .first_pipe_instance(true)
                .create(pipe_name(&path));
            assert!(squat.is_err());
            Ok(())
        }

        #[tokio::test]
        async fn the_pipe_admits_only_its_owner() -> io::Result<()> {
            // A pipe whose DACL grants someone else (here SYSTEM alone)
            // refuses this process: the DACL, not the name, decides.
            let name = pipe_name(Path::new(r"C:\senate-test\other-owner.sock"));
            let system = win::OwnerOnly::from_sddl("D:P(A;;GA;;;SY)")?;
            let _server = instance(&name, &system, true)?;
            let refused = ClientOptions::new().open(&name).expect_err("refused");
            assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied, "{refused}");
            // The person's own pipe admits them.
            let me = win::OwnerOnly::current()?;
            let own = pipe_name(Path::new(r"C:\senate-test\own.sock"));
            let _own = instance(&own, &me, true)?;
            assert!(ClientOptions::new().open(&own).is_ok());
            Ok(())
        }

        #[test]
        fn pipe_names_ignore_case_and_separators() {
            assert_eq!(
                pipe_name(Path::new(r"C:\Users\P\.senate\bridge.sock")),
                pipe_name(Path::new("c:/users/p/.senate/bridge.sock"))
            );
            assert_ne!(
                pipe_name(Path::new(r"C:\a\bridge.sock")),
                pipe_name(Path::new(r"C:\b\bridge.sock"))
            );
            assert!(pipe_name(Path::new("x")).starts_with(r"\\.\pipe\senate-"));
        }
    }
}
