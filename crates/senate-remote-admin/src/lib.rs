//! Root-owned remote authorization. The bridge reads this store; only the
//! standalone administrator command changes it.

use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub const HELPER: &str = "/usr/local/libexec/senate-remote-admin";
pub const SYSTEM_DIR: &str = "/etc/senate/remote";
pub const SUDOERS_PATH: &str = "/etc/sudoers.d/99-senate-remote-admin";
pub const SUDOERS_RULE: &str =
    "Defaults!/usr/local/libexec/senate-remote-admin timestamp_timeout=0";
const VERSION: u8 = 3;
pub const REQUEST_LIMIT: usize = 2048;
pub const MAX_VALID_SECS: u64 = 30 * 86400;
pub const OFFER_SECONDS: u64 = 120;

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

pub fn unsafe_display(ch: char) -> bool {
    ch.is_control()
        || matches!(ch, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Canonical device names use the same ASCII slug grammar as Senate names.
pub fn canonical_name(name: &str) -> anyhow::Result<&str> {
    let bytes = name.as_bytes();
    ensure!(
        !bytes.is_empty()
            && bytes.len() <= 63
            && bytes[0].is_ascii_alphanumeric()
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-'),
        "invalid device name"
    );
    Ok(name)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Scopes {
    pub watch: bool,
    pub attach: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub scopes: Scopes,
    pub expires: i64,
    pub deadline: i64,
}

const STORE_LIMIT: u64 = 128 * 1024;
const MAX_ATTEMPTS: u8 = 3;
const FAILURE_LIMIT: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Listener {
    pub at: SocketAddr,
    pub host_pin: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub revision: u64,
    pub listener: Listener,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Enrollment {
    pub policy: Policy,
    pub device: Device,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Request<T> {
    version: u8,
    request: T,
}

pub fn encode<T: Serialize>(request: T) -> anyhow::Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&Request {
        version: VERSION,
        request,
    })?))
}

fn decode<T: DeserializeOwned>(input: &str) -> anyhow::Result<T> {
    ensure!(
        input.len() <= REQUEST_LIMIT,
        "oversized authorization request"
    );
    let value: Request<T> = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(input)?)
        .map_err(|_| anyhow::anyhow!("invalid authorization request"))?;
    ensure!(
        value.version == VERSION,
        "unsupported authorization request version"
    );
    Ok(value.request)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FailedConfirmation {
    device: String,
    attempts: u8,
    last_attempt: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct History {
    pub generation: u64,
    pub at: i64,
    pub action: String,
    pub subject: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    version: u8,
    pub generation: u64,
    pub policy: Option<Policy>,
    pub devices: BTreeMap<String, Device>,
    #[serde(default)]
    failures: BTreeMap<String, FailedConfirmation>,
    #[serde(default)]
    pub history: Vec<History>,
    #[serde(default)]
    approved: BTreeMap<String, i64>,
    #[serde(default)]
    revoked: BTreeMap<String, i64>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            version: VERSION,
            generation: 0,
            policy: None,
            devices: BTreeMap::new(),
            failures: BTreeMap::new(),
            history: Vec::new(),
            approved: BTreeMap::new(),
            revoked: BTreeMap::new(),
        }
    }
}

#[derive(Clone)]
pub struct Store {
    dir: PathBuf,
    uid: u32,
    owner: u32,
    isolated: bool,
    gid: Option<u32>,
}

impl Store {
    pub fn system(uid: u32) -> Self {
        Self {
            dir: SYSTEM_DIR.into(),
            uid,
            owner: 0,
            isolated: false,
            gid: None,
        }
    }

    #[doc(hidden)]
    pub fn isolated(root: &Path) -> Self {
        let uid = os::current_uid();
        Self {
            dir: root.join("authorization"),
            uid,
            owner: uid,
            isolated: true,
            gid: None,
        }
    }

    fn directory(&self, create: bool) -> anyhow::Result<PathBuf> {
        if self.isolated {
            if create {
                create_dir(&self.dir, 0o700)?;
            }
            verify_node(&fs::symlink_metadata(&self.dir)?, self.owner, true)?;
            return Ok(self.dir.clone());
        }
        let etc = Path::new("/etc").canonicalize()?;
        verify_system_chain(&etc, true)?;
        let mut path = etc;
        for part in ["senate", "remote"] {
            path.push(part);
            if create {
                create_dir(&path, 0o755)?;
            }
            verify_node(&fs::symlink_metadata(&path)?, 0, true)?;
        }
        Ok(path)
    }

