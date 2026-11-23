//! Versioned direct machine access. Local IPC controls enrollment. The
//! remote transport accepts only this module's wire types.
mod tls;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, anyhow, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch};

use crate::paths::Paths;
use senate_core::Name;

const VERSION: u8 = 1;
const HANDSHAKE: Duration = Duration::from_secs(6);
const OFFER_TTL: Duration = Duration::from_secs(120);
const MAX_REQUEST: usize = 4096;
const MAX_REPLY: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Scopes {
    pub watch: bool,
    pub attach: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Control {
    Pair { scopes: Scopes, valid_secs: u64 },
    Confirm { comparison: String },
    Devices,
    Revoke { device: String },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Device {
    pub id: String,
    // Prevent a re-enrolled key from resuming a stream from a revoked grant.
    grant: String,
    pub name: Name,
    pub scopes: Scopes,
    pub expires: i64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OfferCode {
    v: u8,
    at: SocketAddr,
    host: String,
    secret: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u8,
    call: Call,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Call {
    Enroll { secret: String, name: Name },
    List {},
    Watch { session: String },
    Attach { session: String },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    pub session: String,
    pub label: String,
    pub alias: Option<String>,
    pub primary: String,
    pub runtime: String,
    pub running: bool,
    pub paused: bool,
    pub attached: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum Reply {
    Compare { code: String },
    Enrolled { device: Device },
    Rows { rows: Vec<Row> },
    Stream,
    Refused { message: String },
}

#[derive(Clone, Deserialize, Serialize)]
struct Identity {
    cert: String,
    key: String,
    keychain: bool,
}

#[derive(Clone, Deserialize, Serialize)]
struct Machine {
    at: SocketAddr,
    host: String,
    identity: Identity,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn fingerprint(cert: &[u8]) -> String {
    hex::encode(Sha256::digest(cert))
}

fn comparison_code(secret: &str, id: &str) -> String {
    let digest = Sha256::digest(format!("{secret}:{id}"));
    let n = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    format!("{:06}", n % 1_000_000)
}

fn random() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| anyhow!("generate random credential: {error}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn create_private_dir(path: &Path) -> anyhow::Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && metadata.mode() & 0o077 == 0,
        "remote state directory must be private"
    );
    Ok(())
}

fn state_dir(paths: &Paths) -> PathBuf {
    paths.home().join("remote")
}

fn read<T: DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.mode() & 0o077 == 0,
        "remote state file must be private"
    );
    Ok(serde_json::from_reader(file)?)
}

fn durable<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<tempfile::NamedTempFile> {
    let dir = path
        .parent()
        .context("remote state file has no directory")?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer(file.as_file_mut(), value)?;
    file.as_file().sync_all()?;
    Ok(file)
}

fn save<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    durable(path, value)?.persist(path)?;
    Ok(())
}

fn save_new<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    durable(path, value)?.persist_noclobber(path)?;
    Ok(())
}

fn locked<T>(state: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    state.lock().unwrap_or_else(|error| error.into_inner())
}

impl Identity {
    fn create(dir: &Path) -> anyhow::Result<Self> {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec![tls::HOST.into(), tls::PAIR.into()])?;
        let id = uuid::Uuid::new_v4().to_string();
        let secret = signing_key.serialize_der();
        #[cfg(target_os = "macos")]
        if std::env::var_os(crate::testenv::CONFINE_ENV).is_none() {
            match security_framework::passwords::set_generic_password(
                "dev.senate.remote",
                &id,
                &secret,
            ) {
                Ok(()) => {
                    return Ok(Self {
                        cert: URL_SAFE_NO_PAD.encode(cert.der()),
                        key: id,
                        keychain: true,
                    });
                }
                // errSecNotAvailable and errSecNoSuchKeychain. An ACL error
                // or an operator denying Keychain access is not a fallback.
                Err(error) if matches!(error.code(), -25291 | -25294) => {}
                Err(error) => {
                    return Err(error).context("store remote private key in macOS Keychain");
                }
            }
        }
        save_new(
            &dir.join(format!("key-{id}.json")),
            &URL_SAFE_NO_PAD.encode(secret),
        )?;
        Ok(Self {
            cert: URL_SAFE_NO_PAD.encode(cert.der()),
            key: id,
            keychain: false,
        })
    }

    fn secret(&self, dir: &Path) -> anyhow::Result<Vec<u8>> {
        ensure!(
            uuid::Uuid::parse_str(&self.key).is_ok(),
            "invalid key reference"
        );
        if self.keychain {
            #[cfg(target_os = "macos")]
            return security_framework::passwords::get_generic_password(
                "dev.senate.remote",
                &self.key,
            )
            .context("read Senate device key from Keychain");
            #[cfg(not(target_os = "macos"))]
            bail!("this key is in a macOS Keychain");
        }
        let encoded: String = read(&dir.join(format!("key-{}.json", self.key)))?;
        Ok(URL_SAFE_NO_PAD.decode(encoded)?)
    }

    fn remove(&self, dir: &Path) {
        if self.keychain {
            #[cfg(target_os = "macos")]
            let _ = security_framework::passwords::delete_generic_password(
                "dev.senate.remote",
                &self.key,
            );
        } else {
            let _ = std::fs::remove_file(dir.join(format!("key-{}.json", self.key)));
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Config {
    listen: Option<SocketAddr>,
}

#[derive(Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    remote: Config,
}

struct Offer {
    secret: String,
    scopes: Scopes,
    valid_secs: u64,
    expires: tokio::time::Instant,
    candidate: Option<Candidate>,
}

struct Candidate {
    id: String,
    name: Name,
    comparison: String,
    accepted: oneshot::Sender<Device>,
}

#[derive(Default)]
struct State {
    offer: Option<Offer>,
    devices: BTreeMap<String, Device>,
}

pub(crate) struct Service {
    dir: PathBuf,
    at: SocketAddr,
    identity: Identity,
    state: Mutex<State>,
    changes: watch::Sender<()>,
    audit_lock: Mutex<()>,
}

impl Service {
    pub(crate) async fn open(paths: &Paths) -> anyhow::Result<Option<(Arc<Self>, TcpListener)>> {
        let config = match std::fs::read_to_string(paths.config()) {
            Ok(text) => toml::from_str::<ConfigFile>(&text)?.remote,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(error) => return Err(error.into()),
        };
        let Some(at) = config.listen else {
            return Ok(None);
        };
        ensure!(
            !at.ip().is_unspecified() && !at.ip().is_multicast(),
            "remote listen needs an explicit unicast address"
        );
        if std::env::var_os(crate::testenv::CONFINE_ENV).is_some() {
            ensure!(
                at.ip().is_loopback() && crate::testenv::confined(paths.home()),
                "test listener must be isolated on loopback"
            );
        }
        let dir = state_dir(paths);
        create_private_dir(&dir)?;
        let path = dir.join("host.json");
        let identity = if path.exists() {
            read(&path)?
        } else {
            let identity = Identity::create(&dir)?;
            if let Err(error) = save_new(&path, &identity) {
                identity.remove(&dir);
                return Err(error);
            }
            identity
        };
        // Check keys and saved grants before accepting connections.
        identity.secret(&dir)?;
        let devices = if dir.join("devices.json").exists() {
            read(&dir.join("devices.json"))?
        } else {
            BTreeMap::new()
        };
        let listener = TcpListener::bind(at)
            .await
            .context("bind the configured remote address")?;
        let service = Arc::new(Self {
            dir,
            at: listener.local_addr()?,
            identity,
            state: Mutex::new(State {
                devices,
                offer: None,
            }),
            changes: watch::channel(()).0,
            audit_lock: Mutex::new(()),
        });
        Ok(Some((service, listener)))
    }

    fn device(&self, id: &str) -> Option<Device> {
        locked(&self.state)
            .devices
            .get(id)
            .filter(|d| d.expires > now())
            .cloned()
    }

    pub(crate) fn control(&self, request: Control) -> anyhow::Result<serde_json::Value> {
        let mut state = locked(&self.state);
        match request {
            Control::Pair { scopes, valid_secs } => {
                ensure!(
                    (1..=365 * 86400).contains(&valid_secs),
                    "device validity must be 1 second to 365 days"
                );
                let secret = random()?;
                let code = OfferCode {
                    v: VERSION,
                    at: self.at,
                    host: self.identity.cert.clone(),
                    secret: secret.clone(),
                };
                state.offer = Some(Offer {
                    secret,
                    scopes,
                    valid_secs,
                    expires: tokio::time::Instant::now() + OFFER_TTL,
                    candidate: None,
                });
                Ok(serde_json::json!(
                    URL_SAFE_NO_PAD.encode(serde_json::to_vec(&code)?)
                ))
            }
            Control::Confirm { comparison } => {
                let offer = state
                    .offer
                    .as_mut()
                    .filter(|offer| offer.expires > tokio::time::Instant::now())
                    .context("no active pairing")?;
                let candidate = offer
                    .candidate
                    .as_ref()
                    .context("the client has not requested pairing")?;
                ensure!(
                    comparison == candidate.comparison && !candidate.accepted.is_closed(),
                    "comparison code does not match an active pairing"
                );
                let device = Device {
                    id: candidate.id.clone(),
                    grant: uuid::Uuid::new_v4().to_string(),
                    name: candidate.name.clone(),
                    scopes: offer.scopes,
                    expires: now() + i64::try_from(offer.valid_secs)?,
                };
                let mut devices = state.devices.clone();
                devices.insert(device.id.clone(), device.clone());
                self.audit(&device.id, "pair", None)?;
                save(&self.dir.join("devices.json"), &devices)?;
                state.devices = devices;
                if let Some(candidate) = state.offer.take().and_then(|offer| offer.candidate) {
                    let _ = candidate.accepted.send(device.clone());
                }
                Ok(serde_json::to_value(device)?)
            }
            Control::Devices => Ok(serde_json::to_value(
                state.devices.values().collect::<Vec<_>>(),
            )?),
            Control::Revoke { device } => {
                let mut devices = state.devices.clone();
                ensure!(devices.remove(&device).is_some(), "unknown device");
                self.audit(&device, "revoke", None)?;
                save(&self.dir.join("devices.json"), &devices)?;
                state.devices = devices;
                self.changes.send_replace(());
                Ok(serde_json::Value::Null)
            }
        }
    }

    fn enroll(
        &self,
        id: String,
        secret: &str,
        name: Name,
    ) -> anyhow::Result<(String, tokio::time::Instant, oneshot::Receiver<Device>)> {
        let mut state = locked(&self.state);
        ensure!(
            !state.devices.contains_key(&id),
            "device key already enrolled"
        );
        let offer = state
            .offer
            .as_mut()
            .filter(|offer| {
                offer.expires > tokio::time::Instant::now() && offer.candidate.is_none()
            })
            .context("pairing code expired or already used")?;
        ensure!(
            bool::from(offer.secret.as_bytes().ct_eq(secret.as_bytes())),
            "invalid pairing code"
        );
        let comparison = comparison_code(secret, &id);
        let (accepted, result) = oneshot::channel();
        offer.candidate = Some(Candidate {
            id,
            name,
            comparison: comparison.clone(),
            accepted,
        });
        Ok((comparison, offer.expires, result))
    }

    fn audit(&self, device: &str, action: &str, session: Option<&str>) -> anyhow::Result<()> {
        use std::io::Write as _;
        let _lock = locked(&self.audit_lock);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.dir.join("audit.jsonl"))?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.mode() & 0o077 == 0,
            "remote audit file must be private"
        );
        let mut line = serde_json::to_vec(&serde_json::json!({
            "at": chrono::Utc::now(), "device": device, "action": action, "session": session
        }))?;
        line.push(b'\n');
        file.write_all(&line)?;
        Ok(())
    }

    async fn revoked(&self, id: &str, grant: &str, mut change: watch::Receiver<()>) {
        loop {
            let Some(device) = self.device(id).filter(|device| device.grant == grant) else {
                return;
            };
            let left = std::time::Duration::from_secs(
                u64::try_from(device.expires.saturating_sub(now())).unwrap_or_default(),
            );
            tokio::select! {
                _ = tokio::time::sleep(left) => {}
                result = change.changed() => if result.is_err() { return }
            }
        }
    }
}

