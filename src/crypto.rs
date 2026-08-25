use sha2::{Digest, Sha256};

pub fn cert_sha256(der: &[u8]) -> [u8; 32] {
    Sha256::digest(der).into()
}

pub fn cert_fingerprint(der: &[u8]) -> String {
    hex::encode(cert_sha256(der))
}

pub fn device_id_from_cert(der: &[u8]) -> String {
    hex::encode(&cert_sha256(der)[..8])
}

/// 8-digit comparison code bound to both TLS certificates.
/// A MITM terminates TLS with different certs, so the codes will not match.
pub fn sas_code(cert_a: &[u8], cert_b: &[u8]) -> String {
    let ha = cert_sha256(cert_a);
    let hb = cert_sha256(cert_b);
    let (first, second) = if ha <= hb { (ha, hb) } else { (hb, ha) };
    let mut h = Sha256::new();
    h.update(b"pastebridge-sas-v1");
    h.update(first);
    h.update(second);
    let out = h.finalize();
    let n = u32::from_be_bytes(out[0..4].try_into().unwrap()) % 100_000_000;
    format!("{:04} {:04}", n / 10_000, n % 10_000)
}

pub fn clip_hash(mime: &str, bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(mime.as_bytes());
    h.update([0]);
    h.update(bytes);
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sas_is_order_independent() {
        let a = b"cert-one";
        let b = b"cert-two";
        assert_eq!(sas_code(a, b), sas_code(b, a));
        assert_ne!(sas_code(a, b), sas_code(a, b"other"));
    }

    #[test]
    fn sas_is_eight_digits() {
        let code = sas_code(b"a", b"b");
        assert_eq!(code.len(), 9);
        assert_eq!(code.chars().filter(|c| c.is_ascii_digit()).count(), 8);
    }

    #[test]
    fn clip_hash_changes_with_bytes() {
        assert_ne!(clip_hash("text/plain", b"a"), clip_hash("text/plain", b"b"));
        assert_ne!(clip_hash("text/plain", b"a"), clip_hash("image/png", b"a"));
    }
}