    fn profile_path(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}.v3.json", self.uid))
    }

    pub fn read(&self) -> anyhow::Result<Profile> {
        let dir = match self.directory(false) {
            Ok(dir) => dir,
            Err(error) if missing(&error) => return Ok(Profile::default()),
            Err(error) => return Err(error),
        };
        self.read_at(&dir)
    }

    fn read_at(&self, dir: &Path) -> anyhow::Result<Profile> {
        let file = match OpenOptions::new()
            .read(true)
            .no_follow()
            .open(self.profile_path(dir))
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Profile::default()),
            Err(error) => return Err(error.into()),
        };
        let meta = file.metadata()?;
        verify_node(&meta, self.owner, false)?;
        ensure!(
            meta.len() <= STORE_LIMIT,
            "oversized remote authority store"
        );
        let value: Profile = serde_json::from_reader(file)?;
        value.validate(self.isolated)?;
        Ok(value)
    }

    fn transaction(&self, command: Command, tty: &mut impl Terminal) -> anyhow::Result<()> {
        let previous = self.read()?;
        let _ = previous.clone().apply(&command, self.isolated, now())?;
        let approved = authorize(&command, &previous, tty)?;
        if !approved && !matches!(command, Command::Enroll(_)) {
            bail!("authorization cancelled");
        }
        let dir = self.directory(true)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .private()
            .no_follow()
            .open(dir.join(format!("{}.lock", self.uid)))?;
        verify_node(&lock.metadata()?, self.owner, false)?;
        lock.lock()?;
        let current = self.read_at(&dir)?;
        ensure!(
            !approved || current.generation == previous.generation,
            "authorization changed; prepare the request again"
        );
        // Expiry and attempt budgets are rechecked after the terminal interaction.
        let updated = if let (false, Command::Enroll(enrollment)) = (approved, &command) {
            current.failed_confirmation(enrollment, now())?
        } else {
            current.apply(&command, self.isolated, now())?
        };
        let bytes = serde_json::to_vec(&updated)?;
        ensure!(
            bytes.len() as u64 <= STORE_LIMIT,
            "remote grant store is full"
        );
        let mut file = tempfile::NamedTempFile::new_in(&dir)?;
        if let Some(gid) = self.gid {
            os::set_group(file.as_file(), gid)?;
        }
        os::set_mode(file.as_file(), 0o640)?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        file.persist(self.profile_path(&dir))?;
        if matches!(command, Command::Enable(_)) {
            let legacy = dir.join(format!("{}.json", self.uid));
            if fs::symlink_metadata(&legacy)
                .is_ok_and(|meta| verify_node(&meta, self.owner, false).is_ok())
                && let Err(error) = fs::remove_file(legacy)
            {
                writeln!(tty, "Could not remove the old profile: {error}").ok();
            }
        }
        if let Err(error) = File::open(dir).and_then(|dir| dir.sync_all()) {
            writeln!(
                tty,
                "Authorization committed. Could not sync the directory: {error}"
            )
            .ok();
        }
        ensure!(
            approved,
            "comparison did not match; confirmation denied and recorded"
        );
        writeln!(tty, "Remote authorization updated.")?;
        Ok(())
    }
}

fn missing(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
}

