use std::{fs, path::Path, time::Duration};

use open_tf_mirror::tls_reload::ReloadingCertResolver;
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::sign::CertifiedKey as RustlsCertifiedKey;

fn write_cert_pair(dir: &Path, name: &str) -> Vec<u8> {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec![format!("{name}.example.test")]).unwrap();
    let cert_pem = cert.pem();
    let key_pem = signing_key.serialize_pem();

    fs::write(dir.join(format!("{name}.crt")), cert_pem).unwrap();
    fs::write(dir.join(format!("{name}.key")), key_pem).unwrap();

    cert.der().to_vec()
}

fn leaf_cert_der(certified_key: &RustlsCertifiedKey) -> Vec<u8> {
    certified_key.cert[0].as_ref().to_vec()
}

#[test]
fn new_rejects_malformed_certificate_or_key() {
    let tmp = tempfile::tempdir().unwrap();
    write_cert_pair(tmp.path(), "valid");

    let bad_cert = tmp.path().join("bad.crt");
    let bad_key = tmp.path().join("bad.key");
    fs::write(
        &bad_cert,
        "-----BEGIN CERTIFICATE-----\nnot base64\n-----END CERTIFICATE-----\n",
    )
    .unwrap();
    fs::write(
        &bad_key,
        "-----BEGIN PRIVATE KEY-----\nnot base64\n-----END PRIVATE KEY-----\n",
    )
    .unwrap();

    assert!(
        ReloadingCertResolver::new(&bad_cert, tmp.path().join("valid.key")).is_err(),
        "malformed certificate should fail resolver construction"
    );
    assert!(
        ReloadingCertResolver::new(tmp.path().join("valid.crt"), &bad_key).is_err(),
        "malformed private key should fail resolver construction"
    );
}

#[test]
fn reloading_cert_files_changes_future_resolved_certificate() {
    let tmp = tempfile::tempdir().unwrap();
    let first_cert = write_cert_pair(tmp.path(), "first");
    let second_cert = write_cert_pair(tmp.path(), "second");

    let live_cert = tmp.path().join("live.crt");
    let live_key = tmp.path().join("live.key");
    fs::copy(tmp.path().join("first.crt"), &live_cert).unwrap();
    fs::copy(tmp.path().join("first.key"), &live_key).unwrap();

    let resolver =
        ReloadingCertResolver::new_with_reload_interval(&live_cert, &live_key, Duration::ZERO)
            .unwrap();
    let initially_resolved = resolver.resolve_current_cert();
    assert_eq!(leaf_cert_der(&initially_resolved), first_cert);

    fs::copy(tmp.path().join("second.crt"), &live_cert).unwrap();
    fs::copy(tmp.path().join("second.key"), &live_key).unwrap();

    let reloaded = resolver.resolve_current_cert();
    assert_eq!(leaf_cert_der(&reloaded), second_cert);
    assert_ne!(leaf_cert_der(&reloaded), leaf_cert_der(&initially_resolved));
}

#[test]
fn resolver_uses_cached_certificate_within_reload_interval() {
    let tmp = tempfile::tempdir().unwrap();
    let first_cert = write_cert_pair(tmp.path(), "first");
    write_cert_pair(tmp.path(), "second");

    let live_cert = tmp.path().join("live.crt");
    let live_key = tmp.path().join("live.key");
    fs::copy(tmp.path().join("first.crt"), &live_cert).unwrap();
    fs::copy(tmp.path().join("first.key"), &live_key).unwrap();

    let resolver = ReloadingCertResolver::new_with_reload_interval(
        &live_cert,
        &live_key,
        Duration::from_secs(60),
    )
    .unwrap();
    let initially_resolved = resolver.resolve_current_cert();
    assert_eq!(leaf_cert_der(&initially_resolved), first_cert);

    fs::copy(tmp.path().join("second.crt"), &live_cert).unwrap();
    fs::copy(tmp.path().join("second.key"), &live_key).unwrap();

    let cached = resolver.resolve_current_cert();
    assert_eq!(leaf_cert_der(&cached), first_cert);
}

#[test]
fn resolver_falls_back_to_last_good_certificate_when_reload_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let first_cert = write_cert_pair(tmp.path(), "first");

    let live_cert = tmp.path().join("live.crt");
    let live_key = tmp.path().join("live.key");
    fs::copy(tmp.path().join("first.crt"), &live_cert).unwrap();
    fs::copy(tmp.path().join("first.key"), &live_key).unwrap();

    let resolver =
        ReloadingCertResolver::new_with_reload_interval(&live_cert, &live_key, Duration::ZERO)
            .unwrap();
    let initially_resolved = resolver.resolve_current_cert();
    assert_eq!(leaf_cert_der(&initially_resolved), first_cert);

    fs::write(&live_cert, "not a certificate").unwrap();

    let fallback = resolver.resolve_current_cert();
    assert_eq!(leaf_cert_der(&fallback), first_cert);
}
