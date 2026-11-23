//! Root-owned remote authorization. The bridge reads this store; only the
//! standalone administrator command changes it.

use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use super::{Device, MAX_VALID_SECS, now};

pub(crate) const HELPER: &str = "/usr/local/libexec/senate-remote-admin";
pub(crate) const SYSTEM_DIR: &str = "/etc/senate/remote";
const VERSION: u8 = 1;
const REQUEST_LIMIT: usize = 2048;
const STORE_LIMIT: u64 = 128 * 1024;
const MAX_ATTEMPTS: u8 = 3;
const FAILURE_LIMIT: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Listener {
    pub(super) at: SocketAddr,
    pub(super) host_pin: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Policy {
    pub(super) revision: u64,
    pub(super) listener: Listener,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Enrollment {
    pub(super) policy: Policy,
    pub(super) device: Device,
    pub(super) deadline: i64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Request<T> {
    version: u8,
    request: T,
}

pub(super) fn encode<T: Serialize>(request: T) -> anyhow::Result<String> {
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
pub(super) struct Profile {
    version: u8,
    pub(super) generation: u64,
    pub(super) policy: Option<Policy>,
    pub(super) devices: BTreeMap<String, Device>,
    #[serde(default)]
    failures: BTreeMap<String, FailedConfirmation>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            version: VERSION,
            generation: 0,
            policy: None,
            devices: BTreeMap::new(),
            failures: BTreeMap::new(),
        }
    }
}

#[derive(Clone)]
pub(super) struct Store {
    dir: PathBuf,
    uid: u32,
    owner: u32,
    isolated: bool,
}

impl Store {
    pub(super) fn system(uid: u32) -> Self {
        Self {
            dir: SYSTEM_DIR.into(),
            uid,
            owner: 0,
            isolated: false,
        }
    }

    pub(super) fn isolated(root: &Path) -> Self {
        let uid = crate::service::current_uid();
        Self {
            dir: root.join("authorization"),
            uid,
            owner: uid,
            isolated: true,
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
        dir.join(format!("{}.json", self.uid))
    }

    pub(super) fn read(&self) -> anyhow::Result<Profile> {
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
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
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
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
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
        file.as_file()
            .set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        file.persist(self.profile_path(&dir))?;
        File::open(dir)?.sync_all()?;
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
    match DirBuilder::new().mode(mode).create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

fn verify_node(meta: &fs::Metadata, uid: u32, directory: bool) -> anyhow::Result<()> {
    ensure!(
        (if directory {
            meta.is_dir()
        } else {
            meta.is_file()
        }) && meta.uid() == uid
            && meta.mode() & 0o022 == 0,
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

pub(crate) fn pin(value: &str) -> bool {
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
            self.devices
                .iter()
                .all(|(id, entry)| id == &entry.id && pin(id) && pin(&entry.grant)),
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
        match command {
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
            }
            Command::Disable => {
                ensure!(self.policy.is_some(), "remote listener already disabled");
                self.policy = None;
                self.devices.clear();
            }
            Command::Enroll(enrollment) => {
                self.check_enrollment(enrollment, wall)?;
                let device = &enrollment.device;
                self.devices.retain(|_, device| device.expires > wall);
                ensure!(
                    !self.devices.contains_key(&device.id),
                    "device is already enrolled"
                );
                self.devices.insert(device.id.clone(), device.clone());
            }
            Command::Revoke(id) => {
                ensure!(self.devices.remove(id).is_some(), "unknown device");
            }
        }
        Ok(self)
    }

    fn check_enrollment(&self, enrollment: &Enrollment, wall: i64) -> anyhow::Result<()> {
        let device = &enrollment.device;
        ensure!(
            self.policy.as_ref() == Some(&enrollment.policy),
            "listener policy changed"
        );
        ensure!(
            enrollment.deadline > wall
                && enrollment.deadline <= wall + super::OFFER_TTL.as_secs() as i64,
            "authorization request expired or out of bounds"
        );
        ensure!(
            pin(&device.id)
                && pin(&device.grant)
                && device.expires > wall
                && device.expires <= wall + MAX_VALID_SECS as i64,
            "invalid device authorization"
        );
        ensure!(
            !self
                .failures
                .get(&device.grant)
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
        ensure!(
            self.failures.contains_key(&device.grant) || self.failures.len() < FAILURE_LIMIT,
            "remote confirmation audit is full"
        );
        let failure = self
            .failures
            .entry(device.grant.clone())
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
        Ok(self)
    }

    pub(super) fn live_devices(self, wall: i64) -> BTreeMap<String, Device> {
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

pub(super) fn comparison(
    binding: &str,
    host_pin: &str,
    device_pin: &str,
    name: &senate_core::Name,
) -> String {
    // All three key values have fixed-width hex encodings. Rebind the
    // displayed device and host here, even though the exporter also
    // commits to them. The unprivileged CLI supplies the request fields.
    let mut digest = Sha256::new();
    digest.update(b"senate-admin-comparison-v1");
    digest.update(binding.as_bytes());
    digest.update(host_pin.as_bytes());
    digest.update(device_pin.as_bytes());
    digest.update(name.to_string().as_bytes());
    let digest = digest.finalize();
    let mut digits = [0; 8];
    digits.copy_from_slice(&digest[..8]);
    format!("{:010}", u64::from_be_bytes(digits) % 10_000_000_000)
}

trait Terminal: Write {
    fn answer(&mut self) -> io::Result<String>;
}

impl Terminal for File {
    fn answer(&mut self) -> io::Result<String> {
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
            let code = comparison(
                &device.grant,
                &request.policy.listener.host_pin,
                &device.id,
                &device.name,
            );
            writeln!(
                tty,
                "Host {} (SPKI {}), device {} (SPKI {}).",
                request.policy.listener.at,
                request.policy.listener.host_pin,
                device.name,
                device.id
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
    Ok(bool::from(
        tty.answer()?.trim().as_bytes().ct_eq(expected.as_bytes()),
    ))
}

#[allow(unsafe_code)]
fn check_sudo() -> anyhow::Result<u32> {
    // SAFETY: geteuid is scalar. getpwuid returns a pointer we only test
    // for null in this single-threaded privileged process, never dereference.
    ensure!(
        unsafe { libc::geteuid() } == 0,
        "run the installed helper through sudo"
    );
    let uid: u32 = std::env::var("SUDO_UID")
        .context("missing sudo identity")?
        .parse()?;
    ensure!(
        uid != 0 && !unsafe { libc::getpwuid(uid) }.is_null(),
        "sudo must name a real non-root user"
    );
    Ok(uid)
}

pub(super) fn run() -> anyhow::Result<()> {
    let uid = check_sudo()?;
    verify_helper()?;
    ensure!(
        std::env::current_exe()?.canonicalize()? == Path::new(HELPER).canonicalize()?,
        "run the installed helper through sudo"
    );
    let command = parse(std::env::args().skip(1))?;
    let mut tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("remote authorization requires a controlling terminal")?;
    ensure!(
        tty.is_terminal(),
        "remote authorization requires a controlling terminal"
    );
    Store::system(uid).transaction(command, &mut tty)
}

fn verify_helper() -> anyhow::Result<()> {
    verify_system_chain(Path::new(HELPER), false)?;
    ensure!(
        fs::metadata(HELPER)?.mode() & 0o7777 == 0o755,
        "helper mode must be 0755"
    );
    Ok(())
}

pub(crate) fn check_install_location() -> anyhow::Result<()> {
    let prefix = Path::new("/usr/local/libexec");
    if prefix.exists() {
        verify_system_chain(prefix, true)
    } else {
        verify_system_chain(Path::new("/usr/local"), true)
    }
}

pub(crate) fn installation_checks() -> Vec<crate::doctor::Check> {
    use crate::doctor::Check;

    let helper =
        match verify_helper().and_then(|()| Ok(hex::encode(Sha256::digest(fs::read(HELPER)?)))) {
            Ok(sha) => match option_env!("SENATE_REMOTE_ADMIN_SHA256") {
                Some(expected) if sha == expected => Check::ok(
                    "remote.helper",
                    format!("root-owned 0755, SHA-256 {sha} matches this build"),
                ),
                Some(expected) => Check::fail(
                    "remote.helper",
                    format!("SHA-256 {sha} differs from expected {expected}"),
                    None::<String>,
                ),
                None => Check::warn(
                    "remote.helper",
                    format!("root-owned 0755, SHA-256 {sha}. This build has no expected hash"),
                    None::<String>,
                ),
            },
            Err(error) if missing(&error) => Check::ok("remote.helper", "not installed"),
            Err(error) => Check::fail("remote.helper", error.to_string(), None::<String>),
        };
    let store = Store::system(crate::service::current_uid());
    let authority = match store.directory(false) {
        Ok(dir) => match store.read_at(&dir) {
            Ok(_) => Check::ok(
                "remote.authority",
                "root-owned directories and profile, no group or other write access",
            ),
            Err(error) => Check::fail("remote.authority", error.to_string(), None::<String>),
        },
        Err(error) if missing(&error) => Check::ok("remote.authority", "not installed"),
        Err(error) => Check::fail("remote.authority", error.to_string(), None::<String>),
    };
    vec![helper, authority]
}

pub(super) fn test_authorize(
    store: Store,
    args: &[String],
    answer: &str,
) -> anyhow::Result<String> {
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

#[cfg(test)]
mod tests {
    use super::super::Scopes;
    use super::*;

    fn listener() -> anyhow::Result<Listener> {
        Ok(Listener {
            at: "127.0.0.1:9443".parse()?,
            host_pin: "a".repeat(64),
        })
    }

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
            grant: "c".repeat(64),
            name: "test-client".parse()?,
            scopes: Scopes {
                watch: true,
                attach: false,
            },
            expires: now() + 60,
        };
        let enroll = Enrollment {
            policy: profile.policy.context("enabled policy")?,
            device: device.clone(),
            deadline: now() + 30,
        };
        let command = ["enroll".into(), encode(&enroll)?];
        assert!(test_authorize(store.clone(), &command, "no").is_err());
        assert!(store.read()?.devices.is_empty());
        let code = comparison(
            &device.grant,
            &enroll.policy.listener.host_pin,
            &device.id,
            &device.name,
        );
        let mut changed = enroll.clone();
        changed.device.id = "e".repeat(64);
        let changed = ["enroll".into(), encode(changed)?];
        assert!(test_authorize(store.clone(), &changed, &code).is_err());
        assert!(store.read()?.devices.is_empty());
        test_authorize(store.clone(), &command, &code)?;
        assert_eq!(store.read()?.devices.get(&device.id), Some(&device));
        test_authorize(store.clone(), &["revoke".into(), device.id], "revoke")?;
        assert!(store.read()?.devices.is_empty());
        assert_eq!(store.read()?.generation, 5);
        test_authorize(store.clone(), &["disable".into()], "disable")?;
        assert!(store.read()?.policy.is_none());
        assert_eq!(store.read()?.generation, 6);
        Ok(())
    }

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
                grant: "c".repeat(64),
                name: "dev".parse()?,
                scopes: Scopes {
                    watch: true,
                    attach: false,
                },
                expires: now() + 60,
            },
            deadline: now() + 30,
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
                .get(&enrollment.device.grant)
                .map(|f| f.attempts),
            Some(3)
        );
        let device = &enrollment.device;
        let code = comparison(
            &device.grant,
            &enrollment.policy.listener.host_pin,
            &device.id,
            &device.name,
        );
        assert!(test_authorize(store, &command, &code).is_err());
        Ok(())
    }

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
            grant: "c".repeat(64),
            name: "device".parse()?,
            scopes: Scopes {
                watch: false,
                attach: true,
            },
            expires: wall + MAX_VALID_SECS as i64 + 1,
        };
        let enroll = Enrollment {
            policy: profile.policy.clone().context("enabled policy")?,
            device,
            deadline: wall + 60,
        };
        assert!(profile.apply(&Command::Enroll(enroll), true, wall).is_err());
        Ok(())
    }

    #[test]
    fn helper_request_is_bounded_versioned_and_rejects_unknown_fields() -> anyhow::Result<()> {
        let raw =
            |request| URL_SAFE_NO_PAD.encode(serde_json::to_vec(&request).unwrap_or_default());
        let invalid = serde_json::json!({"version": 1, "request": {
            "at": "127.0.0.1:9", "host_pin": "a".repeat(64), "path": "/tmp"
        }});
        assert!(parse(["enable".into(), raw(invalid)]).is_err());
        assert!(parse(["enable".into(), "A".repeat(REQUEST_LIMIT + 1)]).is_err());
        let version = serde_json::json!({"version": 2, "request": listener()?});
        assert!(parse(["enable".into(), raw(version)]).is_err());
        Ok(())
    }
}