/// Bounded NDJSON handshake followed by an existing PTY wire stream.
/// Unread bytes stay in the buffered stream when control switches to PTY.
type Pipe<S> = BufReader<S>;

async fn read_message<T: DeserializeOwned, S: AsyncRead + Unpin>(
    stream: &mut Pipe<S>,
    limit: usize,
) -> anyhow::Result<T> {
    let mut bytes = Vec::new();
    let n = stream
        .take(u64::try_from(limit)? + 1)
        .read_until(b'\n', &mut bytes)
        .await?;
    ensure!(
        n <= limit && bytes.last() == Some(&b'\n'),
        "invalid remote message framing"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

async fn send<T: Serialize, S: AsyncWrite + Unpin>(
    stream: &mut S,
    value: &T,
) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() < MAX_REPLY, "remote message too large");
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

pub(crate) fn names(paths: &Paths) -> Vec<Name> {
    std::fs::read_dir(state_dir(paths).join("machines"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|item| {
            let name = item.file_name();
            Name::new(name.to_str()?.strip_suffix(".json")?).ok()
        })
        .collect()
}

#[derive(Clone)]
pub struct Client {
    machine: Machine,
    config: Arc<rustls::ClientConfig>,
}

impl Client {
    pub fn open(paths: &Paths, name: &Name) -> anyhow::Result<Self> {
        let dir = state_dir(paths);
        let machine: Machine = read(&dir.join("machines").join(format!("{name}.json")))
            .with_context(|| format!("paired machine {name}"))?;
        let config = tls::client(&dir, &machine)?;
        Ok(Self { machine, config })
    }

    async fn call(
        &self,
        call: Call,
        pairing: bool,
    ) -> anyhow::Result<(Pipe<tokio_rustls::client::TlsStream<TcpStream>>, Reply)> {
        tokio::time::timeout(HANDSHAKE, async {
            let socket = TcpStream::connect(self.machine.at).await?;
            socket.set_nodelay(true)?;
            let server = if pairing { tls::PAIR } else { tls::HOST };
            let tls = tokio_rustls::TlsConnector::from(self.config.clone())
                .connect(rustls::pki_types::ServerName::try_from(server)?, socket)
                .await?;
            let expected = URL_SAFE_NO_PAD.decode(&self.machine.host)?;
            ensure!(
                tls.get_ref()
                    .1
                    .peer_certificates()
                    .and_then(|chain| chain.first())
                    .is_some_and(|cert| cert.as_ref() == expected),
                "remote host key changed"
            );
            let mut stream = Pipe::new(tls);
            send(
                &mut stream,
                &Envelope {
                    version: VERSION,
                    call,
                },
            )
            .await?;
            match read_message(&mut stream, MAX_REPLY).await? {
                Reply::Refused { message } => bail!("{message}"),
                reply => Ok((stream, reply)),
            }
        })
        .await
        .context("remote machine connection timed out")?
    }

    pub async fn list(&self) -> anyhow::Result<Vec<Row>> {
        match self.call(Call::List {}, false).await?.1 {
            Reply::Rows { rows } => Ok(rows),
            _ => bail!("unexpected remote reply"),
        }
    }

    pub async fn stream(
        &self,
        session: String,
        attach: bool,
    ) -> anyhow::Result<impl AsyncRead + AsyncWrite + Send + Unpin + 'static> {
        let call = if attach {
            Call::Attach { session }
        } else {
            Call::Watch { session }
        };
        let (stream, reply) = self.call(call, false).await?;
        ensure!(matches!(reply, Reply::Stream), "unexpected remote reply");
        Ok(stream)
    }

    pub(crate) async fn pair(
        paths: &Paths,
        machine_name: &Name,
        device: Name,
        code: &str,
    ) -> anyhow::Result<Device> {
        ensure!(code.len() <= 4096, "oversized pairing code");
        let offer: OfferCode = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(code.trim())?)?;
        ensure!(offer.v == VERSION, "unsupported pairing version");
        if std::env::var_os(crate::testenv::CONFINE_ENV).is_some() {
            ensure!(
                offer.at.ip().is_loopback() && crate::testenv::confined(paths.home()),
                "test pairing must be isolated on loopback"
            );
        }
        paths.ensure_home()?;
        let dir = state_dir(paths);
        create_private_dir(&dir)?;
        create_private_dir(&dir.join("machines"))?;
        let path = dir.join("machines").join(format!("{machine_name}.json"));
        ensure!(!path.exists(), "machine name already paired");
        let machine = Machine {
            at: offer.at,
            host: offer.host,
            identity: Identity::create(&dir)?,
        };
        let result = async {
            let config = tls::client(&dir, &machine)?;
            let client = Client {
                machine: machine.clone(),
                config,
            };
            let (mut stream, reply) = client
                .call(
                    Call::Enroll {
                        secret: offer.secret.clone(),
                        name: device,
                    },
                    true,
                )
                .await?;
            let Reply::Compare { code } = reply else {
                bail!("unexpected pairing reply")
            };
            let own = fingerprint(&URL_SAFE_NO_PAD.decode(&machine.identity.cert)?);
            ensure!(
                code == comparison_code(&offer.secret, &own),
                "pairing comparison did not match this device"
            );
            println!("Comparison code {code}. Confirm it on the target machine.");
            std::io::Write::flush(&mut std::io::stdout())?;
            let reply =
                tokio::time::timeout(OFFER_TTL, read_message::<Reply, _>(&mut stream, MAX_REPLY))
                    .await
                    .context("pairing expired")??;
            let Reply::Enrolled { device } = reply else {
                bail!("pairing was declined or expired")
            };
            save_new(&path, &machine)?;
            Ok(device)
        }
        .await;
        if result.is_err() {
            machine.identity.remove(&dir);
        }
        result
    }
}

