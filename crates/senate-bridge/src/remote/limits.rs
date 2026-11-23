//! Separate admission budgets for unauthenticated traffic and enrolled work.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::locked;

pub(super) struct Limits {
    incoming: Budget<Peer>,
    reserve: Arc<Semaphore>,
    requests: Budget<String>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            incoming: Budget::new(16, 4),
            reserve: Arc::new(Semaphore::new(1)),
            requests: Budget::new(64, 8),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Peer {
    V4(Ipv4Addr),
    V6([u16; 4]),
}

impl From<IpAddr> for Peer {
    fn from(ip: IpAddr) -> Self {
        match ip.to_canonical() {
            IpAddr::V4(address) => Self::V4(address),
            IpAddr::V6(address) => {
                let [a, b, c, d, ..] = address.segments();
                Self::V6([a, b, c, d])
            }
        }
    }
}

pub(super) enum Handshake {
    Regular { _permit: Admission<Peer> },
    Pairing { _permit: OwnedSemaphorePermit },
}

impl Handshake {
    pub(super) fn pairing_only(&self) -> bool {
        matches!(self, Self::Pairing { .. })
    }
}

impl Limits {
    pub(super) fn handshake(&self, ip: IpAddr, pairing: bool) -> Option<Handshake> {
        if let Some(permit) = self.incoming.enter(ip.into()) {
            return Some(Handshake::Regular { _permit: permit });
        }
        if pairing {
            let permit = self.reserve.clone().try_acquire_owned().ok()?;
            return Some(Handshake::Pairing { _permit: permit });
        }
        None
    }

    pub(super) fn device(&self, id: String) -> Option<Admission<String>> {
        self.requests.enter(id)
    }
}

struct Budget<K> {
    slots: Arc<Semaphore>,
    counts: Arc<Mutex<BTreeMap<K, usize>>>,
    by_key: usize,
}

impl<K: Ord + Clone> Budget<K> {
    fn new(global: usize, by_key: usize) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(global)),
            counts: Arc::new(Mutex::new(BTreeMap::new())),
            by_key,
        }
    }

    fn enter(&self, key: K) -> Option<Admission<K>> {
        let slot = self.slots.clone().try_acquire_owned().ok()?;
        let mut counts = locked(&self.counts);
        let count = counts.entry(key.clone()).or_default();
        if *count == self.by_key {
            return None;
        }
        *count += 1;
        Some(Admission {
            _slot: slot,
            key,
            counts: self.counts.clone(),
        })
    }
}

pub(super) struct Admission<K: Ord> {
    _slot: OwnedSemaphorePermit,
    key: K,
    counts: Arc<Mutex<BTreeMap<K, usize>>>,
}

impl<K: Ord> Drop for Admission<K> {
    fn drop(&mut self) {
        let mut counts = locked(&self.counts);
        if let Some(count) = counts.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                counts.remove(&self.key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv6_prefixes_share_the_limit_and_pairing_has_one_reserve() -> anyhow::Result<()> {
        let limits = Limits::default();
        let first: IpAddr = "2001:db8:1:2::1".parse()?;
        let second: IpAddr = "2001:db8:1:2::2".parse()?;
        let permits: Vec<_> = (0..4)
            .filter_map(|_| limits.handshake(first, false))
            .collect();
        assert_eq!(permits.len(), 4);
        assert!(limits.handshake(second, false).is_none());
        let reserved = limits
            .handshake(second, true)
            .ok_or_else(|| anyhow::anyhow!("missing reserve"))?;
        assert!(reserved.pairing_only());
        assert!(limits.handshake(second, true).is_none());
        Ok(())
    }

    #[test]
    fn stalled_handshakes_leave_device_budget_available() {
        let limits = Limits::default();
        let source = IpAddr::from([127, 0, 0, 1]);
        let one_peer: Vec<_> = (0..4)
            .filter_map(|_| limits.handshake(source, false))
            .collect();
        assert_eq!(one_peer.len(), 4);
        assert!(limits.handshake(source, false).is_none());
        let many_peers: Vec<_> = (2..20)
            .filter_map(|i| limits.handshake(IpAddr::from([127, 0, 0, i]), false))
            .collect();
        assert_eq!(many_peers.len(), 12);
        assert!(
            limits
                .handshake(IpAddr::from([127, 0, 1, 1]), false)
                .is_none()
        );
        assert!(limits.device("enrolled".into()).is_some());
        drop(one_peer);
        assert!(limits.handshake(source, false).is_some());
    }
}

pub(super) struct AcceptBackoff {
    until: tokio::time::Instant,
}

impl Default for AcceptBackoff {
    fn default() -> Self {
        Self {
            until: tokio::time::Instant::now(),
        }
    }
}

impl AcceptBackoff {
    pub(super) fn until(&self) -> tokio::time::Instant {
        self.until
    }

    pub(super) fn accepted<T>(&mut self, result: std::io::Result<T>) -> Option<T> {
        match result {
            Ok(connection) => Some(connection),
            Err(error) => {
                tracing::warn!(kind = ?error.kind(), "remote accept failed; retrying");
                self.until = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
                None
            }
        }
    }
}

#[cfg(test)]
#[tokio::test(start_paused = true)]
async fn accept_error_recovers_after_a_bounded_delay() {
    let mut backoff = AcceptBackoff::default();
    let start = tokio::time::Instant::now();
    let error = std::io::Error::from(std::io::ErrorKind::ConnectionAborted);
    assert!(backoff.accepted::<()>(Err(error)).is_none());
    assert_eq!(backoff.until(), start + std::time::Duration::from_secs(1));
    tokio::time::sleep_until(backoff.until()).await;
    assert_eq!(backoff.accepted(Ok("connection")), Some("connection"));
}
