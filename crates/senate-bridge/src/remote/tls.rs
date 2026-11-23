use super::{Identity, Machine, Service, fingerprint};
use anyhow::Context;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    ClientConfig, DigitallySignedStruct, DistinguishedName, Error, RootCertStore, ServerConfig,
    SignatureScheme, client::danger::HandshakeSignatureValid,
};
use std::path::Path;
use std::sync::{Arc, Weak};

pub(super) const HOST: &str = "machine.senate.invalid";
pub(super) const PAIR: &str = "pair.senate.invalid";

fn key(
    dir: &Path,
    identity: &Identity,
) -> anyhow::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    Ok((
        vec![CertificateDer::from(
            URL_SAFE_NO_PAD.decode(&identity.cert)?,
        )],
        PrivatePkcs8KeyDer::from(identity.secret(dir)?).into(),
    ))
}

pub(super) fn client(dir: &Path, machine: &Machine) -> anyhow::Result<Arc<ClientConfig>> {
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(URL_SAFE_NO_PAD.decode(&machine.host)?))
        .context("invalid pinned host certificate")?;
    let (cert, secret) = key(dir, &machine.identity)?;
    let mut config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_root_certificates(roots)
            .with_client_auth_cert(cert, secret)?;
    config.resumption = rustls::client::Resumption::disabled();
    config.enable_early_data = false;
    Ok(Arc::new(config))
}

#[derive(Debug)]
struct Clients {
    service: Weak<Service>,
    pairing: bool,
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for Clients {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end: &CertificateDer<'_>,
        chain: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        // Enrollment is a separate SNI and endpoint. It still verifies
        // possession through CertificateVerify, then requires the one-time
        // offer. Every operational handshake requires a live enrolled pin.
        let rejected = || Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer);
        if !chain.is_empty() || end.len() > 2048 {
            return Err(rejected());
        }
        let id = fingerprint(end)?;
        if !self.pairing && self.service.upgrade().and_then(|s| s.device(&id)).is_none() {
            return Err(rejected());
        }
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("TLS 1.3 required".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        if signature.scheme != SignatureScheme::ECDSA_NISTP256_SHA256 {
            return Err(Error::General("unsupported device key".into()));
        }
        rustls::crypto::verify_tls13_signature(message, cert, signature, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ECDSA_NISTP256_SHA256]
    }
}

pub(super) fn server(service: &Arc<Service>, pairing: bool) -> anyhow::Result<Arc<ServerConfig>> {
    let provider = rustls::crypto::ring::default_provider();
    let algorithms = provider.signature_verification_algorithms;
    let (cert, secret) = key(&service.dir, &service.identity)?;
    let mut config = ServerConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(Clients {
            service: Arc::downgrade(service),
            pairing,
            algorithms,
        }))
        .with_single_cert(cert, secret)?;
    config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    config.max_early_data_size = 0;
    config.send_tls13_tickets = 0;
    Ok(Arc::new(config))
}

pub(super) fn enrollment_name(name: Option<&str>) -> anyhow::Result<bool> {
    match name {
        Some(PAIR) => Ok(true),
        Some(HOST) => Ok(false),
        _ => anyhow::bail!("unknown remote service"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::{State, audit, authority, locked, now};

    #[tokio::test]
    async fn listener_refuses_legacy_tls_missing_and_unenrolled_certificates() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let host = Identity::create(dir.path(), true)?;
        let client_identity = Identity::create(dir.path(), true)?;
        let policy = authority::Policy {
            revision: 1,
            listener: authority::Listener {
                at: "127.0.0.1:9443".parse()?,
                host_pin: fingerprint(&URL_SAFE_NO_PAD.decode(&host.cert)?.into())?,
            },
        };
        let service = Arc::new(Service {
            dir: dir.path().into(),
            at: policy.listener.at,
            identity: host,
            authority: authority::Store::isolated(dir.path()),
            policy,
            state: std::sync::Mutex::new(State {
                enabled: true,
                ..Default::default()
            }),
            changes: tokio::sync::watch::channel(()).0,
            audit: audit::Audit::open(dir.path())?,
        });

        let config = server(&service, false)?;
        let host_cert: CertificateDer<'static> =
            URL_SAFE_NO_PAD.decode(&service.identity.cert)?.into();
        let mut roots = RootCertStore::empty();
        roots.add(host_cert)?;
        let client = |version, authenticate| -> anyhow::Result<Arc<ClientConfig>> {
            let builder = ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_protocol_versions(&[version])?
            .with_root_certificates(roots.clone());
            let config = if authenticate {
                let (cert, secret) = key(dir.path(), &client_identity)?;
                builder.with_client_auth_cert(cert, secret)?
            } else {
                builder.with_no_client_auth()
            };
            Ok(Arc::new(config))
        };

        async fn accepted(
            server: Arc<ServerConfig>,
            client: Arc<ClientConfig>,
        ) -> anyhow::Result<bool> {
            let (remote, local) = tokio::io::duplex(16 * 1024);
            let name = HOST.try_into()?;
            let (_, connection) = tokio::time::timeout(std::time::Duration::from_secs(1), async {
                tokio::join!(
                    tokio_rustls::TlsConnector::from(client).connect(name, remote),
                    tokio_rustls::TlsAcceptor::from(server).accept(local)
                )
            })
            .await?;
            Ok(connection.is_ok())
        }

        assert!(!accepted(config.clone(), client(&rustls::version::TLS13, true)?).await?);
        let id = fingerprint(&URL_SAFE_NO_PAD.decode(&client_identity.cert)?.into())?;
        locked(&service.state).devices.insert(
            id.clone(),
            super::super::Device {
                id,
                grant: "a".repeat(64),
                name: "dev".into(),
                scopes: super::super::Scopes {
                    watch: false,
                    attach: false,
                },
                expires: now() + 60,
            },
        );
        assert!(!accepted(config.clone(), client(&rustls::version::TLS12, true)?).await?);
        assert!(!accepted(config.clone(), client(&rustls::version::TLS13, false)?).await?);
        assert!(accepted(config, client(&rustls::version::TLS13, true)?).await?);
        Ok(())
    }

    #[test]
    fn only_the_two_exact_server_names_select_a_role() -> anyhow::Result<()> {
        assert!(enrollment_name(Some(PAIR))?);
        assert!(!enrollment_name(Some(HOST))?);
        assert!(enrollment_name(Some("other.senate.invalid")).is_err());
        assert!(enrollment_name(None).is_err());
        Ok(())
    }
}
