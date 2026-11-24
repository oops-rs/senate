use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use senate_core::{CommitteeId, SenatorName, Seq};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::watch;
use url::Url;

use crate::audit::Audit;
use crate::cards::{CardKey, Cards};
use crate::previews::{LinePolicy, Preview, Previews};
use crate::projection::Projections;
use crate::{MachineFacts, MachineSource, ProjectionError, SenateSource};

const MAX_GRANTS: usize = 64;
const DEFAULT_DAYS: u16 = 30;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AdminRequest {
    Create {
        grant: CreateGrant,
    },
    List,
    Identity,
    HistorySession {
        id: String,
    },
    Preview {
        committee: CommitteeId,
        seq: Seq,
    },
    Publish {
        committee: CommitteeId,
        seq: Seq,
        reviewed: Preview,
    },
    Withdraw {
        committee: CommitteeId,
        seq: Seq,
    },
    Revoke {
        id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateGrant {
    pub name: String,
    pub committees: Option<Vec<CommitteeId>>,
    pub all_committees: bool,
    pub scope: Scope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<LinePolicy>,
    pub origins: Vec<String>,
    pub topology: bool,
    pub expires_in_days: Option<u16>,
    /// Set by the daemon after receiving approval on its authenticated app channel.
    #[serde(skip)]
    pub auto_confirmed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Metadata,
    Lines,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrantInfo {
    pub id: String,
    pub name: String,
    pub committees: Option<BTreeSet<CommitteeId>>,
    pub scope: Scope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<LinePolicy>,
    pub origins: BTreeSet<String>,
    pub topology: bool,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installation {
    pub id: String,
    pub url: String,
}

#[derive(Serialize, Deserialize)]
pub struct IssuedGrant {
    pub url: String,
    pub token: String,
    pub grant: GrantInfo,
}

#[derive(Debug, thiserror::Error)]
pub enum GrantError {
    #[error("observer grant request is invalid: {0}")]
    Invalid(&'static str),
    #[error("observer grant not found")]
    NotFound,
    #[error("observer grant storage is unavailable")]
    Storage(#[from] io::Error),
    #[error("observer grant storage could not be decoded")]
    Format(#[from] serde_json::Error),
    #[error("observer token randomness is unavailable")]
    Random,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredGrant {
    info: GrantInfo,
    digest: [u8; 32],
}

struct ActiveGrant {
    stored: StoredGrant,
    reads: Bucket,
    streams: Bucket,
    revoked: watch::Sender<bool>,
}

impl ActiveGrant {
    fn new(stored: StoredGrant) -> Self {
        Self {
            stored,
            reads: Bucket::new(60, 4),
            streams: Bucket::new(30, 4),
            revoked: watch::channel(false).0,
        }
    }
}

#[derive(Clone)]
pub(crate) struct Authorized {
    pub(crate) info: GrantInfo,
    pub(crate) revoked: watch::Receiver<bool>,
}

#[derive(Clone, Copy)]
pub(crate) enum AuthError {
    Unauthorized,
    Origin,
    Rate,
    AuthenticatedRate,
}

pub(crate) struct Bucket {
    capacity: f64,
    refill_per_sec: f64,
    balance: f64,
    updated: Instant,
}

impl Bucket {
    fn new(per_minute: u16, burst: u16) -> Self {
        Self {
            capacity: f64::from(burst),
            refill_per_sec: f64::from(per_minute) / 60.0,
            balance: f64::from(burst),
            updated: Instant::now(),
        }
    }

    fn take(&mut self) -> bool {
        let now = Instant::now();
        self.balance = (self.balance
            + now.duration_since(self.updated).as_secs_f64() * self.refill_per_sec)
            .min(self.capacity);
        self.updated = now;
        if self.balance < 1.0 {
            return false;
        }
        self.balance -= 1.0;
        true
    }
}

pub struct Registry {
    address: SocketAddr,
    path: PathBuf,
    grants: Mutex<BTreeMap<String, ActiveGrant>>,
    unauthenticated: Mutex<Bucket>,
    preflights: Mutex<Bucket>,
    pub(crate) audit: Arc<Audit>,
    pub(crate) projections: Arc<Projections>,
    seed: [u8; 32],
    machine: Mutex<Option<Arc<dyn MachineSource>>>,
    previews: Previews,
    cards: Cards,
    pub(crate) releases: crate::releases::Releases,
}

impl Registry {
    pub fn open(directory: &Path, address: SocketAddr) -> Result<Arc<Self>, GrantError> {
        if !address.ip().is_loopback() || address.port() == 0 {
            return Err(GrantError::Invalid("a bound loopback address is required"));
        }
        fs::create_dir_all(directory)?;
        let path = directory.join("observe-grants.json");
        let stored: Vec<StoredGrant> = match File::open(&path) {
            Ok(file) => serde_json::from_reader(file)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        if stored.len() > MAX_GRANTS {
            return Err(GrantError::Invalid("too many stored grants"));
        }
        Ok(Arc::new(Self {
            address,
            path,
            grants: Mutex::new(
                stored
                    .into_iter()
                    .filter(|grant| grant.info.expires_at > Utc::now())
                    .map(|grant| (grant.info.id.clone(), ActiveGrant::new(grant)))
                    .collect(),
            ),
            unauthenticated: Mutex::new(Bucket::new(120, 20)),
            preflights: Mutex::new(Bucket::new(120, 20)),
            audit: Arc::new(Audit::new(directory.join("observe-audit.jsonl"))),
            projections: Projections::new(),
            seed: installation_seed(directory)?,
            machine: Mutex::new(None),
            previews: Previews::open(directory)?,
            cards: Cards::open(directory)?,
            releases: crate::releases::Releases::default(),
        }))
    }

    pub fn attach_senate(&self, source: Arc<dyn SenateSource>) {
        self.projections.set_source(source);
    }

    pub fn shutdown_observation(&self) {
        self.projections.shutdown();
    }

    pub fn attach_machine(&self, source: Arc<dyn MachineSource>) {
        *lock(&self.machine) = Some(source);
    }

    pub fn installation(&self) -> Result<Installation, GrantError> {
        Ok(Installation {
            id: self.installation_id()?,
            url: format!("http://{}", self.address),
        })
    }

    pub fn installation_id(&self) -> Result<String, GrantError> {
        self.scoped_id(b"senate", b"")
    }

    pub(crate) fn project_id(&self, root: &Path) -> Result<String, GrantError> {
        self.scoped_id(b"project", root.as_os_str().as_encoded_bytes())
    }

    fn scoped_id(&self, domain: &[u8], value: &[u8]) -> Result<String, GrantError> {
        let mut hash =
            Hmac::<Sha256>::new_from_slice(&self.seed).map_err(|_| GrantError::Random)?;
        hash.update(domain);
        hash.update(&[0]);
        hash.update(value);
        Ok(hex::encode(&hash.finalize().into_bytes()[..16]))
    }

    pub(crate) fn machine_facts(&self) -> Result<MachineFacts, ProjectionError> {
        let source = lock(&self.machine)
            .clone()
            .ok_or(ProjectionError::NotReady)?;
        source.read()
    }

    pub async fn merge_machine_facts(
        &self,
        grant: &GrantInfo,
        snapshot: &mut senate_observe_protocol::Snapshot,
    ) -> Result<(), ProjectionError> {
        crate::machine::merge(self, grant, snapshot).await
    }

    pub fn tool_demand(&self) -> Option<crate::ToolDemand> {
        self.projections.tool_demand()
    }

    pub fn release_machine_observation(&self) {
        if let Some(machine) = lock(&self.machine).as_ref() {
            machine.idle();
        }
    }

    pub fn tool_identity(&self, grant: &GrantInfo, id: &str) -> Result<String, GrantError> {
        self.scoped_id(b"tool", format!("{}:{id}", grant.id).as_bytes())
    }

    pub fn is_watched(&self) -> bool {
        self.projections.is_watched()
    }

    pub fn notice(&self, notice: crate::LocalNotice) {
        self.projections.notice(notice);
    }

    pub fn history_id(&self, native: &str) -> Result<String, GrantError> {
        Ok(format!(
            "obs-{}",
            self.scoped_id(b"history", native.as_bytes())?
        ))
    }

    pub fn approval_id(&self, native: &str) -> Result<String, GrantError> {
        self.scoped_id(b"approval", native.as_bytes())
    }

    pub fn route_id(
        &self,
        grant: &GrantInfo,
        committee: &CommitteeId,
        seq: Seq,
        senator: Option<&SenatorName>,
    ) -> Result<String, GrantError> {
        let target = senator.map_or("", SenatorName::as_str);
        let key = format!("{}:{committee}:{seq}:{target}", grant.id);
        self.scoped_id(b"route", key.as_bytes())
    }

    pub async fn preview(&self, committee: &CommitteeId, seq: Seq) -> Result<Preview, GrantError> {
        let source = self
            .projections
            .source()
            .ok_or(GrantError::Invalid("Observe source is unavailable"))?;
        source
            .preview(committee, seq)
            .await
            .ok_or(GrantError::Invalid(
                "no eligible first line at that position",
            ))
    }

    pub async fn publish_line(
        &self,
        committee: &CommitteeId,
        seq: Seq,
        reviewed: Preview,
    ) -> Result<(), GrantError> {
        let candidate = self.preview(committee, seq).await?;
        if candidate != reviewed {
            return Err(GrantError::Invalid(
                "the reviewed line changed before publication",
            ));
        }
        if self.previews.publish(candidate)? {
            self.projections.notice(crate::LocalNotice::Preview {
                committee: committee.clone(),
                seq,
            });
        }
        Ok(())
    }

    pub async fn withdraw_line(&self, committee: &CommitteeId, seq: Seq) -> Result<(), GrantError> {
        let candidate = self.preview(committee, seq).await?;
        if self.previews.withdraw(committee, &candidate.event_id)? {
            self.projections.invalidate_lines();
        }
        Ok(())
    }

    pub fn line_for(&self, grant: &GrantInfo, event: &senate_core::Event) -> Option<String> {
        self.previews.line(grant.lines?, event)
    }

    pub fn card(&self, key: &CardKey) -> Option<senate_observe_protocol::Card> {
        self.cards.get(key)
    }

    pub fn set_card(
        &self,
        key: CardKey,
        writer: &SenatorName,
        document: &str,
    ) -> Result<(), GrantError> {
        if let Some(revision) = self.cards.set(key.clone(), writer, document)? {
            self.projections
                .notice(crate::LocalNotice::Card { key, revision });
        }
        Ok(())
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn create(
        &self,
        request: CreateGrant,
        readable: BTreeSet<CommitteeId>,
    ) -> Result<IssuedGrant, GrantError> {
        let name = request.name.trim();
        if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
            return Err(GrantError::Invalid(
                "expected a display name of 1 to 80 characters",
            ));
        }
        match (request.scope, request.lines) {
            (Scope::Metadata, None) | (Scope::Lines, Some(LinePolicy::Published)) => {}
            (Scope::Lines, Some(LinePolicy::Auto)) if request.auto_confirmed => {}
            (Scope::Lines, Some(LinePolicy::Auto)) => {
                return Err(GrantError::Invalid(
                    "automatic previews need explicit confirmation",
                ));
            }
            _ => {
                return Err(GrantError::Invalid(
                    "a lines grant must select auto or published previews",
                ));
            }
        }
        let days = request.expires_in_days.unwrap_or(DEFAULT_DAYS);
        if !(1..=90).contains(&days) {
            return Err(GrantError::Invalid("expiry must be 1 to 90 days"));
        }
        if request.origins.len() > 8 || request.origins.iter().any(|origin| !valid_origin(origin)) {
            return Err(GrantError::Invalid(
                "expected at most eight exact HTTPS or literal loopback HTTP origins",
            ));
        }
        let committees = match (request.all_committees, request.committees) {
            (true, None) => None,
            (false, None) => Some(readable),
            (false, Some(committees))
                if !committees.is_empty()
                    && committees
                        .iter()
                        .all(|committee| readable.contains(committee)) =>
            {
                Some(committees.into_iter().collect())
            }
            _ => return Err(GrantError::Invalid("select readable committees or all")),
        };

        let mut grants = lock(&self.grants);
        let now = Utc::now();
        grants.retain(|_, grant| grant.stored.info.expires_at > now);
        if grants.len() >= MAX_GRANTS {
            return Err(GrantError::Invalid("observer grant limit reached"));
        }
        let mut random = [0u8; 48];
        getrandom::fill(&mut random).map_err(|_| GrantError::Random)?;
        let (id, secret) = random.split_at(16);
        let id = hex::encode(id);
        if grants.contains_key(&id) {
            return Err(GrantError::Random);
        }
        let token = format!("obs1_{id}_{}", hex::encode(secret));
        let info = GrantInfo {
            id: id.clone(),
            name: name.to_owned(),
            committees,
            scope: request.scope,
            lines: request.lines,
            origins: request.origins.into_iter().collect(),
            topology: request.topology,
            expires_at: now + chrono::Duration::days(i64::from(days)),
        };
        let stored = StoredGrant {
            info: info.clone(),
            digest: digest(&token),
        };
        self.save(grants.values().map(|grant| &grant.stored).chain([&stored]))?;
        grants.insert(id, ActiveGrant::new(stored));
        self.audit.record("token", "issued", Some(&info.id));
        Ok(IssuedGrant {
            url: format!("http://{}", self.address),
            token,
            grant: info,
        })
    }

    pub fn list(&self) -> Vec<GrantInfo> {
        let now = Utc::now();
        lock(&self.grants)
            .values()
            .map(|grant| &grant.stored.info)
            .filter(|info| info.expires_at > now)
            .cloned()
            .collect()
    }

    pub fn revoke(&self, id: &str) -> Result<(), GrantError> {
        let mut grants = lock(&self.grants);
        if !grants.contains_key(id) {
            return Err(GrantError::NotFound);
        }
        self.save(
            grants
                .iter()
                .filter(|(key, _)| key.as_str() != id)
                .map(|(_, grant)| &grant.stored),
        )?;
        if let Some(grant) = grants.remove(id) {
            grant.revoked.send_replace(true);
        }
        self.audit.record("token", "revoked", Some(id));
        Ok(())
    }

    fn save<'a>(&self, grants: impl Iterator<Item = &'a StoredGrant>) -> Result<(), GrantError> {
        let directory = self
            .path
            .parent()
            .ok_or(GrantError::Invalid("missing grant directory"))?;
        let mut temp = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer(&mut temp, &grants.collect::<Vec<_>>())?;
        temp.flush()?;
        temp.as_file().sync_all()?;
        temp.persist(&self.path).map_err(|err| err.error)?;
        Ok(())
    }

    pub(crate) fn authenticate(
        &self,
        credential: &str,
        origin: Option<&str>,
        stream: bool,
    ) -> Result<Authorized, AuthError> {
        let Some(id) = token_id(credential) else {
            return self.reject_unauthenticated();
        };
        let mut grants = lock(&self.grants);
        let Some(grant) = grants.get_mut(id) else {
            return self.reject_unauthenticated();
        };
        if grant.stored.info.expires_at <= Utc::now()
            || !bool::from(grant.stored.digest.ct_eq(&digest(credential)))
        {
            return self.reject_unauthenticated();
        }
        if origin.is_some_and(|origin| !grant.stored.info.origins.contains(origin)) {
            return Err(AuthError::Origin);
        }
        let bucket = if stream {
            &mut grant.streams
        } else {
            &mut grant.reads
        };
        if !bucket.take() {
            return Err(AuthError::AuthenticatedRate);
        }
        Ok(Authorized {
            info: grant.stored.info.clone(),
            revoked: grant.revoked.subscribe(),
        })
    }

    fn reject_unauthenticated(&self) -> Result<Authorized, AuthError> {
        if lock(&self.unauthenticated).take() {
            Err(AuthError::Unauthorized)
        } else {
            Err(AuthError::Rate)
        }
    }

    pub(crate) fn preflight(&self, origin: &str) -> Result<(), AuthError> {
        if !lock(&self.preflights).take() {
            return Err(AuthError::Rate);
        }
        let now = Utc::now();
        if lock(&self.grants).values().any(|grant| {
            let info = &grant.stored.info;
            info.expires_at > now && info.origins.contains(origin)
        }) {
            Ok(())
        } else {
            Err(AuthError::Origin)
        }
    }
}

impl Authorized {
    pub(crate) fn is_live(&self) -> bool {
        !*self.revoked.borrow() && self.info.expires_at > Utc::now()
    }
}

fn installation_seed(directory: &Path) -> Result<[u8; 32], GrantError> {
    let path = directory.join("observe-installation.key");
    let mut seed = [0; 32];
    match File::open(&path) {
        Ok(file) => file.take(seed.len() as u64).read_exact(&mut seed)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            getrandom::fill(&mut seed).map_err(|_| GrantError::Random)?;
            let mut temp = tempfile::NamedTempFile::new_in(directory)?;
            temp.write_all(&seed)?;
            temp.as_file().sync_all()?;
            match temp.persist_noclobber(&path) {
                Ok(_) => {}
                Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                    File::open(path)?.read_exact(&mut seed)?;
                }
                Err(error) => return Err(error.error.into()),
            }
        }
        Err(error) => return Err(error.into()),
    }
    Ok(seed)
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn token_id(token: &str) -> Option<&str> {
    let (id, secret) = token.strip_prefix("obs1_")?.split_once('_')?;
    (id.len() == 32
        && secret.len() == 64
        && id
            .bytes()
            .chain(secret.bytes())
            .all(|byte| byte.is_ascii_hexdigit()))
    .then_some(id)
}

fn valid_origin(origin: &str) -> bool {
    let Ok(url) = Url::parse(origin) else {
        return false;
    };
    if !url.username().is_empty()
        || url.password().is_some()
        || url.origin().ascii_serialization() != origin
    {
        return false;
    }
    match (url.scheme(), url.host()) {
        ("https", Some(_)) => true,
        ("http", Some(url::Host::Ipv4(ip))) => ip.is_loopback(),
        ("http", Some(url::Host::Ipv6(ip))) => ip.is_loopback(),
        _ => false,
    }
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn request() -> CreateGrant {
        CreateGrant {
            name: "test".into(),
            committees: None,
            all_committees: false,
            scope: Scope::Metadata,
            origins: vec![],
            topology: false,
            expires_in_days: None,
            lines: None,
            auto_confirmed: false,
        }
    }

    #[test]
    fn stores_hash_and_scope_and_revokes_existing_authorizations()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let address = "127.0.0.1:32111".parse()?;
        let registry = Registry::open(directory.path(), address)?;
        let issued = registry.create(request(), BTreeSet::from(["senate".parse()?]))?;
        let path = directory.path().join("observe-grants.json");
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        assert!(!fs::read_to_string(&path)?.contains(&issued.token));
        drop(registry);
        let registry = Registry::open(directory.path(), address)?;
        let authorized = registry
            .authenticate(&issued.token, None, true)
            .map_err(|_| "cannot authenticate stored grant")?;
        assert_eq!(
            authorized.info.committees,
            Some(BTreeSet::from(["senate".parse()?]))
        );
        assert!(!serde_json::to_string(&registry.list())?.contains(&issued.token));
        registry.revoke(&issued.grant.id)?;
        assert!(!authorized.is_live());
        assert!(registry.authenticate(&issued.token, None, false).is_err());
        let registry = Registry::open(directory.path(), address)?;
        assert!(registry.list().is_empty());
        assert!(
            !fs::read_to_string(directory.path().join("observe-audit.jsonl"))?
                .contains(&issued.token)
        );
        Ok(())
    }

    #[test]
    fn issuance_rejects_unknown_committees_origin_aliases_and_lines_in_metadata_slice()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let registry = Registry::open(directory.path(), "127.0.0.1:32111".parse()?)?;
        let bad_origin = |origin: &str| CreateGrant {
            origins: vec![origin.into()],
            ..request()
        };
        for origin in [
            "null",
            "*",
            "file://",
            "http://localhost:9000",
            "https://scene.example/path",
            "http://127.1",
        ] {
            assert!(
                registry
                    .create(bad_origin(origin), BTreeSet::new())
                    .is_err(),
                "{origin}"
            );
        }
        assert!(
            registry
                .create(
                    CreateGrant {
                        committees: Some(vec!["unknown".parse()?]),
                        ..request()
                    },
                    BTreeSet::new()
                )
                .is_err()
        );
        assert!(
            registry
                .create(
                    CreateGrant {
                        scope: Scope::Lines,
                        ..request()
                    },
                    BTreeSet::new()
                )
                .is_err()
        );
        let issued = registry.create(
            CreateGrant {
                all_committees: true,
                ..request()
            },
            BTreeSet::new(),
        )?;
        assert_eq!(issued.grant.committees, None);
        Ok(())
    }
}