fn create_dir(path: &Path, mode: u32) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    os::dir_mode(&mut builder, mode);
    match builder.create(path) {
        Ok(()) => os::set_path_mode(path, mode),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

fn verify_node(meta: &fs::Metadata, uid: u32, directory: bool) -> anyhow::Result<()> {
    ensure!(
        os::SUPPORTED,
        "remote authority needs a root-owned store and sudo, which this platform does not have"
    );
    ensure!(
        (if directory {
            meta.is_dir()
        } else {
            meta.is_file()
        }) && os::owner(meta) == Some(uid)
            && os::mode(meta) & 0o022 == 0,
        "remote authority requires trusted ownership and no group or other write access"
    );
    Ok(())
}

fn verify_system_chain(path: &Path, directory: bool) -> anyhow::Result<()> {
    ensure!(path.is_absolute(), "remote authority path must be absolute");
    verify_node(&fs::symlink_metadata(path)?, 0, directory)?;
    for parent in path.ancestors().skip(1) {
        verify_node(&fs::symlink_metadata(parent)?, 0, true)?;
    }
    Ok(())
}

pub fn pin(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}

fn listener_ok(listener: &Listener, isolated: bool) -> bool {
    let ip = listener.at.ip().to_canonical();
    !ip.is_unspecified()
        && !ip.is_multicast()
        && !matches!(ip, IpAddr::V4(ip) if ip.is_broadcast())
        && pin(&listener.host_pin)
        && if isolated {
            ip.is_loopback()
        } else {
            listener.at.port() > 0
        }
}

impl Profile {
    fn validate(&self, isolated: bool) -> anyhow::Result<()> {
        ensure!(
            self.version == VERSION,
            "unsupported remote authority version"
        );
        if let Some(policy) = &self.policy {
            ensure!(
                policy.revision > 0
                    && policy.revision <= self.generation
                    && listener_ok(&policy.listener, isolated),
                "invalid protected listener policy"
            );
        } else {
            ensure!(
                self.devices.is_empty(),
                "device grants without a protected listener policy"
            );
        }
        ensure!(
            self.devices.iter().all(|(id, entry)| id == &entry.id
                && pin(id)
                && canonical_name(&entry.name).is_ok()),
            "invalid protected device grant"
        );
        ensure!(
            self.failures.len() <= FAILURE_LIMIT
                && self.failures.iter().all(|(binding, failed)| pin(binding)
                    && pin(&failed.device)
                    && (1..=MAX_ATTEMPTS).contains(&failed.attempts)),
            "invalid administrator confirmation audit"
        );
        Ok(())
    }

    fn apply(mut self, command: &Command, isolated: bool, wall: i64) -> anyhow::Result<Self> {
        self.generation = self
            .generation
            .checked_add(1)
            .context("authorization generation exhausted")?;
        self.approved.retain(|_, expiry| *expiry > wall);
        self.revoked.retain(|_, expiry| *expiry > wall);
        let (action, subject) = match command {
            Command::Enable(listener) => {
                ensure!(
                    listener_ok(listener, isolated),
                    "listener needs an exact unicast address, nonzero port and host pin"
                );
                self.devices.clear();
                self.policy = Some(Policy {
                    revision: self.generation,
                    listener: listener.clone(),
                });
                ("enable", format!("{} {}", listener.at, listener.host_pin))
            }
            Command::Disable => {
                ensure!(self.policy.is_some(), "remote listener already disabled");
                self.policy = None;
                self.devices.clear();
                ("disable", String::new())
            }
            Command::Enroll(enrollment) => {
                self.check_enrollment(enrollment, wall)?;
                let device = &enrollment.device;
                self.devices.retain(|_, device| device.expires > wall);
                ensure!(
                    !self.devices.contains_key(&device.id),
                    "device is already enrolled"
                );
                let approval = hex::encode(digest(&enrollment.policy.listener.host_pin, device)?);
                ensure!(
                    !self.approved.contains_key(&approval),
                    "approval was already used"
                );
                self.approved.insert(approval, device.expires);
                self.devices.insert(device.id.clone(), device.clone());
                ("enroll", device.id.clone())
            }
            Command::Revoke(id) => {
                let device = self.devices.remove(id).context("unknown device")?;
                self.revoked.insert(id.clone(), device.expires);
                ("revoke", id.clone())
            }
        };
        self.record_history(wall, action, subject);
        Ok(self)
    }

    fn record_history(&mut self, at: i64, action: &str, subject: String) {
        if self.history.len() >= 128 {
            self.history.remove(0);
        }
        self.history.push(History {
            generation: self.generation,
            at,
            action: action.into(),
            subject,
        });
    }

    fn check_enrollment(&self, enrollment: &Enrollment, wall: i64) -> anyhow::Result<()> {
        let device = &enrollment.device;
        ensure!(
            self.policy.as_ref() == Some(&enrollment.policy),
            "listener policy changed"
        );
        ensure!(
            device.deadline > wall && device.deadline <= wall + OFFER_SECONDS as i64,
            "authorization request expired or out of bounds"
        );
        ensure!(
            pin(&device.id)
                && device.expires > wall
                && device.expires <= wall + MAX_VALID_SECS as i64,
            "invalid device authorization"
        );
        ensure!(
            !self
                .revoked
                .get(&device.id)
                .is_some_and(|expiry| *expiry > wall),
            "device was revoked; pair with a new key"
        );
        let attempt_key = hex::encode(digest(&enrollment.policy.listener.host_pin, device)?);
        ensure!(
            !self
                .failures
                .get(&attempt_key)
                .is_some_and(|failed| failed.attempts >= MAX_ATTEMPTS),
            "too many incorrect confirmations; start a new pairing"
        );
        Ok(())
    }

    fn failed_confirmation(mut self, enrollment: &Enrollment, wall: i64) -> anyhow::Result<Self> {
        self.check_enrollment(enrollment, wall)?;
        self.failures
            .retain(|_, failed| failed.last_attempt.saturating_add(86400) > wall);
        let device = &enrollment.device;
        let attempt_key = hex::encode(digest(&enrollment.policy.listener.host_pin, device)?);
        ensure!(
            self.failures.contains_key(&attempt_key) || self.failures.len() < FAILURE_LIMIT,
            "remote confirmation audit is full"
        );
        let failure = self
            .failures
            .entry(attempt_key)
            .or_insert_with(|| FailedConfirmation {
                device: device.id.clone(),
                attempts: 0,
                last_attempt: wall,
            });
        failure.attempts += 1;
        failure.last_attempt = wall;
        self.generation = self
            .generation
            .checked_add(1)
            .context("authorization generation exhausted")?;
        self.record_history(wall, "enrollment_rejected", device.id.clone());
        Ok(self)
    }

    pub fn live_devices(self, wall: i64) -> BTreeMap<String, Device> {
        self.devices
            .into_iter()
            .filter(|(_, d)| d.expires > wall && d.expires <= wall + MAX_VALID_SECS as i64)
            .collect()
    }
}

enum Command {
    Enable(Listener),
    Disable,
    Enroll(Enrollment),
    Revoke(String),
}

fn parse(args: impl IntoIterator<Item = String>) -> anyhow::Result<Command> {
    let mut args = args.into_iter();
    let command = match (args.next().as_deref(), args.next(), args.next()) {
        (Some("enable"), Some(request), None) => Command::Enable(decode(&request)?),
        (Some("disable"), None, None) => Command::Disable,
        (Some("enroll"), Some(request), None) => Command::Enroll(decode(&request)?),
        (Some("revoke"), Some(id), None) if pin(&id) => Command::Revoke(id),
        _ => bail!(
            "usage: senate-remote-admin enable REQUEST | disable | enroll REQUEST | revoke DEVICE-SPKI"
        ),
    };
    Ok(command)
}

fn reach(ip: IpAddr) -> &'static str {
    match ip.to_canonical() {
        ip if ip.is_loopback() => "loopback",
        IpAddr::V4(ip) if ip.is_link_local() => "link-local",
        IpAddr::V6(ip) if ip.is_unicast_link_local() => "link-local",
        IpAddr::V4(ip) if ip.is_private() => "private",
        IpAddr::V6(ip) if ip.is_unique_local() => "private",
        _ => "public or otherwise routable",
    }
}

