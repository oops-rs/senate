//! Versioned direct machine access. Local IPC controls enrollment. The
//! remote transport accepts only this module's wire types.
mod audit;
pub(crate) mod authority;
mod limits;
mod terminal;
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

const VERSION: u8 = 4;
use authority::MAX_VALID_SECS;
const HANDSHAKE: Duration = Duration::from_secs(6);
const OFFER_TTL: Duration = Duration::from_secs(authority::OFFER_SECONDS);
const MAX_REQUEST: usize = 4096;
const MAX_REPLY: usize = 1024 * 1024;

pub use senate_remote_admin::{Device, Scopes};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Control {
    PrepareEnable { listen: SocketAddr },
    Pair { scopes: Scopes, valid_secs: u64 },
    Candidate,
    PrepareEnroll,
    Devices,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct CandidateDetails {
    pub(crate) name: String,
    pub(crate) key_prefix: String,
    pub(crate) scopes: Scopes,
    pub(crate) valid_secs: u64,
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
    Compare { device: Device, proof: String },
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

fn fingerprint(cert: &rustls::pki_types::CertificateDer<'_>) -> Result<String, rustls::Error> {
    let key = rustls::server::ParsedCertificate::try_from(cert)?.subject_public_key_info();
    Ok(hex::encode(Sha256::digest(key.as_ref())))
}

fn comparison_context(
    secret: &str,
    at: SocketAddr,
    host: &str,
    device: &Device,
) -> anyhow::Result<Vec<u8>> {
    Ok(serde_json::to_vec(&(
        VERSION,
        secret,
        at,
        authority::digest(host, device)?,
    ))?)
}

fn binding(exporter: [u8; 32]) -> String {
    hex::encode(Sha256::digest(exporter))
}

fn verify_enrollment_proof(
    device: &Device,
    proof: &str,
    expected: &str,
    wall: i64,
) -> anyhow::Result<()> {
    ensure!(
        device.expires > wall && device.expires <= wall + MAX_VALID_SECS as i64,
        "enrollment expiry is out of bounds"
    );
    ensure!(
        bool::from(proof.as_bytes().ct_eq(expected.as_bytes())),
        "enrollment TLS binding did not match this connection"
    );
    Ok(())
}

fn verify_enrollment_identity(proposed: &Device, id: &str, name: &Name) -> anyhow::Result<()> {
    ensure!(
        proposed.id == id && proposed.name == name.as_str(),
        "enrollment identity changed"
    );
    Ok(())
}

fn approved_candidate(candidate: &Candidate, devices: &BTreeMap<String, Device>) -> Option<Device> {
    (candidate.prepared && devices.get(&candidate.device.id) == Some(&candidate.device))
        .then(|| candidate.device.clone())
}

async fn offer_expired((deadline, wall): (tokio::time::Instant, i64)) {
    while tokio::time::Instant::now() < deadline && now() < wall {
        tokio::time::sleep_until(
            deadline.min(tokio::time::Instant::now() + Duration::from_secs(1)),
        )
        .await;
    }
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

fn test_root(paths: &Paths) -> anyhow::Result<Option<PathBuf>> {
    test_root_for(
        paths,
        std::env::var_os(crate::testenv::CONFINE_ENV),
        cfg!(debug_assertions),
    )
}

fn test_root_for(
    paths: &Paths,
    root: Option<std::ffi::OsString>,
    debug_build: bool,
) -> anyhow::Result<Option<PathBuf>> {
    let Some(root) = root else { return Ok(None) };
    ensure!(
        debug_build,
        "remote test mode is disabled in release builds"
    );
    let root = PathBuf::from(root).canonicalize()?;
    let tmp = Path::new("/tmp").canonicalize()?;
    let metadata = root.metadata()?;
    ensure!(
        root.parent() == Some(tmp.as_path())
            && metadata.is_dir()
            && metadata.uid() == crate::service::current_uid()
            && metadata.mode() & 0o077 == 0
            && crate::testenv::confined(paths.home()),
        "remote test requires a private /tmp home and loopback"
    );
    Ok(Some(root))
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

fn save_new<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    durable(path, value)?.persist_noclobber(path)?;
    Ok(())
}

fn locked<T>(state: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    state.lock().unwrap_or_else(|error| error.into_inner())
}

impl Identity {
    fn create(dir: &Path, _isolated_test: bool) -> anyhow::Result<Self> {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec![tls::HOST.into(), tls::PAIR.into()])?;
        let id = uuid::Uuid::new_v4().to_string();
        let secret = signing_key.serialize_der();
        #[cfg(target_os = "macos")]
        if !_isolated_test {
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
    #[serde(default)]
    experimental: bool,
}

#[derive(Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    remote: Config,
}

fn read_config(paths: &Paths) -> anyhow::Result<Config> {
    match std::fs::read_to_string(paths.config()) {
        Ok(text) => Ok(toml::from_str::<ConfigFile>(&text)
            .map_err(|_| anyhow!("invalid [remote] configuration"))?
            .remote),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn experimental_notice(paths: &Paths) -> Option<&'static str> {
    let config = match read_config(paths) {
        Ok(config) => config,
        Err(_) => {
            return Some("Remote access is disabled because its configuration could not be read.");
        }
    };
    if !config.experimental {
        return config.listen.map(
            |_| "Remote access is experimental and disabled without [remote] experimental = true.",
        );
    }
    Some(match authority_for(paths).and_then(|store| store.read()) {
        Ok(profile) if profile.policy.is_some() => {
            "Remote access is experimental. An administrator policy is installed."
        }
        Ok(_) => "Remote access is experimental and disabled until an administrator enables it.",
        Err(_) => "Remote access is disabled because its administrator policy could not be read.",
    })
}

fn authority_for(paths: &Paths) -> anyhow::Result<authority::Store> {
    Ok(match test_root(paths)? {
        Some(root) => authority::Store::isolated(&root),
        None => authority::Store::system(crate::service::current_uid()),
    })
}

#[doc(hidden)]
pub fn authorize_isolated_test(
    paths: &Paths,
    args: &[String],
    answer: &str,
) -> anyhow::Result<String> {
    let root = test_root(paths)?.context("missing remote test confinement")?;
    authority::test_authorize(authority::Store::isolated(&root), args, answer)
}

fn host_identity(dir: &Path, isolated: bool) -> anyhow::Result<Identity> {
    create_private_dir(dir)?;
    let path = dir.join("host.json");
    if path.exists() {
        return read(&path);
    }
    let identity = Identity::create(dir, isolated)?;
    if let Err(error) = save_new(&path, &identity) {
        identity.remove(dir);
        return Err(error);
    }
    Ok(identity)
}

pub(crate) fn prepare_enable(paths: &Paths, at: SocketAddr) -> anyhow::Result<String> {
    let isolated = test_root(paths)?.is_some();
    ensure!(
        !at.ip().is_unspecified()
            && !at.ip().is_multicast()
            && if isolated {
                at.ip().is_loopback()
            } else {
                at.port() > 0
            },
        "remote listen needs an exact unicast address and nonzero port"
    );
    let identity = host_identity(&state_dir(paths), isolated)?;
    let host_pin = fingerprint(&URL_SAFE_NO_PAD.decode(&identity.cert)?.into())?;
    authority::encode(authority::Listener { at, host_pin })
}

struct Offer {
    secret: String,
    scopes: Scopes,
    valid_secs: u64,
    expires: tokio::time::Instant,
    expires_wall: i64,
    candidate: Option<Candidate>,
}

impl Offer {
    fn active(&self) -> bool {
        self.expires > tokio::time::Instant::now() && self.expires_wall > now()
    }
}

struct Candidate {
    device: Device,
    peer: SocketAddr,
    prepared: bool,
    accepted: oneshot::Sender<Device>,
}

struct PendingEnrollment {
    reply: Reply,
    deadline: (tokio::time::Instant, i64),
    accepted: oneshot::Receiver<Device>,
}

#[derive(Default)]
struct State {
    offer: Option<Offer>,
    devices: BTreeMap<String, Device>,
    generation: u64,
    enabled: bool,
}

pub(crate) struct Service {
    dir: PathBuf,
    at: SocketAddr,
    identity: Identity,
    authority: authority::Store,
    policy: authority::Policy,
    state: Mutex<State>,
    changes: watch::Sender<()>,
    audit: audit::Audit,
}

async fn accept_remote(listener: Option<&TcpListener>) -> std::io::Result<(TcpStream, SocketAddr)> {
    if let Some(listener) = listener {
        listener.accept().await
    } else {
        std::future::pending().await
    }
}

impl Service {
    pub(crate) async fn open(paths: &Paths) -> anyhow::Result<Option<(Arc<Self>, TcpListener)>> {
        let config = read_config(paths)?;
        if !config.experimental {
            if config.listen.is_some() {
                tracing::warn!(
                    "remote access is experimental; listener disabled without [remote] experimental = true"
                );
            }
            return Ok(None);
        }
        let authority = authority_for(paths)?;
        let profile = authority.read()?;
        let Some(policy) = profile.policy.clone() else {
            tracing::warn!("remote listener disabled; no administrator policy is installed");
            return Ok(None);
        };
        let generation = profile.generation;
        let devices = profile.live_devices(now());
        let at = policy.listener.at;
        let dir = state_dir(paths);
        let identity: Identity = read(&dir.join("host.json"))
            .context("prepare the approved host identity as the daemon user")?;
        identity.secret(&dir)?;
        let host_pin = fingerprint(&URL_SAFE_NO_PAD.decode(&identity.cert)?.into())?;
        ensure!(
            host_pin == policy.listener.host_pin,
            "local host key differs from the administrator policy"
        );
        tracing::warn!(
            "remote access is experimental; starting the administrator-approved listener"
        );
        let listener = TcpListener::bind(at)
            .await
            .context("bind the configured remote address")?;
        let audit = audit::Audit::open(&dir)?;
        let service = Arc::new(Self {
            dir,
            at: listener.local_addr()?,
            identity,
            authority,
            policy,
            state: Mutex::new(State {
                devices,
                offer: None,
                generation,
                enabled: true,
            }),
            changes: watch::channel(()).0,
            audit,
        });
        Ok(Some((service, listener)))
    }

    fn device(&self, id: &str) -> Option<Device> {
        let state = locked(&self.state);
        if !state.enabled {
            return None;
        }
        state.devices.get(id).filter(|d| d.expires > now()).cloned()
    }

    pub(crate) fn control(&self, request: Control) -> anyhow::Result<serde_json::Value> {
        let mut state = locked(&self.state);
        ensure!(state.enabled, "remote authorization is disabled");
        match request {
            Control::PrepareEnable { .. } => bail!("prepare the listener through the local bridge"),
            Control::Pair { scopes, valid_secs } => {
                ensure!(
                    (1..=MAX_VALID_SECS).contains(&valid_secs),
                    "device validity must be 1 second to 30 days"
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
                    expires_wall: now() + OFFER_TTL.as_secs() as i64,
                    candidate: None,
                });
                Ok(serde_json::json!(
                    URL_SAFE_NO_PAD.encode(serde_json::to_vec(&code)?)
                ))
            }
            Control::Candidate => {
                let offer = state
                    .offer
                    .as_ref()
                    .filter(|offer| offer.active())
                    .context("no active pairing")?;
                let details = offer
                    .candidate
                    .as_ref()
                    .filter(|candidate| !candidate.accepted.is_closed())
                    .map(|candidate| CandidateDetails {
                        name: candidate.device.name.clone(),
                        key_prefix: candidate.device.id.chars().take(16).collect(),
                        scopes: offer.scopes,
                        valid_secs: offer.valid_secs,
                    });
                Ok(serde_json::to_value(details)?)
            }
            Control::PrepareEnroll => {
                let offer = state
                    .offer
                    .as_mut()
                    .filter(|offer| offer.active())
                    .context("no active pairing")?;
                let candidate = offer
                    .candidate
                    .as_mut()
                    .filter(|candidate| !candidate.accepted.is_closed())
                    .context("the client has not requested pairing")?;
                let request = authority::encode(authority::Enrollment {
                    policy: self.policy.clone(),
                    device: candidate.device.clone(),
                })?;
                candidate.prepared = true;
                Ok(serde_json::Value::String(request))
            }
            Control::Devices => Ok(serde_json::to_value(
                state.devices.values().collect::<Vec<_>>(),
            )?),
        }
    }

    fn deactivate(&self) {
        let mut state = locked(&self.state);
        if state.enabled {
            state.enabled = false;
            state.offer = None;
            state.devices.clear();
            self.changes.send_replace(());
        }
    }

    fn refresh_authority(&self) -> anyhow::Result<()> {
        let profile = self.authority.read()?;
        let mut state = locked(&self.state);
        let previous_generation = state.generation;
        ensure!(
            profile.generation >= previous_generation,
            "remote administrator profile rolled back"
        );
        // Even an explicit disable advances the floor. An older active
        // snapshot must not reopen the listener after the disable was seen.
        state.generation = profile.generation;
        ensure!(
            profile.policy.as_ref() == Some(&self.policy),
            "remote administrator policy removed or changed"
        );
        if state.enabled && profile.generation == previous_generation {
            return Ok(());
        }
        state.enabled = true;
        let devices = profile.live_devices(now());
        for (id, previous) in &state.devices {
            if !devices.contains_key(id) {
                self.audit.record(previous, "revoke", None, None);
            }
        }
        state.devices = devices;
        self.changes.send_replace(());
        let accepted = state
            .offer
            .as_ref()
            .filter(|offer| offer.active())
            .and_then(|offer| offer.candidate.as_ref())
            .and_then(|candidate| approved_candidate(candidate, &state.devices));
        if let Some(device) = accepted
            && let Some(candidate) = state.offer.take().and_then(|offer| offer.candidate)
        {
            self.audit
                .record(&device, "pair", Some(candidate.peer), None);
            let _ = candidate.accepted.send(device);
        }
        Ok(())
    }

    fn enroll(
        &self,
        id: String,
        secret: &str,
        name: Name,
        peer: SocketAddr,
        tls: &rustls::ServerConnection,
    ) -> anyhow::Result<PendingEnrollment> {
        let mut state = locked(&self.state);
        ensure!(!state.devices.contains_key(&id), "pairing unavailable");
        let offer = state
            .offer
            .as_mut()
            .filter(|offer| offer.active() && offer.candidate.is_none())
            .context("pairing unavailable")?;
        ensure!(
            bool::from(offer.secret.as_bytes().ct_eq(secret.as_bytes())),
            "pairing unavailable"
        );
        let device = Device {
            id,
            name: name.to_string(),
            scopes: offer.scopes,
            expires: now() + offer.valid_secs as i64,
            deadline: offer.expires_wall,
        };
        let context = comparison_context(secret, self.at, &self.policy.listener.host_pin, &device)?;
        let proof = binding(tls.export_keying_material(
            [0; 32],
            b"EXPORTER-Senate-Pair-v4",
            Some(&context),
        )?);
        let (accepted, result) = oneshot::channel();
        offer.candidate = Some(Candidate {
            device: device.clone(),
            peer,
            prepared: false,
            accepted,
        });
        Ok(PendingEnrollment {
            reply: Reply::Compare { device, proof },
            deadline: (offer.expires, offer.expires_wall),
            accepted: result,
        })
    }

    async fn revoked(&self, approved: &Device, mut change: watch::Receiver<()>) {
        loop {
            let Some(device) = self
                .device(&approved.id)
                .filter(|device| device == approved)
            else {
                return;
            };
            let left = std::time::Duration::from_secs(
                u64::try_from(device.expires.saturating_sub(now())).unwrap_or_default(),
            );
            tokio::select! {
                _ = tokio::time::sleep(left.min(Duration::from_secs(1))) => {}
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
                Reply::Refused { message } => {
                    let display: String = message
                        .chars()
                        .filter(|ch| !authority::unsafe_display(*ch))
                        .take(256)
                        .collect();
                    bail!("remote refused: {display}");
                }
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
        let isolated_test = test_root(paths)?.is_some();
        ensure!(
            !isolated_test || offer.at.ip().is_loopback(),
            "remote test pairing must stay on loopback"
        );
        paths.ensure_home()?;
        let dir = state_dir(paths);
        create_private_dir(&dir)?;
        create_private_dir(&dir.join("machines"))?;
        let path = dir.join("machines").join(format!("{machine_name}.json"));
        ensure!(!path.exists(), "machine name already paired");
        let machine = Machine {
            at: offer.at,
            host: offer.host,
            identity: Identity::create(&dir, isolated_test)?,
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
                        name: device.clone(),
                    },
                    true,
                )
                .await?;
            let Reply::Compare {
                device: proposed,
                proof,
            } = reply
            else {
                bail!("unexpected pairing reply")
            };
            let own = fingerprint(&URL_SAFE_NO_PAD.decode(&machine.identity.cert)?.into())?;
            let host = fingerprint(&URL_SAFE_NO_PAD.decode(&machine.host)?.into())?;
            verify_enrollment_identity(&proposed, &own, &device)?;
            let context = comparison_context(&offer.secret, machine.at, &host, &proposed)?;
            let expected = binding(stream.get_ref().get_ref().1.export_keying_material(
                [0; 32],
                b"EXPORTER-Senate-Pair-v4",
                Some(&context),
            )?);
            verify_enrollment_proof(&proposed, &proof, &expected, now())?;
            let code = authority::grouped(&authority::comparison(&host, &proposed)?);
            let expiry = chrono::DateTime::from_timestamp(proposed.expires, 0)
                .context("invalid grant expiry")?;
            println!("Comparison code {code}. Confirm it on the target machine.");
            println!(
                "Candidate {} · key {} · list, watch={}, attach={} · expires {expiry}",
                proposed.name,
                &own[..16],
                proposed.scopes.watch,
                proposed.scopes.attach
            );
            std::io::Write::flush(&mut std::io::stdout())?;
            let reply =
                tokio::time::timeout(OFFER_TTL, read_message::<Reply, _>(&mut stream, MAX_REPLY))
                    .await
                    .context("pairing expired")??;
            let Reply::Enrolled { device } = reply else {
                bail!("pairing was declined or expired")
            };
            ensure!(
                device == proposed && device.expires > now(),
                "the enrollment grant does not match this device and connection"
            );
            // Before committing the local machine, verify that the enrolled
            // key can actually authenticate on the operational endpoint.
            client
                .list()
                .await
                .context("the paired endpoint did not accept the enrolled key")?;
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
        let limits = Arc::new(limits::Limits::default());
        let mut backoff = limits::AcceptBackoff::default();
        let mut connections = tokio::task::JoinSet::new();
        let mut authority_refresh = tokio::time::interval(Duration::from_millis(250));
        authority_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut listener = Some(listener);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                _ = authority_refresh.tick() => match self.refresh_authority() {
                    Ok(()) if listener.is_none() => {
                        match TcpListener::bind(self.at).await {
                            Ok(bound) => {
                                listener = Some(bound);
                                tracing::info!("remote administrator policy recovered; listening again");
                            }
                            Err(_) => self.deactivate(),
                        }
                    }
                    Ok(()) => {}
                    Err(error) => {
                        self.deactivate();
                        if listener.take().is_some() {
                            tracing::warn!(%error, "remote listener paused; retrying administrator policy");
                            connections.shutdown().await;
                        }
                    }
                },
                _ = connections.join_next(), if !connections.is_empty() => {}
                _ = tokio::time::sleep_until(backoff.until()), if backoff.until() > tokio::time::Instant::now() => {}
                incoming = accept_remote(listener.as_ref()), if backoff.until() <= tokio::time::Instant::now() => {
                    let Some((socket, peer)) = backoff.accepted(incoming) else { continue };
                    let active_offer = locked(&self.state).offer.as_ref()
                        .is_some_and(|offer| offer.active() && offer.candidate.is_none());
                    let Some(admission) = limits.handshake(peer.ip(), active_offer) else { continue };
                    let limits = limits.clone();
                    let service = self.clone();
                    let front = front.clone();
                    let ordinary = ordinary.clone();
                    let pairing = pairing.clone();
                    connections.spawn(async move {
                        if service.connection((socket, peer), &front, (ordinary, pairing), (&limits, admission)).await.is_err() {
                            tracing::debug!("remote connection rejected or ended with a transport error");
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
        (socket, peer): (TcpStream, SocketAddr),
        front: &crate::daemon::Front,
        (ordinary, enrollment): (Arc<rustls::ServerConfig>, Arc<rustls::ServerConfig>),
        (limits, admission): (&limits::Limits, limits::Handshake),
    ) -> anyhow::Result<()> {
        socket.set_nodelay(true)?;
        let pairing_only = admission.pairing_only();
        let timeout = if pairing_only {
            Duration::from_secs(1)
        } else {
            HANDSHAKE
        };
        let (pairing, mut wire, id, request) = tokio::time::timeout(timeout, async {
            let hello =
                tokio_rustls::LazyConfigAcceptor::new(rustls::server::Acceptor::default(), socket)
                    .await?;
            let pairing = tls::enrollment_name(hello.client_hello().server_name())?;
            ensure!(!pairing_only || pairing, "reserved for enrollment");
            let transport = hello
                .into_stream(if pairing { enrollment } else { ordinary })
                .await;
            if !pairing && transport.is_err() {
                self.audit.rejected(peer);
            }
            let transport = transport?;
            let cert = transport
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|certs| certs.first())
                .context("missing device certificate")?;
            let id = fingerprint(cert)?;
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
            match self.enroll(id, &secret, name, peer, wire.get_ref().get_ref().1) {
                Ok(PendingEnrollment {
                    reply,
                    deadline,
                    accepted,
                }) => {
                    drop(admission);
                    tokio::time::timeout(HANDSHAKE, send(&mut wire, &reply)).await??;
                    tokio::select! {
                        _ = offer_expired(deadline) => {}
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
        let _slot = limits
            .device(id.clone())
            .context("remote request budget exhausted")?;
        drop(admission);
        self.audit.record(&device, "connect", Some(peer), None);
        tokio::select! {
            biased;
            _ = self.revoked(&device, changes) => {},
            result = self.action(wire, front, &device, peer, request) => {
                self.audit.record(&device, "disconnect", Some(peer), None);
                return result;
            },
        }
        self.audit.record(&device, "disconnect", Some(peer), None);
        Ok(())
    }

    async fn action(
        &self,
        mut wire: Pipe<tokio_rustls::server::TlsStream<TcpStream>>,
        front: &crate::daemon::Front,
        device: &Device,
        peer: SocketAddr,
        request: Call,
    ) -> anyhow::Result<()> {
        async fn reply(wire: &mut (impl AsyncWrite + Unpin), value: Reply) -> anyhow::Result<()> {
            tokio::time::timeout(HANDSHAKE, send(wire, &value)).await?
        }

        let stream = match request {
            Call::List {} => {
                self.audit.record(device, "list", Some(peer), None);
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
            self.audit.record(
                device,
                if attach {
                    "attach_denied"
                } else {
                    "watch_denied"
                },
                Some(peer),
                None,
            );
            return reply(
                &mut wire,
                Reply::Refused {
                    message: format!("device has no {action} grant"),
                },
            )
            .await;
        }
        let host =
            match tokio::time::timeout(HANDSHAKE, front.remote_terminal(&session, attach)).await {
                Ok(Ok(host)) => host,
                _ => {
                    self.audit.record(
                        device,
                        if attach {
                            "attach_unavailable"
                        } else {
                            "watch_unavailable"
                        },
                        Some(peer),
                        None,
                    );
                    return reply(
                        &mut wire,
                        Reply::Refused {
                            message: "no running hosted terminal for that session".into(),
                        },
                    )
                    .await;
                }
            };
        self.audit
            .record(device, action, Some(peer), Some(&session));
        reply(&mut wire, Reply::Stream).await?;
        terminal::relay(wire, host, attach).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_checks_identity_and_daemon_requires_the_exact_pending_grant() -> anyhow::Result<()> {
        let device = Device {
            id: "a".repeat(64),
            name: "test-client".into(),
            scopes: Scopes {
                watch: true,
                attach: false,
            },
            expires: now() + 60,
            deadline: now() + 30,
        };
        let name: Name = "test-client".parse()?;
        verify_enrollment_identity(&device, &device.id, &name)?;
        assert!(verify_enrollment_identity(&device, &"b".repeat(64), &name).is_err());
        assert!(verify_enrollment_identity(&device, &device.id, &"other".parse()?).is_err());
        let candidate = Candidate {
            device: device.clone(),
            peer: "127.0.0.1:4000".parse()?,
            prepared: true,
            accepted: oneshot::channel().0,
        };
        let mut devices = BTreeMap::from([(device.id.clone(), device.clone())]);
        assert_eq!(
            approved_candidate(&candidate, &devices),
            Some(device.clone())
        );
        if let Some(altered) = devices.get_mut(&device.id) {
            altered.scopes.attach = true;
        }
        assert_eq!(approved_candidate(&candidate, &devices), None);
        Ok(())
    }

    #[test]
    fn client_rejects_a_mismatched_tls_exporter_proof() -> anyhow::Result<()> {
        let wall = now();
        let device = Device {
            id: "a".repeat(64),
            name: "test-client".into(),
            scopes: Scopes {
                watch: true,
                attach: false,
            },
            expires: wall + 60,
            deadline: wall + 30,
        };
        verify_enrollment_proof(&device, &"b".repeat(64), &"b".repeat(64), wall)?;
        assert!(verify_enrollment_proof(&device, &"b".repeat(64), &"c".repeat(64), wall).is_err());
        let context =
            comparison_context("offer", "127.0.0.1:4000".parse()?, &"d".repeat(64), &device)?;
        let mut altered = device;
        altered.scopes.attach = true;
        assert_ne!(
            context,
            comparison_context(
                "offer",
                "127.0.0.1:4000".parse()?,
                &"d".repeat(64),
                &altered
            )?
        );
        Ok(())
    }

    #[test]
    fn release_configuration_cannot_use_the_test_environment() -> anyhow::Result<()> {
        let paths = Paths::new("/tmp/isolated");
        assert!(test_root_for(&paths, Some("/tmp/isolated".into()), false).is_err());
        assert!(test_root_for(&paths, None, false)?.is_none());
        Ok(())
    }

    #[test]
    fn device_pin_is_stable_across_certificates_for_the_same_key() -> anyhow::Result<()> {
        let key = rcgen::KeyPair::generate()?;
        let mut params = rcgen::CertificateParams::new(vec![tls::PAIR.into()])?;
        params.serial_number = Some(1_u64.into());
        let first = params.self_signed(&key)?;
        params.serial_number = Some(2_u64.into());
        let second = params.self_signed(&key)?;
        let other = params.self_signed(&rcgen::KeyPair::generate()?)?;
        assert_ne!(first.der(), second.der());
        assert_eq!(fingerprint(first.der())?, fingerprint(second.der())?);
        assert_ne!(fingerprint(first.der())?, fingerprint(other.der())?);
        Ok(())
    }

    #[test]
    fn the_wire_has_no_actor_paths_or_disclosure_operations() {
        for request in [
            r#"{"version":4,"call":{"op":"list","path":"/tmp/socket"}}"#,
            r#"{"version":4,"call":{"op":"attach","session":"abc","actor":"person"}}"#,
            r#"{"version":4,"call":{"op":"watch","session":"abc","selector":{"pid":1}}}"#,
            r#"{"version":4,"call":{"op":"timeline","session":"abc"}}"#,
            r#"{"version":4,"call":{"op":"approve","session":"abc"}}"#,
            r#"{"version":4,"call":{"op":"stop","session":"abc"}}"#,
        ] {
            assert!(
                serde_json::from_str::<Envelope>(request).is_err(),
                "{request}"
            );
        }
        assert!(serde_json::from_str::<Envelope>(r#"{"version":4,"call":{"op":"list"}}"#).is_ok());
    }
}

#[cfg(test)]
mod experimental_tests {
    use super::*;

    #[tokio::test]
    async fn configured_listener_without_opt_in_leaves_no_remote_state() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let paths = Paths::new(dir.path());
        for opt_in in ["", "experimental = false\n"] {
            std::fs::write(
                paths.config(),
                format!("[remote]\nlisten = \"127.0.0.1:0\"\n{opt_in}"),
            )?;
            assert!(Service::open(&paths).await?.is_none());
            assert!(!state_dir(&paths).exists());
            assert!(experimental_notice(&paths).is_some_and(|note| note.contains("disabled")));
        }
        Ok(())
    }
}
