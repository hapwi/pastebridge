use anyhow::Result;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{ClientConfig, ServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, Error as TlsError, SignatureScheme};
use std::sync::{Arc, RwLock};

use crate::crypto::cert_sha256;
use crate::identity::Identity;

const ALPN: &[u8] = b"pastebridge/1";

#[derive(Clone, Debug)]
pub struct PinStore {
    inner: Arc<RwLock<Vec<[u8; 32]>>>,
}

impl PinStore {
    pub fn new(pins: Vec<[u8; 32]>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(pins)),
        }
    }

    pub fn replace(&self, pins: Vec<[u8; 32]>) {
        match self.inner.write() {
            Ok(mut stored) => *stored = pins,
            Err(poisoned) => *poisoned.into_inner() = pins,
        }
    }

    pub fn contains(&self, der: &[u8]) -> bool {
        let hash = cert_sha256(der);
        match self.inner.read() {
            Ok(stored) => stored.iter().any(|pin| pin == &hash),
            Err(poisoned) => poisoned.into_inner().iter().any(|pin| pin == &hash),
        }
    }
}

pub fn server_config(
    identity: &Identity,
    pins: PinStore,
    require_client: bool,
) -> Result<ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cert = identity.cert_der()?;
    let key = identity.key_der()?;

    let mut crypto = if require_client {
        rustls::ServerConfig::builder()
            .with_client_cert_verifier(Arc::new(PinnedClientVerifier {
                pins,
                provider: provider.clone(),
            }))
            .with_single_cert(vec![cert], key)?
    } else {
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)?
    };
    crypto.alpn_protocols = vec![ALPN.to_vec()];
    crypto.max_early_data_size = 0;

    let mut server = ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(crypto)?));
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(std::time::Duration::from_secs(45).try_into()?));
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(10)));
    server.transport_config(Arc::new(transport));
    Ok(server)
}

pub fn client_config(
    identity: &Identity,
    pins: PinStore,
    skip_verify: bool,
) -> Result<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cert = identity.cert_der()?;
    let key = identity.key_der()?;

    let builder = rustls::ClientConfig::builder();
    let mut crypto = if skip_verify {
        builder
            .dangerous()
            .with_custom_certificate_verifier(SkipServerVerification::new(provider))
            .with_client_auth_cert(vec![cert], key)?
    } else {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedServerVerifier { pins, provider }))
            .with_client_auth_cert(vec![cert], key)?
    };
    crypto.alpn_protocols = vec![ALPN.to_vec()];

    Ok(ClientConfig::new(Arc::new(QuicClientConfig::try_from(
        crypto,
    )?)))
}

pub fn pairing_server(identity: &Identity) -> Result<ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cert = identity.cert_der()?;
    let key = identity.key_der()?;
    let mut crypto = rustls::ServerConfig::builder()
        .with_client_cert_verifier(Arc::new(AcceptAnyClient {
            provider: provider.clone(),
        }))
        .with_single_cert(vec![cert], key)?;
    crypto.alpn_protocols = vec![ALPN.to_vec()];
    crypto.max_early_data_size = 0;
    let mut server = ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(crypto)?));
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(std::time::Duration::from_secs(45).try_into()?));
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(10)));
    server.transport_config(Arc::new(transport));
    Ok(server)
}

pub fn pairing_client(identity: &Identity) -> Result<ClientConfig> {
    client_config(identity, PinStore::new(vec![]), true)
}

pub fn peer_cert(conn: &quinn::Connection) -> anyhow::Result<CertificateDer<'static>> {
    let ident = conn
        .peer_identity()
        .ok_or_else(|| anyhow::anyhow!("peer did not present a certificate"))?;
    let certs = ident
        .downcast_ref::<Vec<CertificateDer>>()
        .ok_or_else(|| anyhow::anyhow!("unexpected TLS identity type"))?;
    certs
        .first()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("empty certificate chain"))
}

#[derive(Debug)]
struct SkipServerVerification {
    provider: Arc<CryptoProvider>,
}

impl SkipServerVerification {
    fn new(provider: Arc<CryptoProvider>) -> Arc<Self> {
        Arc::new(Self { provider })
    }
}

impl ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[derive(Debug)]
struct PinnedServerVerifier {
    pins: PinStore,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        if self.pins.contains(end_entity.as_ref()) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(TlsError::General("unpinned server certificate".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[derive(Debug)]
struct PinnedClientVerifier {
    pins: PinStore,
    provider: Arc<CryptoProvider>,
}

impl ClientCertVerifier for PinnedClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        if self.pins.contains(end_entity.as_ref()) {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(TlsError::General("unpinned client certificate".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[derive(Debug)]
struct AcceptAnyClient {
    provider: Arc<CryptoProvider>,
}

impl ClientCertVerifier for AcceptAnyClient {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