/// Full digest of the exact authorization fields. Neither TLS exporter
/// material nor any unprivileged request nonce is an input.
pub fn digest(host_pin: &str, device: &Device) -> anyhow::Result<[u8; 32]> {
    let name = canonical_name(&device.name)?;
    ensure!(
        pin(host_pin) && pin(&device.id),
        "invalid enrollment public-key pin"
    );
    let mut digest = Sha256::new();
    digest.update(b"senate-remote-approval-v2\0");
    digest.update(hex::decode(host_pin)?);
    digest.update(hex::decode(&device.id)?);
    digest.update((name.len() as u16).to_be_bytes());
    digest.update(name.as_bytes());
    digest.update([u8::from(device.scopes.watch) | (u8::from(device.scopes.attach) << 1)]);
    digest.update(device.expires.to_be_bytes());
    digest.update(device.deadline.to_be_bytes());
    Ok(digest.finalize().into())
}

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

pub fn comparison(host_pin: &str, device: &Device) -> anyhow::Result<String> {
    let digest = digest(host_pin, device)?;
    let mut code = String::with_capacity(20);
    for group in 0..20 {
        let bit = group * 5;
        let word = u16::from_be_bytes([digest[bit / 8], digest[bit / 8 + 1]]);
        let value = (word >> (11 - bit % 8)) & 31;
        code.push(char::from(CROCKFORD[usize::from(value)]));
    }
    Ok(code)
}

pub fn grouped(code: &str) -> String {
    code.chars()
        .enumerate()
        .flat_map(|(index, ch)| {
            [((index > 0) && (index % 4 == 0)).then_some('-'), Some(ch)]
                .into_iter()
                .flatten()
        })
        .collect()
}

fn normalize(value: &str) -> Option<[u8; 20]> {
    let mut result = [0; 20];
    let mut at = 0;
    for byte in value.bytes() {
        let byte = match byte.to_ascii_uppercase() {
            b' ' | b'-' | b'\n' | b'\r' => continue,
            b'I' | b'L' => b'1',
            b'O' => b'0',
            other if CROCKFORD.contains(&other) => other,
            _ => return None,
        };
        *result.get_mut(at)? = byte;
        at += 1;
    }
    (at == result.len()).then_some(result)
}

trait Terminal: Write {
    fn answer(&mut self) -> io::Result<String>;
}

impl Terminal for File {
    fn answer(&mut self) -> io::Result<String> {
        os::flush_input(self)?;
        let mut answer = String::new();
        BufReader::new(self).take(256).read_line(&mut answer)?;
        Ok(answer)
    }
}

