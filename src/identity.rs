use anyhow::{Context, Result};
use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use time::OffsetDateTime;

use crate::config::{read_private_file, write_secret_file, Paths};
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
            let text = read_private_file(&paths.identity_file)?;
            let id: Identity = serde_json::from_str(&text)?;
            id.validate()?;
            return Ok(id);
        }
        let id = Self::generate(name)?;
        id.save(&paths.identity_file)?;
        Ok(id)
    }

    pub fn generate(name: &str) -> Result<Self> {
        validate_device_name(name)?;
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

    fn validate(&self) -> Result<()> {
        let cert = self.cert_der()?;
        let expected_id = device_id_from_cert(cert.as_ref());
        if self.device_id != expected_id {
            anyhow::bail!("identity device_id does not match its certificate");
        }
        validate_device_name(&self.name)
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

    fn validate(&self) -> Result<()> {
        let cert = self.cert_der()?;
        let expected_id = device_id_from_cert(cert.as_ref());
        let expected_fingerprint = cert_fingerprint(cert.as_ref());
        if self.device_id != expected_id {
            anyhow::bail!("peer {} has a mismatched device_id", self.name);
        }
        if self.cert_sha256 != expected_fingerprint {
            anyhow::bail!(
                "peer {} has a mismatched certificate fingerprint",
                self.name
            );
        }
        validate_device_name(&self.name)
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
        let text = read_private_file(&paths.peers_file)?;
        let list: Self = serde_json::from_str(&text).context("parsing peers.json")?;
        let mut ids = HashSet::with_capacity(list.peers.len());
        for peer in &list.peers {
            peer.validate()?;
            if !ids.insert(&peer.device_id) {
                anyhow::bail!("duplicate peer {}", peer.device_id);
            }
        }
        Ok(list)
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
    CertificateDer::from_pem_slice(pem.as_bytes()).context("parsing certificate pem")
}

fn pem_to_key(pem: &str) -> Result<PrivateKeyDer<'static>> {
    PrivateKeyDer::from_pem_slice(pem.as_bytes()).context("parsing private key pem")
}

fn validate_device_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
        anyhow::bail!("invalid device name");
    }
    Ok(())
}
