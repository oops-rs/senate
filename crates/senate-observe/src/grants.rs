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
    SeatIdentity {
        id: String,
        committee: CommitteeId,
    },
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
    // Local storage provenance. Grants from before app confirmation lack it.
    #[serde(default)]
    issued_with_app_consent: bool,
}

impl StoredGrant {
    fn has_valid_issuance(&self) -> bool {
        match (self.info.scope, self.info.lines) {
            (Scope::Metadata, None) => true,
            (Scope::Lines, Some(_)) => self.issued_with_app_consent,
            _ => false,
        }
    }
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
    preflights: Mutex<PreflightLimits>,
    pub(crate) audit: Arc<Audit>,
    pub(crate) projections: Arc<Projections>,
    seed: [u8; 32],
    machine: Mutex<Option<Arc<dyn MachineSource>>>,
    previews: Previews,
    cards: Cards,
    pub(crate) releases: crate::releases::Releases,
}

/// Issuance authority for the machine's verified app-confirmation dispatcher.
///
/// The machine obtains this separately at bootstrap. An ordinary registry
/// handle, including the one used by the HTTP router, cannot recover it.
pub struct AppConsentedGrants {
    registry: Arc<Registry>,
}

impl AppConsentedGrants {
    /// Issue the immutable line grant approved by the verified app dispatcher.
    pub fn create_after_app_consent(
        &self,
        grant: CreateGrant,
        readable: BTreeSet<CommitteeId>,
    ) -> Result<IssuedGrant, GrantError> {
        self.registry.create_after_app_consent(grant, readable)
    }
}

