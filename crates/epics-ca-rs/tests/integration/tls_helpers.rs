//! Smoke tests for the TLS helper API. Built only with the `tls`
//! feature so default builds skip this file.
//!
//! Generates a fresh self-signed certificate per test via `rcgen`, so
//! results are deterministic across machines without checking certs
//! into the repo.

#![cfg(feature = "experimental-rust-tls")]
#![cfg(feature = "client-core")]

use epics_ca_rs::tls;
use std::io::Write;
use tempfile::NamedTempFile;

fn fresh_cert_and_key() -> (String, String) {
    let cert = rcgen::generate_simple_self_signed(vec!["ca-rs-test".into()]).expect("rcgen");
    (cert.cert.pem(), cert.key_pair.serialize_pem())
}

fn write_temp(content: &str) -> NamedTempFile {
    let mut tmp = NamedTempFile::new().expect("temp file");
    tmp.write_all(content.as_bytes()).expect("write");
    tmp
}

#[test]
fn load_certs_parses_pem() {
    let (cert_pem, _) = fresh_cert_and_key();
    let cert_file = write_temp(&cert_pem);
    let chain = tls::load_certs(cert_file.path()).expect("load certs");
    assert_eq!(chain.len(), 1);
}

#[test]
fn load_private_key_parses_pkcs8() {
    let (_, key_pem) = fresh_cert_and_key();
    let key_file = write_temp(&key_pem);
    let _key = tls::load_private_key(key_file.path()).expect("load key");
}

#[test]
fn build_server_config_succeeds() {
    let (cert_pem, key_pem) = fresh_cert_and_key();
    let cert_file = write_temp(&cert_pem);
    let key_file = write_temp(&key_pem);
    let chain = tls::load_certs(cert_file.path()).unwrap();
    let key = tls::load_private_key(key_file.path()).unwrap();
    let _cfg = tls::TlsConfig::server_from_pem(chain, key).expect("server config");
}

#[test]
fn build_client_config_with_roots_succeeds() {
    let (cert_pem, _) = fresh_cert_and_key();
    let cert_file = write_temp(&cert_pem);
    let roots = tls::load_root_store(cert_file.path()).expect("load roots");
    let _cfg = tls::TlsConfig::client_from_roots(roots);
}

#[test]
fn build_mtls_configs_succeeds() {
    let (server_cert_pem, server_key_pem) = fresh_cert_and_key();
    let (client_cert_pem, client_key_pem) = fresh_cert_and_key();

    let server_cert_file = write_temp(&server_cert_pem);
    let server_key_file = write_temp(&server_key_pem);
    let client_cert_file = write_temp(&client_cert_pem);
    let client_key_file = write_temp(&client_key_pem);

    let server_chain = tls::load_certs(server_cert_file.path()).unwrap();
    let server_key = tls::load_private_key(server_key_file.path()).unwrap();
    let client_chain = tls::load_certs(client_cert_file.path()).unwrap();
    let client_key = tls::load_private_key(client_key_file.path()).unwrap();

    // Server requires clients with certs from the client CA pool.
    let client_ca_roots = tls::load_root_store(client_cert_file.path()).unwrap();
    let _server = tls::TlsConfig::server_mtls_from_pem(server_chain, server_key, client_ca_roots)
        .expect("server mtls config");

    // Client trusts the server CA pool and presents its own cert.
    let server_ca_roots = tls::load_root_store(server_cert_file.path()).unwrap();
    let _client = tls::TlsConfig::client_mtls(server_ca_roots, client_chain, client_key)
        .expect("client mtls config");
}

#[test]
fn caclient_config_default_is_plaintext() {
    let cfg = epics_ca_rs::client::CaClientConfig::default();
    assert!(cfg.tls.is_none());
}

#[test]
fn identity_from_cert_uses_san_dns() {
    let cert =
        rcgen::generate_simple_self_signed(vec!["operator.alice.example".into()]).expect("rcgen");
    let der = cert.cert.der().clone();
    let id = epics_ca_rs::tls::identity_from_cert(&der);
    assert_eq!(id, "operator.alice.example");
}

#[test]
fn identity_from_cert_falls_back_to_fingerprint_for_unnamed() {
    // Cert with no SAN and an empty CN — rcgen normally adds CN; we
    // exercise the fingerprint fallback by handcrafting params.
    let mut params = rcgen::CertificateParams::default();
    params.distinguished_name = rcgen::DistinguishedName::new();
    let key_pair = rcgen::KeyPair::generate().expect("keypair");
    let cert = params.self_signed(&key_pair).expect("self-signed");
    let der = cert.der().clone();
    let id = epics_ca_rs::tls::identity_from_cert(&der);
    assert!(
        id.starts_with("sha256:"),
        "expected fingerprint fallback, got {id}"
    );
    assert_eq!(id.len(), 7 + 64); // "sha256:" + 32-byte hex
}