fn authorize(
    command: &Command,
    profile: &Profile,
    tty: &mut impl Terminal,
) -> anyhow::Result<bool> {
    let expected = match command {
        Command::Enable(listener) => {
            let address = listener.at.to_string();
            let risk = reach(listener.at.ip());
            writeln!(
                tty,
                "Enable {address} ({risk}), host SPKI {}. This replaces existing grants.",
                listener.host_pin
            )?;
            if risk == "public or otherwise routable" {
                writeln!(tty, "Type the exact listen address to authorize:")?;
                address
            } else {
                "enable".into()
            }
        }
        Command::Disable => {
            let listener = &profile
                .policy
                .as_ref()
                .context("no remote listener")?
                .listener;
            writeln!(
                tty,
                "Disable {} (host SPKI {}) and remove its device grants.",
                listener.at, listener.host_pin
            )?;
            "disable".into()
        }
        Command::Enroll(request) => {
            let device = &request.device;
            let expiry = chrono::DateTime::from_timestamp(device.expires, 0)
                .context("invalid grant expiry")?;
            let code = comparison(&request.policy.listener.host_pin, device)?;
            writeln!(
                tty,
                "Host {} (SPKI {}), device {} (SPKI {}).",
                request.policy.listener.at,
                &request.policy.listener.host_pin[..16],
                device.name,
                &device.id[..16]
            )?;
            writeln!(
                tty,
                "Scopes: list, watch={}, attach={}. Expires {expiry}.",
                device.scopes.watch, device.scopes.attach
            )?;
            writeln!(
                tty,
                "Type the comparison code read from your own client device:"
            )?;
            code
        }
        Command::Revoke(id) => {
            let device = profile.devices.get(id).context("unknown device")?;
            writeln!(tty, "Revoke {} (SPKI {}).", device.name, device.id)?;
            "revoke".into()
        }
    };
    if !matches!(command, Command::Enroll(_)) {
        writeln!(tty, "Type {expected} to authorize:")?;
    }
    tty.flush()?;
    let answer = tty.answer()?;
    ensure!(!answer.trim().is_empty(), "authorization cancelled");
    if matches!(command, Command::Enroll(_)) {
        Ok(normalize(&answer)
            .is_some_and(|answer| bool::from(answer.as_slice().ct_eq(expected.as_bytes()))))
    } else {
        Ok(bool::from(
            answer.trim().as_bytes().ct_eq(expected.as_bytes()),
        ))
    }
}

