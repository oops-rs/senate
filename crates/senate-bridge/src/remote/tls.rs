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
        if !chain.is_empty()
            || end.len() > 2048
            || (!self.pairing
                && self
                    .service
                    .upgrade()
                    .and_then(|s| s.device(&fingerprint(end)))
                    .is_none())
        {
            return Err(Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            ));
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
