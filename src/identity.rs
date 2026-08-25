use anyhow::{Context, Result};
use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use serde::{Deserialize, Serialize};
use std::path::Path;
use time::OffsetDateTime;

use crate::config::{write_secret_file, Paths};
use crate::crypto::{cert_fingerprint, cert_sha256, device_id_from_cert};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub device_id: String,
    pub name: String,
    pub cert_pem: String,
    pub key_pem: String,
}

impl Identity {
    pub fn load_or_create(paths: &Paths, name: &str) -> Result<Self> {
        if paths.identity_file.exists() {
            let text = std::fs::read_to_string(&paths.identity_file)?;
            let id: Identity = serde_json::from_str(&text)?;
            return Ok(id);
        }
        let id = Self::generate(name)?;
        id.save(&paths.identity_file)?;
        Ok(id)
    }

    pub fn generate(name: &str) -> Result<Self> {
        let key_pair = KeyPair::generate()?;
        let mut params = CertificateParams::new(vec!["pastebridge".to_string()])?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, name);
        params.distinguished_name = dn;
        params.is_ca = IsCa::NoCa;
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        params.not_before = OffsetDateTime::now_utc() - time::Duration::days(1);
        params.not_after = OffsetDateTime::now_utc() + time::Duration::days(3650);

        let cert = params.self_signed(&key_pair)?;
        let cert_pem = cert.pem();
        let key_pem = key_pair.serialize_pem();
        let cert_der = cert.der();
        Ok(Self {
            device_id: device_id_from_cert(cert_der),
            name: name.to_string(),
            cert_pem,
            key_pem,
        })
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_secret_file(path, &serde_json::to_string_pretty(self)?)
    }

    pub fn cert_der(&self) -> Result<CertificateDer<'static>> {
        pem_to_cert(&self.cert_pem)
    }

    pub fn key_der(&self) -> Result<PrivateKeyDer<'static>> {
        pem_to_key(&self.key_pem)
    }

    pub fn fingerprint(&self) -> Result<String> {
        Ok(cert_fingerprint(self.cert_der()?.as_ref()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    pub device_id: String,
    pub name: String,
    pub cert_pem: String,
    pub cert_sha256: String,
    pub last_addr: Option<String>,
    pub paired_at: String,
}

impl Peer {
    pub fn cert_der(&self) -> Result<CertificateDer<'static>> {
        pem_to_cert(&self.cert_pem)
    }

    pub fn pin(&self) -> Result<[u8; 32]> {
        Ok(cert_sha256(self.cert_der()?.as_ref()))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeerList {
    pub peers: Vec<Peer>,
}

impl PeerList {
    pub fn load(paths: &Paths) -> Result<Self> {
        if !paths.peers_file.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&paths.peers_file)?;
        serde_json::from_str(&text).context("parsing peers.json")
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        write_secret_file(&paths.peers_file, &serde_json::to_string_pretty(self)?)
    }

    pub fn get(&self, device_id: &str) -> Option<&Peer> {
        self.peers.iter().find(|p| p.device_id == device_id)
    }

    pub fn upsert(&mut self, peer: Peer) {
        if let Some(existing) = self
            .peers
            .iter_mut()
            .find(|p| p.device_id == peer.device_id)
        {
            *existing = peer;
        } else {
            self.peers.push(peer);
        }
    }

    pub fn remove(&mut self, device_id: &str) -> bool {
        let n = self.peers.len();
        self.peers.retain(|p| p.device_id != device_id);
        self.peers.len() != n
    }

    pub fn pins(&self) -> Result<Vec<[u8; 32]>> {
        self.peers.iter().map(|p| p.pin()).collect()
    }
}

fn pem_to_cert(pem: &str) -> Result<CertificateDer<'static>> {
    use rustls_pki_types::pem::PemObject;
    CertificateDer::from_pem_slice(pem.as_bytes()).context("parsing certificate pem")
}

fn pem_to_key(pem: &str) -> Result<PrivateKeyDer<'static>> {
    use rustls_pki_types::pem::PemObject;
    PrivateKeyDer::from_pem_slice(pem.as_bytes()).context("parsing private key pem")
}
