//! End-to-end CA-over-TLS smoke test.
//!
//! Spawns a Rust IOC with a TLS server config and a Rust client with a
//! matching TLS client config, then exercises a basic get round-trip
//! across the encrypted virtual circuit. Built only with the `tls`
//! feature.

//!
//! `tokio_backend` as well: the IOC it stands is an `epics_ca_rs::server::
//! CaServer`. Being behind a non-default feature is why the exec-gate sweep's
//! `--all-targets` runs never compiled this file — `--all-features` does, and
//! selects the exec backend at the same time.

#![cfg(all(feature = "experimental-rust-tls", tokio_backend))]
#![cfg(feature = "client-core")]

use epics_ca_rs::client::{CaClient, CaClientConfig};
use epics_ca_rs::server::CaServer;
use epics_ca_rs::tls;

fn fresh_cert_and_key() -> (
    Vec<rustls_pki_types::CertificateDer<'static>>,
    rustls_pki_types::PrivateKeyDer<'static>,
    String, // PEM cert (for trusting on the other side)
) {
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).expect("rcgen");
    let cert_pem = cert.cert.pem();
    let key_pem = cert.key_pair.serialize_pem();

    use std::io::Write;
    let mut cert_file = tempfile::NamedTempFile::new().unwrap();
    cert_file.write_all(cert_pem.as_bytes()).unwrap();
    let mut key_file = tempfile::NamedTempFile::new().unwrap();
    key_file.write_all(key_pem.as_bytes()).unwrap();

    let chain = tls::load_certs(cert_file.path()).unwrap();
    let key = tls::load_private_key(key_file.path()).unwrap();
    (chain, key, cert_pem)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rust_client_tls_to_rust_server() {
    // Generate one self-signed cert; the server presents it, the client
    // trusts it. With CN=127.0.0.1, rustls will accept the connection
    // when we connect by IP.
    let (chain, key, cert_pem) = fresh_cert_and_key();
    let server_tls = tls::TlsConfig::server_from_pem(chain, key).expect("server cfg");

    // Build a roots store containing just our self-signed cert.
    let mut cert_file = tempfile::NamedTempFile::new().unwrap();
    use std::io::Write;
    cert_file.write_all(cert_pem.as_bytes()).unwrap();
    let roots = tls::load_root_store(cert_file.path()).unwrap();
    let client_tls = tls::TlsConfig::client_from_roots(roots);

    // Bring up the IOC with TLS enabled on a kernel-assigned port.
    let server = CaServer::builder()
        .pv("TLS:VAL", epics_base_rs::types::EpicsValue::Long(7777))
        .port(0)
        .with_tls(server_tls)
        .build()
        .await
        .expect("build");

    // `build()` bound the listener, so the server is accepting TLS
    // connections already — nothing to wait for.
    let port = server.udp_port();
    let server_arc = std::sync::Arc::new(server);
    let server_clone = server_arc.clone();
    let server_task = tokio::spawn(async move {
        let _ = server_clone.run().await;
    });

    // Set up the client to talk to that port over TLS.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", "127.0.0.1");
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }

    let client = CaClient::new_with_config(CaClientConfig {
        tls: Some(client_tls),
        ..Default::default()
    })
    .await
    .expect("client");

    let ch = client.create_channel("TLS:VAL");
    ch.wait_connected(budget::FACT_BUDGET)
        .await
        .expect("connect over TLS");

    let (_, value) = ch
        .get_with_timeout(budget::FACT_BUDGET)
        .await
        .expect("read over TLS");
    assert_eq!(value.to_f64().unwrap_or(0.0) as i64, 7777);

    // Don't tear down via drop ordering: just abort the server task.
    server_task.abort();
}

#[path = "common/budget.rs"]
mod budget;