pub fn run() -> anyhow::Result<()> {
    let (uid, gid) = os::check_sudo()?;
    verify_helper()?;
    // The sudoers timeout is command-specific. A copy at a different path
    // is not covered by that rule.
    ensure!(
        std::env::current_exe()?.canonicalize()? == Path::new(HELPER),
        "run the helper at its protected installation path"
    );
    ensure!(
        std::env::var("SUDO_COMMAND")
            .ok()
            .as_deref()
            .and_then(|command| command.split_whitespace().next())
            == Some(HELPER),
        "invoke the protected helper directly through sudo"
    );
    verify_sudoers_drop_in()?;
    os::set_private_umask();
    let args = std::env::args_os()
        .skip(1)
        .map(|arg| {
            arg.into_string()
                .map_err(|_| anyhow::anyhow!("administrator argument is not UTF-8"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let command = parse(args)?;
    let mut tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("remote authorization requires a controlling terminal")?;
    ensure!(
        tty.is_terminal(),
        "remote authorization requires a controlling terminal"
    );
    Store {
        gid: Some(gid),
        ..Store::system(uid)
    }
    .transaction(command, &mut tty)
}

pub fn verify_sudoers_drop_in() -> anyhow::Result<()> {
    // /etc itself is a system symlink on macOS.
    let etc = Path::new("/etc").canonicalize()?;
    let path = etc.join("sudoers.d/99-senate-remote-admin");
    verify_system_chain(&path, false).context("verify the helper's sudoers drop-in")?;
    let file = OpenOptions::new().read(true).no_follow().open(path)?;
    let meta = file.metadata()?;
    verify_node(&meta, 0, false)?;
    ensure!(
        os::mode(&meta) & 0o7777 == 0o440,
        "helper sudoers drop-in mode must be 0440"
    );
    let mut contents = String::new();
    file.take(256).read_to_string(&mut contents)?;
    ensure!(
        contents == format!("{SUDOERS_RULE}\n"),
        "unexpected helper sudoers rule"
    );
    Ok(())
}

fn verify_helper() -> anyhow::Result<()> {
    verify_system_chain(Path::new(HELPER), false)?;
    ensure!(
        os::mode(&fs::metadata(HELPER)?) & 0o7777 == 0o755,
        "helper mode must be 0755"
    );
    Ok(())
}

pub fn check_install_location() -> anyhow::Result<()> {
    let prefix = Path::new("/usr/local/libexec");
    if prefix.exists() {
        verify_system_chain(prefix, true)
    } else {
        verify_system_chain(Path::new("/usr/local"), true)
    }
}

pub fn installed_helper_hash() -> anyhow::Result<String> {
    verify_helper()?;
    Ok(hex::encode(Sha256::digest(fs::read(HELPER)?)))
}

pub fn check_store() -> anyhow::Result<()> {
    let store = Store::system(os::current_uid());
    store.read_at(&store.directory(false)?)?;
    Ok(())
}

#[doc(hidden)]
pub fn test_authorize(store: Store, args: &[String], answer: &str) -> anyhow::Result<String> {
    ensure!(
        cfg!(debug_assertions) && store.isolated,
        "test authority is available only in an isolated debug build"
    );
    struct Console {
        answer: String,
        output: Vec<u8>,
    }
    impl Write for Console {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.output.write(bytes)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Terminal for Console {
        fn answer(&mut self) -> io::Result<String> {
            Ok(std::mem::take(&mut self.answer))
        }
    }
    let mut console = Console {
        answer: answer.into(),
        output: Vec::new(),
    };
    store.transaction(parse(args.iter().cloned())?, &mut console)?;
    Ok(String::from_utf8(console.output)?)
}

/// Opening the store's files: never through a link, never blocking on a
/// FIFO planted in their place; created owner-only.
trait OpenExt {
    fn no_follow(&mut self) -> &mut Self;
    fn private(&mut self) -> &mut Self;
}

impl OpenExt for OpenOptions {
    fn no_follow(&mut self) -> &mut Self {
        os::no_follow(self);
        self
    }

    fn private(&mut self) -> &mut Self {
        os::private(self);
        self
    }
}

/// Remote authority rests on root-owned files, Unix modes and sudo.
#[cfg(unix)]
mod os {
    use std::fs::{self, DirBuilder, File, OpenOptions};
    use std::io;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::Path;

    use anyhow::{Context, ensure};

    pub(super) const SUPPORTED: bool = true;

    pub(super) fn no_follow(options: &mut OpenOptions) {
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }

    pub(super) fn private(options: &mut OpenOptions) {
        options.mode(0o600);
    }

    pub(super) fn dir_mode(builder: &mut DirBuilder, mode: u32) {
        builder.mode(mode);
    }

    pub(super) fn set_path_mode(path: &Path, mode: u32) -> io::Result<()> {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }

    pub(super) fn set_mode(file: &File, mode: u32) -> io::Result<()> {
        file.set_permissions(fs::Permissions::from_mode(mode))
    }

    pub(super) fn owner(meta: &fs::Metadata) -> Option<u32> {
        Some(meta.uid())
    }

    pub(super) fn mode(meta: &fs::Metadata) -> u32 {
        meta.mode()
    }

    #[allow(unsafe_code)]
    pub(super) fn set_group(file: &File, gid: u32) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        // SAFETY: the descriptor refers to the new root-owned temporary profile.
        if unsafe { libc::fchown(file.as_raw_fd(), !0, gid) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[allow(unsafe_code)]
    pub(super) fn check_sudo() -> anyhow::Result<(u32, u32)> {
        // SAFETY: geteuid is scalar. getpwuid returns a pointer to libc-owned
        // storage. This process is single-threaded and copies pw_gid before
        // making another account lookup.
        ensure!(
            unsafe { libc::geteuid() } == 0,
            "run the installed helper through sudo"
        );
        let uid: u32 = std::env::var("SUDO_UID")
            .context("missing sudo identity")?
            .parse()?;
        let account = unsafe { libc::getpwuid(uid).as_ref() };
        let account = account
            .filter(|_| uid != 0)
            .context("sudo must name a real non-root user")?;
        Ok((uid, account.pw_gid))
    }

    #[allow(unsafe_code)]
    pub(super) fn set_private_umask() {
        // SAFETY: this runs once in the single-threaded administrator binary.
        unsafe { libc::umask(0o077) };
    }

    #[allow(unsafe_code)]
    pub(super) fn flush_input(tty: &File) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        // SAFETY: the live descriptor refers to the controlling terminal.
        if unsafe { libc::tcflush(tty.as_raw_fd(), libc::TCIFLUSH) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[allow(unsafe_code)]
    pub(super) fn current_uid() -> u32 {
        // SAFETY: getuid has no arguments and touches no caller memory.
        unsafe { libc::getuid() }
    }
}

/// No root-owned store and no sudo: every check fails closed, so no remote
/// device is ever authorized and the administrator command refuses to run.
#[cfg(not(unix))]
mod os {
    use std::fs::{self, DirBuilder, File, OpenOptions};
    use std::io;
    use std::path::Path;

    const UNSUPPORTED: &str = "remote authority is not supported on this platform";

    pub(super) const SUPPORTED: bool = false;

    pub(super) fn no_follow(_: &mut OpenOptions) {}

    pub(super) fn private(_: &mut OpenOptions) {}

    pub(super) fn dir_mode(_: &mut DirBuilder, _: u32) {}

    pub(super) fn set_path_mode(_: &Path, _: u32) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, UNSUPPORTED))
    }

    pub(super) fn set_mode(_: &File, _: u32) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, UNSUPPORTED))
    }

    pub(super) fn owner(_: &fs::Metadata) -> Option<u32> {
        None
    }

    pub(super) fn mode(_: &fs::Metadata) -> u32 {
        0o7777
    }

    pub(super) fn set_group(_: &File, _: u32) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, UNSUPPORTED))
    }

    pub(super) fn check_sudo() -> anyhow::Result<(u32, u32)> {
        anyhow::bail!(UNSUPPORTED)
    }

    pub(super) fn set_private_umask() {}

    pub(super) fn flush_input(_: &File) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, UNSUPPORTED))
    }

    pub(super) fn current_uid() -> u32 {
        u32::MAX
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_digest_vector_and_normalization() -> anyhow::Result<()> {
        let host = "a".repeat(64);
        let mut device = Device {
            id: "b".repeat(64),

            name: "test-client".into(),
            scopes: Scopes {
                watch: true,
                attach: false,
            },
            expires: 1_750_000_000,
            deadline: 1_749_999_990,
        };
        assert_eq!(
            hex::encode(digest(&host, &device)?),
            "f8fff8a1ce406e4ddd87074cba7f8abfd221bd040baeea25441ed2df1c1c923d"
        );
        let code = comparison(&host, &device)?;
        assert_eq!(code, "Z3ZZH8EE81Q4VQC70X6B");
        assert_eq!(
            normalize(&grouped(&code).to_ascii_lowercase())
                .as_ref()
                .map(|s| s.as_slice()),
            Some(code.as_bytes())
        );
        assert_eq!(
            normalize("oiLl- 2345-6789-ABCD-EFGH"),
            Some(*b"011123456789ABCDEFGH")
        );
        assert!(normalize("1234").is_none());
        assert!(normalize("12💥34").is_none());
        device.scopes.attach = true;
        assert_ne!(comparison(&host, &device)?, code);
        device.scopes.attach = false;
        device.expires += 1;
        assert_ne!(comparison(&host, &device)?, code);
        device.expires -= 1;
        device.deadline += 1;
        assert_ne!(comparison(&host, &device)?, code);
        device.deadline -= 1;
        device.name.push_str("-other");
        assert_ne!(comparison(&host, &device)?, code);
        assert!(canonical_name("bad\u{202e}name").is_err());
        assert!(unsafe_display('\u{2067}'));
        Ok(())
    }

    #[cfg(not(unix))]
    #[test]
    fn remote_authority_fails_closed_without_unix_ownership() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let store = Store::isolated(root.path());
        assert!(store.directory(true).is_err());
        assert!(store.read().is_err());
        assert!(os::check_sudo().is_err());
        assert!(run().is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_authority_directory_and_profile_are_refused() -> anyhow::Result<()> {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir()?;
        let other = tempfile::tempdir()?;
        let store = Store::isolated(root.path());
        symlink(other.path(), &store.dir)?;
        assert!(store.read().is_err());
        fs::remove_file(&store.dir)?;
        let dir = store.directory(true)?;
        let victim = other.path().join("victim");
        fs::write(&victim, serde_json::to_vec(&Profile::default())?)?;
        symlink(&victim, store.profile_path(&dir))?;
        assert!(store.read().is_err());
        Ok(())
    }

    fn listener() -> anyhow::Result<Listener> {
        Ok(Listener {
            at: "127.0.0.1:9443".parse()?,
            host_pin: "a".repeat(64),
        })
    }

    #[cfg(unix)]
    #[test]
    fn a_changed_generation_requires_confirmation_again() -> anyhow::Result<()> {
        struct ChangingConsole {
            store: Store,
            command: [String; 2],
        }
        impl Write for ChangingConsole {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl Terminal for ChangingConsole {
            fn answer(&mut self) -> io::Result<String> {
                test_authorize(self.store.clone(), &self.command, "enable")
                    .map_err(io::Error::other)?;
                Ok("enable".into())
            }
        }
        let root = tempfile::tempdir()?;
        let store = Store::isolated(root.path());
        let listener = listener()?;
        let command = ["enable".into(), encode(&listener)?];
        let mut tty = ChangingConsole {
            store: store.clone(),
            command,
        };
        let result = store.transaction(Command::Enable(listener), &mut tty);
        assert!(result.is_err_and(|error| error.to_string().contains("authorization changed")));
        assert_eq!(store.read()?.generation, 1);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn administrator_transaction_requires_confirmation_and_removes_grants() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let store = Store::isolated(root.path());
        let command = ["enable".into(), encode(listener()?)?];
        assert!(test_authorize(store.clone(), &command, "no").is_err());
        assert!(store.read()?.policy.is_none());
        test_authorize(store.clone(), &command, "enable")?;
        let profile = store.read()?;
        let device = Device {
            id: "b".repeat(64),

            name: "test-client".into(),
            scopes: Scopes {
                watch: true,
                attach: false,
            },
            expires: now() + 60,
            deadline: now() + 30,
        };
        let enroll = Enrollment {
            policy: profile.policy.context("enabled policy")?,
            device: device.clone(),
        };
        let command = ["enroll".into(), encode(&enroll)?];
        let before_empty = store.read()?.generation;
        assert!(test_authorize(store.clone(), &command, "").is_err());
        assert_eq!(store.read()?.generation, before_empty);
        let mut expired = enroll.clone();
        expired.device.deadline = now() - 1;
        assert!(
            test_authorize(
                store.clone(),
                &["enroll".into(), encode(expired)?],
                "ignored"
            )
            .is_err()
        );
        let mut changed_policy = enroll.clone();
        changed_policy.policy.revision += 1;
        assert!(
            test_authorize(
                store.clone(),
                &["enroll".into(), encode(changed_policy)?],
                "ignored"
            )
            .is_err()
        );
        assert!(test_authorize(store.clone(), &command, "no").is_err());
        assert!(store.read()?.devices.is_empty());
        let code = comparison(&enroll.policy.listener.host_pin, &device)?;
        let mut changed = enroll.clone();
        changed.device.id = "e".repeat(64);
        let changed = ["enroll".into(), encode(changed)?];
        assert!(test_authorize(store.clone(), &changed, &code).is_err());
        assert!(store.read()?.devices.is_empty());
        test_authorize(store.clone(), &command, &code)?;
        assert_eq!(store.read()?.devices.get(&device.id), Some(&device));
        assert!(
            test_authorize(store.clone(), &command, &code)
                .is_err_and(|error| error.to_string().contains("already enrolled"))
        );
        test_authorize(store.clone(), &["revoke".into(), device.id], "revoke")?;
        assert!(
            test_authorize(store.clone(), &command, &code)
                .is_err_and(|error| error.to_string().contains("revoked"))
        );
        assert!(store.read()?.devices.is_empty());
        assert_eq!(store.read()?.generation, 5);
        test_authorize(store.clone(), &["disable".into()], "disable")?;
        assert!(store.read()?.policy.is_none());
        assert_eq!(store.read()?.generation, 6);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn three_wrong_confirmations_are_audited_and_require_a_new_offer() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let store = Store::isolated(root.path());
        test_authorize(
            store.clone(),
            &["enable".into(), encode(listener()?)?],
            "enable",
        )?;
        let enrollment = Enrollment {
            policy: store.read()?.policy.context("enabled policy")?,
            device: Device {
                id: "b".repeat(64),

                name: "dev".into(),
                scopes: Scopes {
                    watch: true,
                    attach: false,
                },
                expires: now() + 60,
                deadline: now() + 30,
            },
        };
        let command = ["enroll".into(), encode(&enrollment)?];
        for _ in 0..MAX_ATTEMPTS {
            assert!(test_authorize(store.clone(), &command, "wrong").is_err());
        }
        let profile = store.read()?;
        assert!(profile.devices.is_empty());
        assert_eq!(
            profile
                .failures
                .get(&hex::encode(digest(
                    &enrollment.policy.listener.host_pin,
                    &enrollment.device
                )?))
                .map(|f| f.attempts),
            Some(3)
        );
        let device = &enrollment.device;
        let code = comparison(&enrollment.policy.listener.host_pin, device)?;
        assert!(test_authorize(store, &command, &code).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn authority_rejects_untrusted_modes_and_expiry() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir()?;
        let store = Store::isolated(root.path());
        let dir = store.directory(true)?;
        let path = store.profile_path(&dir);
        fs::write(&path, serde_json::to_vec(&Profile::default())?)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666))?;
        assert!(store.read().is_err());
        assert!(verify_node(&fs::metadata(&path)?, store.owner.wrapping_add(1), false).is_err());

        let wall = now();
        let profile = Profile::default().apply(&Command::Enable(listener()?), true, wall)?;
        let device = Device {
            id: "b".repeat(64),

            name: "device".into(),
            scopes: Scopes {
                watch: false,
                attach: true,
            },
            expires: wall + MAX_VALID_SECS as i64 + 1,
            deadline: wall + 60,
        };
        let enroll = Enrollment {
            policy: profile.policy.clone().context("enabled policy")?,
            device,
        };
        assert!(profile.apply(&Command::Enroll(enroll), true, wall).is_err());
        Ok(())
    }

    #[test]
    fn helper_request_is_bounded_versioned_and_rejects_unknown_fields() -> anyhow::Result<()> {
        let raw =
            |request| URL_SAFE_NO_PAD.encode(serde_json::to_vec(&request).unwrap_or_default());
        let invalid = serde_json::json!({"version": VERSION, "request": {
            "at": "127.0.0.1:9", "host_pin": "a".repeat(64), "path": "/tmp"
        }});
        assert!(parse(["enable".into(), raw(invalid)]).is_err());
        assert!(parse(["enable".into(), "A".repeat(REQUEST_LIMIT + 1)]).is_err());
        let version = serde_json::json!({"version": 1, "request": listener()?});
        assert!(parse(["enable".into(), raw(version)]).is_err());
        Ok(())
    }
}
