use pastebridge::crypto::sas_code;
use pastebridge::identity::Identity;
use pastebridge::init_crypto;

#[test]
fn matching_certs_produce_matching_codes() {
    init_crypto();
    let a = Identity::generate("alpha").unwrap();
    let b = Identity::generate("beta").unwrap();
    let da = a.cert_der().unwrap();
    let db = b.cert_der().unwrap();
    assert_eq!(
        sas_code(da.as_ref(), db.as_ref()),
        sas_code(db.as_ref(), da.as_ref())
    );
    let c = Identity::generate("gamma").unwrap();
    let dc = c.cert_der().unwrap();
    assert_ne!(
        sas_code(da.as_ref(), db.as_ref()),
        sas_code(da.as_ref(), dc.as_ref())
    );
}

#[test]
fn identity_roundtrip() {
    init_crypto();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("identity.json");
    let id = Identity::generate("roundtrip").unwrap();
    id.save(&path).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let loaded: Identity = serde_json::from_str(&text).unwrap();
    assert_eq!(id.device_id, loaded.device_id);
    assert_eq!(id.cert_der().unwrap(), loaded.cert_der().unwrap());
}