impl Service {
    pub(crate) async fn serve(
        self: Arc<Self>,
        listener: TcpListener,
        front: Arc<crate::daemon::Front>,
        mut shutdown: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        let ordinary = tls::server(&self, false)?;
        let pairing = tls::server(&self, true)?;
        let slots = Arc::new(tokio::sync::Semaphore::new(64));
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                _ = connections.join_next(), if !connections.is_empty() => {}
                incoming = listener.accept() => {
                    let (socket, _) = incoming?;
                    let Ok(slot) = slots.clone().try_acquire_owned() else { continue };
                    let service = self.clone();
                    let front = front.clone();
                    let ordinary = ordinary.clone();
                    let pairing = pairing.clone();
                    connections.spawn(async move {
                        let _slot = slot;
                        if let Err(error) = service.connection(socket, &front, ordinary, pairing).await {
                            tracing::debug!(%error, "remote connection ended");
                        }
                    });
                }
            }
        }
        connections.shutdown().await;
        Ok(())
    }

    async fn connection(
        &self,
        socket: TcpStream,
        front: &crate::daemon::Front,
        ordinary: Arc<rustls::ServerConfig>,
        enrollment: Arc<rustls::ServerConfig>,
    ) -> anyhow::Result<()> {
        socket.set_nodelay(true)?;
        let (pairing, mut wire, id, request) = tokio::time::timeout(HANDSHAKE, async {
            let hello =
                tokio_rustls::LazyConfigAcceptor::new(rustls::server::Acceptor::default(), socket)
                    .await?;
            let pairing = match hello.client_hello().server_name() {
                Some(tls::PAIR) => true,
                Some(tls::HOST) => false,
                _ => bail!("unknown remote service"),
            };
            let transport = hello
                .into_stream(if pairing { enrollment } else { ordinary })
                .await?;
            let cert = transport
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|certs| certs.first())
                .context("missing device certificate")?;
            let id = fingerprint(cert);
            self.audit(
                &id,
                if pairing {
                    "enrollment_connect"
                } else {
                    "connect"
                },
                None,
            )?;
            let mut wire = Pipe::new(transport);
            let request = read_message::<Envelope, _>(&mut wire, MAX_REQUEST).await?;
            ensure!(
                request.version == VERSION,
                "unknown remote protocol version"
            );
            Ok::<_, anyhow::Error>((pairing, wire, id, request.call))
        })
        .await
        .context("remote handshake timed out")??;
        if pairing {
            let Call::Enroll { secret, name } = request else {
                bail!("enrollment connection only")
            };
            match self.enroll(id, &secret, name) {
                Ok((code, deadline, accepted)) => {
                    tokio::time::timeout(HANDSHAKE, send(&mut wire, &Reply::Compare { code }))
                        .await??;
                    tokio::select! {
                        _ = tokio::time::sleep_until(deadline) => {}
                        _ = wire.read_u8() => {}
                        device = accepted => {
                            if let Ok(device) = device {
                                tokio::time::timeout(HANDSHAKE, send(&mut wire, &Reply::Enrolled { device })).await??;
                            }
                        }
                    }
                }
                Err(error) => {
                    tokio::time::timeout(
                        HANDSHAKE,
                        send(
                            &mut wire,
                            &Reply::Refused {
                                message: error.to_string(),
                            },
                        ),
                    )
                    .await??;
                }
            }
            return Ok(());
        }
        let changes = self.changes.subscribe();
        let device = self.device(&id).context("device expired or revoked")?;
        let grant = device.grant.clone();
        tokio::select! {
            biased;
            _ = self.revoked(&id, &grant, changes) => Ok(()),
            result = self.action(wire, front, device, request) => result,
        }
    }

    async fn action(
        &self,
        mut wire: Pipe<tokio_rustls::server::TlsStream<TcpStream>>,
        front: &crate::daemon::Front,
        device: Device,
        request: Call,
    ) -> anyhow::Result<()> {
        async fn reply(wire: &mut (impl AsyncWrite + Unpin), value: Reply) -> anyhow::Result<()> {
            tokio::time::timeout(HANDSHAKE, send(wire, &value)).await?
        }

        let stream = match request {
            Call::List {} => {
                self.audit(&device.id, "list", None)?;
                let rows = front.remote_sessions().await?;
                return reply(&mut wire, Reply::Rows { rows }).await;
            }
            Call::Watch { session } => (device.scopes.watch, false, session),
            Call::Attach { session } => (device.scopes.attach, true, session),
            Call::Enroll { .. } => bail!("enrollment endpoint required"),
        };
        let (allowed, attach, session) = stream;
        let action = if attach { "attach" } else { "watch" };
        if !allowed {
            self.audit(
                &device.id,
                if attach {
                    "attach_denied"
                } else {
                    "watch_denied"
                },
                None,
            )?;
            return reply(
                &mut wire,
                Reply::Refused {
                    message: format!("device has no {action} grant"),
                },
            )
            .await;
        }
        let path = match front.remote_terminal(&session, attach).await {
            Ok(path) => path,
            Err(_) => {
                self.audit(
                    &device.id,
                    if attach {
                        "attach_unavailable"
                    } else {
                        "watch_unavailable"
                    },
                    Some(&session),
                )?;
                return reply(
                    &mut wire,
                    Reply::Refused {
                        message: "no running hosted terminal for that session".into(),
                    },
                )
                .await;
            }
        };
        self.audit(&device.id, action, Some(&session))?;
        let mut host =
            tokio::time::timeout(HANDSHAKE, tokio::net::UnixStream::connect(path)).await??;
        reply(&mut wire, Reply::Stream).await?;
        if attach {
            // The host grants one interactive viewer across local and
            // remote connections. End both sides when either disconnects.
            let (mut incoming, mut outgoing) = tokio::io::split(wire);
            let (mut display, mut input) = host.split();
            tokio::select! {
                result = tokio::io::copy(&mut incoming, &mut input) => { result?; }
                result = tokio::io::copy(&mut display, &mut outgoing) => { result?; }
            }
        } else {
            // A remote watcher may read terminal bytes but cannot fit the
            // PTY or send input. Any attempted input closes the watch.
            let (mut incoming, mut outgoing) = tokio::io::split(wire);
            tokio::select! {
                _ = incoming.read_u8() => {}
                result = tokio::io::copy(&mut host, &mut outgoing) => { result?; }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_has_no_actor_paths_or_disclosure_operations() {
        for request in [
            r#"{"version":1,"call":{"op":"list","path":"/tmp/socket"}}"#,
            r#"{"version":1,"call":{"op":"attach","session":"abc","actor":"person"}}"#,
            r#"{"version":1,"call":{"op":"watch","session":"abc","selector":{"pid":1}}}"#,
            r#"{"version":1,"call":{"op":"timeline","session":"abc"}}"#,
            r#"{"version":1,"call":{"op":"approve","session":"abc"}}"#,
            r#"{"version":1,"call":{"op":"stop","session":"abc"}}"#,
        ] {
            assert!(
                serde_json::from_str::<Envelope>(request).is_err(),
                "{request}"
            );
        }
        assert!(serde_json::from_str::<Envelope>(r#"{"version":1,"call":{"op":"list"}}"#).is_ok());
    }
}