impl Registry {
    /// Open the machine registry and return a separate app issuance authority.
    /// Keep the second result inside the verified dispatch boundary.
    pub fn open_for_app_confirmation(
        directory: &Path,
        address: SocketAddr,
    ) -> Result<(Arc<Self>, AppConsentedGrants), GrantError> {
        let registry = Self::open(directory, address)?;
        let issuer = AppConsentedGrants {
            registry: registry.clone(),
        };
        Ok((registry, issuer))
    }

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
                    .filter(|grant| {
                        grant.info.expires_at > Utc::now() && grant.has_valid_issuance()
                    })
                    .map(|grant| (grant.info.id.clone(), ActiveGrant::new(grant)))
                    .collect(),
            ),
            unauthenticated: Mutex::new(Bucket::new(120, 20)),
            preflights: Mutex::new(PreflightLimits::new()),
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
        self.audit.flush_denied();
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

    #[cfg(feature = "replay")]
    pub(crate) fn invalidate_views(&self) {
        self.projections.invalidate_views();
    }

    pub fn is_watched(&self) -> bool {
        self.projections.is_watched()
    }

    pub fn notice(&self, notice: crate::LocalNotice) {
        self.projections.notice(notice);
    }

    pub fn history_id(&self, grant: &GrantInfo, native: &str) -> Result<String, GrantError> {
        self.scoped_selector(b"history", grant, native)
    }

    pub fn seat_id(&self, grant: &GrantInfo, native: &str) -> Result<String, GrantError> {
        self.scoped_selector(b"seat", grant, native)
    }

    fn scoped_selector(
        &self,
        domain: &[u8],
        grant: &GrantInfo,
        native: &str,
    ) -> Result<String, GrantError> {
        let hash = self.scoped_id(domain, format!("{}:{native}", grant.id).as_bytes())?;
        let hint = grant
            .id
            .get(..12)
            .ok_or(GrantError::Invalid("invalid grant ID"))?;
        Ok(format!("obs-{hint}-{hash}"))
    }

    pub fn selector_grants(&self, selector: &str) -> Vec<GrantInfo> {
        let Some(hint) = selector
            .strip_prefix("obs-")
            .and_then(|value| value.split_once('-'))
            .map(|(hint, _)| hint)
        else {
            return Vec::new();
        };
        self.list()
            .into_iter()
            .filter(|grant| hint.len() == 12 && grant.id.starts_with(hint))
            .collect()
    }

    pub fn approval_id(&self, grant: &GrantInfo, native: &str) -> Result<String, GrantError> {
        self.scoped_id(b"approval", format!("{}:{native}", grant.id).as_bytes())
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
        if grant.scope != Scope::Lines {
            return None;
        }
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

    /// Ordinary issuance accepts metadata grants.
    ///
    /// ```compile_fail
    /// use senate_observe::{CreateGrant, Registry};
    ///
    /// fn bypass(registry: &Registry, grant: CreateGrant) {
    ///     let _ = registry.create_after_app_consent(grant, Default::default());
    /// }
    /// ```
    pub fn create(
        &self,
        request: CreateGrant,
        readable: BTreeSet<CommitteeId>,
    ) -> Result<IssuedGrant, GrantError> {
        if request.scope == Scope::Lines {
            return Err(GrantError::Invalid(
                "lines grants require daemon-verified person confirmation",
            ));
        }
        if request.lines.is_some() {
            return Err(GrantError::Invalid(
                "metadata grants do not accept a lines policy",
            ));
        }
        self.issue(request, readable)
    }

    fn create_after_app_consent(
        &self,
        request: CreateGrant,
        readable: BTreeSet<CommitteeId>,
    ) -> Result<IssuedGrant, GrantError> {
        if request.scope != Scope::Lines || request.lines.is_none() {
            return Err(GrantError::Invalid(
                "app-confirmed issuance requires a lines grant with an explicit policy",
            ));
        }
        self.issue(request, readable)
    }

    // The two callers validate disjoint scope/policy pairs before entering.
    fn issue(
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
            issued_with_app_consent: request.scope == Scope::Lines,
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

    pub(crate) fn preflight(&self, origin: &str, peer: std::net::IpAddr) -> Result<(), AuthError> {
        let now = Utc::now();
        if !lock(&self.grants).values().any(|grant| {
            let info = &grant.stored.info;
            info.expires_at > now && info.origins.contains(origin)
        }) {
            return Err(AuthError::Origin);
        }
        let mut limits = lock(&self.preflights);
        limits
            .origins
            .retain(|_, bucket| bucket.updated.elapsed() < std::time::Duration::from_secs(60));
        let key = (origin.to_owned(), peer);
        if limits.origins.len() >= 1024 && !limits.origins.contains_key(&key) {
            return Err(AuthError::Rate);
        }
        let local = limits
            .origins
            .entry(key)
            .or_insert_with(|| Bucket::new(60, 8));
        if !local.take() || !limits.global.take() {
            return Err(AuthError::Rate);
        }
        Ok(())
    }
}

struct PreflightLimits {
    origins: BTreeMap<(String, std::net::IpAddr), Bucket>,
    global: Bucket,
}

impl PreflightLimits {
    fn new() -> Self {
        Self {
            origins: BTreeMap::new(),
            global: Bucket::new(600, 64),
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
        #[cfg(unix)]
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&fs::metadata(&path)?.permissions()) & 0o777,
            0o600
        );
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
    #[test]
    fn unlisted_origins_do_not_spend_a_browser_scenes_preflight_budget()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let registry = Registry::open(directory.path(), "127.0.0.1:45501".parse()?)?;
        let peer = "127.0.0.1".parse()?;
        for origin in ["https://scene.example", "https://other.example"] {
            registry.create(
                CreateGrant {
                    origins: vec![origin.into()],
                    ..request()
                },
                BTreeSet::new(),
            )?;
        }
        for _ in 0..200 {
            assert!(matches!(
                registry.preflight("https://unlisted.example", peer),
                Err(AuthError::Origin)
            ));
        }
        for _ in 0..8 {
            assert!(registry.preflight("https://scene.example", peer).is_ok());
        }
        assert!(matches!(
            registry.preflight("https://scene.example", peer),
            Err(AuthError::Rate)
        ));
        assert!(registry.preflight("https://other.example", peer).is_ok());
        Ok(())
    }

    #[test]
    fn a_client_claim_is_not_confirmation_for_automatic_lines()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let registry = Registry::open(directory.path(), "127.0.0.1:45501".parse()?)?;
        for policy in ["auto", "published"] {
            let raw = serde_json::json!({
                "name":"fixture", "scope":"lines", "lines":policy, "auto_confirmed":true,
                "committees":null,"all_committees":true,"origins":[],"topology":false
            });
            let requested: CreateGrant = serde_json::from_value(raw)?;
            assert!(registry.create(requested, BTreeSet::new()).is_err());
        }
        assert!(registry.list().is_empty());
        Ok(())
    }
    #[test]
    fn only_app_issued_lines_survive_restart_and_release_the_selected_preview()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let address = "127.0.0.1:32111".parse()?;
        let (registry, issuer) = Registry::open_for_app_confirmation(directory.path(), address)?;
        assert!(
            issuer
                .create_after_app_consent(request(), BTreeSet::new())
                .is_err()
        );
        assert!(
            issuer
                .create_after_app_consent(
                    CreateGrant {
                        scope: Scope::Lines,
                        ..request()
                    },
                    BTreeSet::new()
                )
                .is_err()
        );
        let committee: CommitteeId = "visible".parse()?;
        let readable = BTreeSet::from([committee.clone()]);
        let issue = |policy| {
            issuer.create_after_app_consent(
                CreateGrant {
                    scope: Scope::Lines,
                    lines: Some(policy),
                    ..request()
                },
                readable.clone(),
            )
        };
        let automatic = issue(LinePolicy::Auto)?;
        let published = issue(LinePolicy::Published)?;
        let legacy = issue(LinePolicy::Auto)?;
        let path = directory.path().join("observe-grants.json");
        let mut stored: Vec<serde_json::Value> = serde_json::from_slice(&fs::read(&path)?)?;
        for entry in &mut stored {
            if entry["info"]["id"] == legacy.grant.id {
                entry
                    .as_object_mut()
                    .ok_or("missing stored grant object")?
                    .remove("issued_with_app_consent");
            }
        }
        fs::write(path, serde_json::to_vec(&stored)?)?;
        drop(issuer);
        drop(registry);

        let registry = Registry::open(directory.path(), address)?;
        let line = |issued: &IssuedGrant| {
            registry
                .authenticate(&issued.token, None, false)
                .map(|authorization| authorization.info)
                .map_err(|_| "app-issued grant missing")
        };
        let automatic = line(&automatic)?;
        let published = line(&published)?;
        assert_eq!(automatic.committees, Some(readable));
        assert!(registry.authenticate(&legacy.token, None, false).is_err());
        let event = senate_core::NewEvent::new(
            committee,
            senate_core::Kind::Note,
            senate_core::Address::Speaker,
            "preview\nprivate body",
        )
        .into_event(Seq(1), Utc::now());
        assert_eq!(
            registry.line_for(&automatic, &event).as_deref(),
            Some("preview")
        );
        assert_eq!(registry.line_for(&published, &event), None);
        registry.previews.publish(Preview {
            committee: event.committee.clone(),
            event_id: event.id.clone(),
            line: "reviewed".into(),
        })?;
        assert_eq!(
            registry.line_for(&published, &event).as_deref(),
            Some("reviewed")
        );
        Ok(())
    }
}
