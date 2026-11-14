#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn PublicKey____text_form____round_trips_and_rejects_junk() {
    let (_, public) = generate_key().unwrap();

    let text = public.to_string();
    let back: PublicKey = text.parse().unwrap();

    assert!(text.starts_with("ed25519:"), "{text}");
    assert_eq!(back, public);
    assert!("ed25519:AAAA".parse::<PublicKey>().is_err());
    assert!("rsa:AAAA".parse::<PublicKey>().is_err());
}

#[test]
fn PublicKey____ed25519_bytes____round_trip_through_the_der() {
    let (_, public) = generate_key().unwrap();

    let raw = public.ed25519().unwrap().to_vec();

    assert_eq!(raw.len(), 32);
    assert_eq!(PublicKey::from_ed25519(&raw), public);
    assert_eq!(PublicKey::from_der(b"not a key").ed25519(), None);
}

#[test]
fn load_key_pem____a_generated_key____its_public_key() {
    let (pem, public) = generate_key().unwrap();

    let (_, loaded) = load_key_pem(pem.as_bytes()).unwrap();

    assert_eq!(loaded, public);
    assert!(load_key_pem(b"not pem").is_err());
}

#[cfg(unix)]
#[test]
fn load_key____a_file_its_group_reads____refused_and_load_key_pem_reads_its_bytes() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("admin.key");
    let (pem, public) = generate_key().unwrap();
    std::fs::write(&path, &pem).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

    let refused = load_key(&path);
    let (_, read) = load_key_pem(&std::fs::read(&path).unwrap()).unwrap();

    assert!(refused.is_err());
    assert_eq!(read, public);
}
