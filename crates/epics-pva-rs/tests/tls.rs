//! TLS end-to-end test.
//!
//! Generates a self-signed certificate at runtime, spins up a server that
//! accepts TLS-only, then connects a client that trusts that exact cert
//! and performs a GET. Confirms that the TCP-over-TLS plumbing in
//! `client_native::server_conn::connect_tls` and the `tokio_rustls`
//! acceptor in `server_native::tcp` actually shake hands and exchange
//! frames.
//!
//! Compiled only with the `tls` feature (ON by default) — without it the
//! crate links no rustls, so there is nothing here to exercise.

#![cfg(tokio_backend)]
#![cfg(feature = "tls")]
#![allow(clippy::manual_async_fn)]
#![cfg(feature = "client")]

use epics_pva_rs::server_native::MonitorStream;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::sync::Mutex;

use epics_pva_rs::auth::{TlsClientConfig, TlsServerConfig};
use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::PvaServer;
use epics_pva_rs::server_native::{ChannelSource, OpError, PvaServerConfig};

/// Server-side hook invoked once a peer's credentials are resolved.
type AuthCompleteHook = Arc<
    dyn Fn(std::net::SocketAddr, &epics_pva_rs::server_native::tcp::ClientCredentials)
        + Send
        + Sync,
>;

// Generate a self-signed cert + matching key pair for tests.
fn generate_self_signed() -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()])
        .expect("self-signed cert");
    let cert_der = CertificateDer::from(cert.cert.der().to_vec());
    let key_der: PrivateKeyDer<'static> =
        PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()).into();
    (cert_der, key_der)
}

// Build a self-signed root CA with the given CommonName.
fn make_ca(cn: &str) -> (rcgen::Certificate, rcgen::KeyPair) {
    let mut params = rcgen::CertificateParams::new(Vec::new()).expect("ca params");
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, cn);
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let key = rcgen::KeyPair::generate().expect("ca key");
    let cert = params.self_signed(&key).expect("ca self-signed");
    (cert, key)
}

// Build a leaf cert (CN + 127.0.0.1 SAN) signed by `ca`.
fn make_leaf(
    cn: &str,
    ca: &rcgen::Certificate,
    ca_key: &rcgen::KeyPair,
) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let mut params =
        rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()]).expect("leaf params");
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, cn);
    params.is_ca = rcgen::IsCa::ExplicitNoCa;
    let key = rcgen::KeyPair::generate().expect("leaf key");
    let cert = params.signed_by(&key, ca, ca_key).expect("leaf signed");
    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der: PrivateKeyDer<'static> = PrivatePkcs8KeyDer::from(key.serialize_der()).into();
    (cert_der, key_der)
}

#[derive(Clone)]
struct StaticSource {
    inner: Arc<Mutex<std::collections::HashMap<String, PvField>>>,
}

impl StaticSource {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }
    async fn put(&self, name: &str, value: f64) {
        let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
        s.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(value))));
        self.inner
            .lock()
            .await
            .insert(name.to_string(), PvField::Structure(s));
    }
}

fn nt_scalar_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "epics:nt/NTScalar:1.0".into(),
        fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
    }
}

impl ChannelSource for StaticSource {
    fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
        let inner = self.inner.clone();
        async move { inner.lock().await.keys().cloned().collect::<Vec<_>>() }
    }
    fn has_pv(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
        let inner = self.inner.clone();
        let n = name.to_string();
        async move { inner.lock().await.contains_key(&n) }
    }
    fn get_introspection(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
        let inner = self.inner.clone();
        let n = name.to_string();
        async move {
            if inner.lock().await.contains_key(&n) {
                Some(nt_scalar_desc())
            } else {
                None
            }
        }
    }
    fn get_value(&self, name: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
        let inner = self.inner.clone();
        let n = name.to_string();
        async move { inner.lock().await.get(&n).cloned() }
    }
    fn put_value(
        &self,
        _name: &str,
        _value: PvField,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        async { Ok(()) }
    }
    fn is_writable(&self, _name: &str) -> impl std::future::Future<Output = bool> + Send {
        async { false }
    }
    fn subscribe(
        &self,
        _name: &str,
    ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send {
        async { None }
    }
}

#[tokio::test]
async fn tls_client_to_tls_server_full_handshake() {
    // Reseed the global rustls crypto provider with ring (otherwise
    // ServerConfig::builder() panics on default-features=false rustls).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let (cert, key) = generate_self_signed();

    // Build server-side TLS config (no client-cert auth).
    let server_cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key)
        .expect("server tls config");
    let server_tls = Arc::new(TlsServerConfig {
        config: Arc::new(server_cfg),
        require_client_cert: false,
        trust_roots: Arc::new(RootCertStore::empty()),
    });

    // Build client-side TLS config trusting the server cert.
    let mut roots = RootCertStore::empty();
    roots.add(cert).unwrap();
    let client_cfg = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let client_tls = Arc::new(TlsClientConfig {
        config: Arc::new(client_cfg),
    });

    // Server source.
    let source = Arc::new(StaticSource::new());
    source.put("TLS:PV", 12.5).await;

    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: 0,
        tls_port: 0,
        idle_timeout: Duration::from_secs(60),
        max_connections: 16,
        max_channels_per_connection: 64,
        monitor_queue_depth: 8,
        tls: Some(server_tls),
        ..Default::default()
    };
    let server = PvaServer::start(source, cfg).expect("server start");
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Client targeting the TLS server explicitly.
    let server_addr = server.tcp_addr();
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .with_tls(client_tls)
        .build();

    let v = tokio::time::timeout(Duration::from_secs(5), client.pvget("TLS:PV"))
        .await
        .expect("pvget timed out")
        .expect("pvget failed");
    match v {
        PvField::Structure(s) => {
            assert_eq!(s.struct_id, "epics:nt/NTScalar:1.0");
            assert!(matches!(
                s.get_value(),
                Some(ScalarValue::Double(d)) if (d - 12.5).abs() < 1e-9
            ));
        }
        other => panic!("expected NTScalar structure, got {other:?}"),
    }

    server.stop();
}

/// A TLS-enabled server must bind a DEDICATED TLS listener on its own
/// port (default `EPICS_PVAS_TLS_PORT` 5076 / ephemeral when 0),
/// distinct from the plaintext `tcp_port`, and a client addressed
/// straight at that port must complete a TLS handshake + GET. pvxs binds
/// a separate per-interface TLS socket on `effective.tls_port`
/// (src/server.cpp:580-594, `tls` `b3a10bf0`) and advertises it for protoTLS
/// SEARCH (src/server.cpp:835-844). Pre-fix the Rust server had no
/// `tls_port` at all — EPICS_PVAS_TLS_PORT was ignored and TLS was
/// reachable only on the plaintext port.
#[tokio::test]
async fn tls_server_binds_dedicated_tls_port_distinct_from_plaintext() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let (cert, key) = generate_self_signed();
    let server_cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key)
        .expect("server tls config");
    let server_tls = Arc::new(TlsServerConfig {
        config: Arc::new(server_cfg),
        require_client_cert: false,
        trust_roots: Arc::new(RootCertStore::empty()),
    });

    let mut roots = RootCertStore::empty();
    roots.add(cert).unwrap();
    let client_cfg = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let client_tls = Arc::new(TlsClientConfig {
        config: Arc::new(client_cfg),
    });

    let source = Arc::new(StaticSource::new());
    source.put("TLS:PV", 7.5).await;

    // Ephemeral plaintext AND TLS ports (both 0 via `isolated()`): the
    // runtime must bind two DISTINCT TCP listeners and stamp both back.
    let cfg = PvaServerConfig {
        tls: Some(server_tls),
        ..PvaServerConfig::isolated()
    };
    let server = epics_pva_rs::server_native::PvaServer::start(source, cfg).expect("server start");

    let report = server.report();
    assert!(report.tls_enabled, "TLS must be enabled");
    assert_ne!(report.tls_port, 0, "a dedicated TLS port must be bound");
    assert_ne!(
        report.tls_port, report.tcp_port,
        "the TLS listener must bind a port distinct from the plaintext port"
    );

    // The dedicated TLS endpoint must actually serve TLS: connect straight
    // at tls_addr() (NOT the plaintext tcp port) and GET.
    let tls_addr = server.tls_addr().expect("tls_addr present when TLS on");
    assert_eq!(
        tls_addr.port(),
        report.tls_port,
        "tls_addr() must report the bound TLS port"
    );
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(tls_addr)
        .with_tls(client_tls)
        .build();
    let v = tokio::time::timeout(Duration::from_secs(5), client.pvget("TLS:PV"))
        .await
        .expect("pvget timed out")
        .expect("pvget over the dedicated TLS port failed");
    match v {
        PvField::Structure(s) => assert!(matches!(
            s.get_value(),
            Some(ScalarValue::Double(d)) if (d - 7.5).abs() < 1e-9
        )),
        other => panic!("expected NTScalar structure, got {other:?}"),
    }

    server.stop();
}

/// With `disable_plaintext` set the server must REFUSE a plaintext peer
/// (anti-downgrade) while still serving a TLS client. pvxs carries
/// `tls_disable_plaintext` in the shared config and refuses the downgrade
/// client-side (it drops a plaintext SEARCH reply, src/client.cpp:944); the
/// Rust server unifies TLS + plaintext on each listener via the
/// first-byte peek, so the server-side guarantee is to drop the non-TLS
/// peer at accept time. Pre-fix the option was never parsed and a plain
/// client was always served.
#[tokio::test]
async fn disable_plaintext_server_refuses_plain_but_serves_tls() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let (cert, key) = generate_self_signed();
    let server_cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key)
        .expect("server tls config");
    let server_tls = Arc::new(TlsServerConfig {
        config: Arc::new(server_cfg),
        require_client_cert: false,
        trust_roots: Arc::new(RootCertStore::empty()),
    });

    let mut roots = RootCertStore::empty();
    roots.add(cert).unwrap();
    let client_cfg = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let client_tls = Arc::new(TlsClientConfig {
        config: Arc::new(client_cfg),
    });

    let source = Arc::new(StaticSource::new());
    source.put("SECURE:PV", 3.25).await;

    let cfg = PvaServerConfig {
        tls: Some(server_tls),
        disable_plaintext: true,
        ..PvaServerConfig::isolated()
    };
    let server = epics_pva_rs::server_native::PvaServer::start(source, cfg).expect("server start");

    // A plaintext client (no TLS) must be refused: the server drops the
    // connection after the first-byte peek instead of serving plain PVA.
    let plain = PvaClient::builder()
        .timeout(Duration::from_millis(800))
        .server_addr(server.tcp_addr())
        .build();
    match tokio::time::timeout(Duration::from_secs(3), plain.pvget("SECURE:PV")).await {
        Ok(Err(_)) | Err(_) => {} // pvget errored / timed out — refused
        Ok(Ok(v)) => panic!("plaintext GET must be refused under disable_plaintext, got {v:?}"),
    }

    // A TLS client on the dedicated TLS port is still served.
    let tls_addr = server.tls_addr().expect("tls_addr present when TLS on");
    let secure = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(tls_addr)
        .with_tls(client_tls)
        .build();
    let v = tokio::time::timeout(Duration::from_secs(5), secure.pvget("SECURE:PV"))
        .await
        .expect("tls pvget timed out")
        .expect("tls pvget failed");
    match v {
        PvField::Structure(s) => assert!(matches!(
            s.get_value(),
            Some(ScalarValue::Double(d)) if (d - 3.25).abs() < 1e-9
        )),
        other => panic!("expected NTScalar structure, got {other:?}"),
    }

    server.stop();
}

/// a client presenting an X.509 client certificate over mTLS must
/// have its connection credentials populated with `method = "x509"`,
/// `account` = the client leaf cert's subject CommonName, and
/// `authority` = the root CA's CommonName. Mirrors pvxs
/// `SSLContext::fill_credentials`.
#[tokio::test]
async fn mtls_client_cert_populates_x509_credentials() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    // One root CA signs both the server and the client leaf certs.
    let (ca_cert, ca_key) = make_ca("EPICS Test Root CA");
    let ca_der = CertificateDer::from(ca_cert.der().to_vec());
    let (server_cert, server_key) = make_leaf("pva-test-server", &ca_cert, &ca_key);
    let (client_cert, client_key) = make_leaf("operator-bob", &ca_cert, &ca_key);

    // Server: present the server leaf chain (leaf + root) and require a
    // client cert verified against the CA.
    let mut client_ca_roots = RootCertStore::empty();
    client_ca_roots.add(ca_der.clone()).unwrap();
    let client_verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(client_ca_roots))
        .build()
        .expect("client verifier");
    let server_cfg = ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(vec![server_cert.clone(), ca_der.clone()], server_key)
        .expect("server tls config");
    let server_tls = Arc::new(TlsServerConfig {
        config: Arc::new(server_cfg),
        require_client_cert: true,
        trust_roots: Arc::new(RootCertStore::empty()),
    });

    // Client: trust the CA, present its own leaf chain (leaf + root).
    let mut server_ca_roots = RootCertStore::empty();
    server_ca_roots.add(ca_der.clone()).unwrap();
    let client_cfg = ClientConfig::builder()
        .with_root_certificates(server_ca_roots)
        .with_client_auth_cert(vec![client_cert, ca_der.clone()], client_key)
        .expect("client tls config");
    let client_tls = Arc::new(TlsClientConfig {
        config: Arc::new(client_cfg),
    });

    let source = Arc::new(StaticSource::new());
    source.put("MTLS:PV", 7.0).await;

    // `auth_complete` hook captures the credentials the server derived
    // for the connecting peer.
    let captured: Arc<Mutex<Option<(String, String, String)>>> = Arc::new(Mutex::new(None));
    let captured_hook = captured.clone();
    let auth_complete: AuthCompleteHook = Arc::new(move |_peer, cred| {
        if let Ok(mut g) = captured_hook.try_lock() {
            *g = Some((
                cred.method.clone(),
                cred.account.clone(),
                cred.authority.clone(),
            ));
        }
    });

    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: 0,
        tls_port: 0,
        idle_timeout: Duration::from_secs(60),
        max_connections: 16,
        max_channels_per_connection: 64,
        monitor_queue_depth: 8,
        tls: Some(server_tls),
        auth_complete: Some(auth_complete),
        ..Default::default()
    };
    let server = PvaServer::start(source, cfg).expect("server start");
    tokio::time::sleep(Duration::from_millis(100)).await;

    let server_addr = server.tcp_addr();
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .with_tls(client_tls)
        .build();

    let v = tokio::time::timeout(Duration::from_secs(5), client.pvget("MTLS:PV"))
        .await
        .expect("pvget timed out")
        .expect("pvget failed");
    match v {
        PvField::Structure(s) => {
            assert_eq!(s.struct_id, "epics:nt/NTScalar:1.0");
            assert!(matches!(
                s.get_value(),
                Some(ScalarValue::Double(d)) if (d - 7.0).abs() < 1e-9
            ));
        }
        other => panic!("expected NTScalar structure, got {other:?}"),
    }

    // The server must have mapped the client cert to x509 credentials.
    let creds = captured
        .lock()
        .await
        .clone()
        .expect("auth_complete hook should have fired");
    assert_eq!(creds.0, "x509", "method must be x509 for mTLS peer");
    assert_eq!(
        creds.1, "operator-bob",
        "account must be the client leaf cert CommonName"
    );
    assert_eq!(
        creds.2, "EPICS Test Root CA",
        "authority must be the root CA CommonName"
    );

    server.stop();
}

/// when the client sends only the leaf cert (no root CA in the chain),
/// the server must still populate `authority` by walking its trust store.
/// This mirrors pvxs `SSL_get0_verified_chain` which appends the root from
/// the trust store even when the peer omits it.
#[tokio::test]
async fn mtls_partial_chain_authority_resolved_from_trust_roots() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let (ca_cert, ca_key) = make_ca("EPICS Test Root CA");
    let ca_der = CertificateDer::from(ca_cert.der().to_vec());
    let (server_cert, server_key) = make_leaf("pva-test-server", &ca_cert, &ca_key);
    let (client_cert, client_key) = make_leaf("operator-alice", &ca_cert, &ca_key);

    // Server trust store for client verification.
    let mut client_ca_roots = RootCertStore::empty();
    client_ca_roots.add(ca_der.clone()).unwrap();
    let trust_roots = Arc::new(client_ca_roots);

    let client_verifier = rustls::server::WebPkiClientVerifier::builder(trust_roots.clone())
        .build()
        .expect("client verifier");
    let server_cfg = ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(vec![server_cert.clone(), ca_der.clone()], server_key)
        .expect("server tls config");
    let server_tls = Arc::new(TlsServerConfig {
        config: Arc::new(server_cfg),
        require_client_cert: true,
        trust_roots,
    });

    // Client sends only [leaf] — NO root CA in the chain (partial-chain scenario).
    let mut server_ca_roots = RootCertStore::empty();
    server_ca_roots.add(ca_der.clone()).unwrap();
    let client_cfg = ClientConfig::builder()
        .with_root_certificates(server_ca_roots)
        .with_client_auth_cert(vec![client_cert], client_key)
        .expect("client tls config");
    let client_tls = Arc::new(TlsClientConfig {
        config: Arc::new(client_cfg),
    });

    let source = Arc::new(StaticSource::new());
    source.put("R68:PV", 99.0).await;

    let captured: Arc<Mutex<Option<(String, String, String)>>> = Arc::new(Mutex::new(None));
    let captured_hook = captured.clone();
    let auth_complete: AuthCompleteHook = Arc::new(move |_peer, cred| {
        if let Ok(mut g) = captured_hook.try_lock() {
            *g = Some((
                cred.method.clone(),
                cred.account.clone(),
                cred.authority.clone(),
            ));
        }
    });

    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: 0,
        tls_port: 0,
        idle_timeout: Duration::from_secs(60),
        max_connections: 16,
        max_channels_per_connection: 64,
        monitor_queue_depth: 8,
        tls: Some(server_tls),
        auth_complete: Some(auth_complete),
        ..Default::default()
    };
    let server = PvaServer::start(source, cfg).expect("server start");
    tokio::time::sleep(Duration::from_millis(100)).await;

    let server_addr = server.tcp_addr();
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .with_tls(client_tls)
        .build();

    let v = tokio::time::timeout(Duration::from_secs(5), client.pvget("R68:PV"))
        .await
        .expect("pvget timed out")
        .expect("pvget failed");
    match v {
        PvField::Structure(s) => {
            assert!(matches!(
                s.get_value(),
                Some(ScalarValue::Double(d)) if (d - 99.0).abs() < 1e-9
            ));
        }
        other => panic!("expected NTScalar structure, got {other:?}"),
    }

    let creds = captured
        .lock()
        .await
        .clone()
        .expect("auth_complete hook should have fired");
    assert_eq!(creds.0, "x509", "method must be x509 for mTLS peer");
    assert_eq!(creds.1, "operator-alice", "account from leaf CN");
    assert_eq!(
        creds.2, "EPICS Test Root CA",
        "authority from trust store when peer sends partial chain"
    );

    server.stop();
}

/// PVA item #5: the client must extract the *server*'s X.509 identity
/// from the verified TLS chain and expose it via
/// `pvinfo_full_with_credentials` — the credentials pvxs `pvxinfo -v`
/// prints. The server presents a leaf cert (CN `pva-test-server`)
/// signed by the root CA `EPICS Test Root CA`, so the client must see
/// `account = "pva-test-server"` and `authority = "EPICS Test Root CA"`.
#[tokio::test]
async fn client_extracts_server_x509_identity() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Root CA → server leaf. The client trusts the CA and (server-only
    // TLS, no client cert) just verifies the server.
    let (ca_cert, ca_key) = make_ca("EPICS Test Root CA");
    let ca_der = CertificateDer::from(ca_cert.der().to_vec());
    let (server_cert, server_key) = make_leaf("pva-test-server", &ca_cert, &ca_key);

    // Server presents leaf + root so the client can build the chain.
    let server_cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![server_cert.clone(), ca_der.clone()], server_key)
        .expect("server tls config");
    let server_tls = Arc::new(TlsServerConfig {
        config: Arc::new(server_cfg),
        require_client_cert: false,
        trust_roots: Arc::new(RootCertStore::empty()),
    });

    let mut roots = RootCertStore::empty();
    roots.add(ca_der.clone()).unwrap();
    let client_cfg = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let client_tls = Arc::new(TlsClientConfig {
        config: Arc::new(client_cfg),
    });

    let source = Arc::new(StaticSource::new());
    source.put("SRVID:PV", 3.0).await;

    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: 0,
        tls_port: 0,
        idle_timeout: Duration::from_secs(60),
        max_connections: 16,
        max_channels_per_connection: 64,
        monitor_queue_depth: 8,
        tls: Some(server_tls),
        ..Default::default()
    };
    let server = PvaServer::start(source, cfg).expect("server start");
    let tcp = server.report().tcp_port;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let server_addr = server.tcp_addr();
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .with_tls(client_tls)
        .build();

    let (_desc, addr, cred) = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvinfo_full_with_credentials("SRVID:PV"),
    )
    .await
    .expect("pvinfo timed out")
    .expect("pvinfo failed");

    assert_eq!(addr.port(), tcp, "must report the queried server's port");
    let cred = cred.expect("TLS connection must yield a server X.509 identity");
    assert_eq!(
        cred.account, "pva-test-server",
        "account must be the server leaf cert CommonName"
    );
    assert_eq!(
        cred.authority, "EPICS Test Root CA",
        "authority must be the root CA CommonName"
    );

    server.stop();
}

/// A plain `pva://` (non-TLS) connection has no peer certificate, so
/// the client must report `None` for the server identity — the
/// `pvinfo -v` path then prints the anonymous credential line.
#[tokio::test]
async fn plain_connection_has_no_server_identity() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let source = Arc::new(StaticSource::new());
    source.put("PLAIN:PV", 1.0).await;

    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: 0,
        idle_timeout: Duration::from_secs(60),
        max_connections: 16,
        max_channels_per_connection: 64,
        monitor_queue_depth: 8,
        tls: None,
        ..Default::default()
    };
    let server = PvaServer::start(source, cfg).expect("server start");
    tokio::time::sleep(Duration::from_millis(100)).await;

    let server_addr = server.tcp_addr();
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .build();

    let (_desc, _addr, cred) = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvinfo_full_with_credentials("PLAIN:PV"),
    )
    .await
    .expect("pvinfo timed out")
    .expect("pvinfo failed");

    assert!(
        cred.is_none(),
        "a plain pva:// connection must have no server X.509 identity"
    );

    server.stop();
}

/// TLS-NAMESERVER regression: a single TCP port configured with TLS must
/// accept BOTH TLS clients and plain PVA clients (e.g. a pvxs name-server
/// redirect). Pre-fix: the TLS-only listener rejected the plain connection
/// with a TLS handshake error. Post-fix: first-byte peek (0x16 → TLS,
/// else → plain) dispatches correctly and both clients GET successfully.
#[tokio::test]
async fn pva_tls_nameserver_mixed_mode_listener() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let (cert, key) = generate_self_signed();

    let server_cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key)
        .expect("server tls config");
    let server_tls = Arc::new(TlsServerConfig {
        config: Arc::new(server_cfg),
        require_client_cert: false,
        trust_roots: Arc::new(RootCertStore::empty()),
    });

    let source = Arc::new(StaticSource::new());
    source.put("MIXED:PV", 42.0).await;

    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: 0,
        tls_port: 0,
        idle_timeout: Duration::from_secs(60),
        max_connections: 16,
        max_channels_per_connection: 64,
        monitor_queue_depth: 8,
        tls: Some(server_tls),
        ..Default::default()
    };
    let server = PvaServer::start(source, cfg).expect("server start");
    tokio::time::sleep(Duration::from_millis(100)).await;

    let server_addr = server.tcp_addr();

    // ── TLS client ────────────────────────────────────────────────────────────
    let mut roots = RootCertStore::empty();
    roots.add(cert).unwrap();
    let client_cfg = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let client_tls = Arc::new(TlsClientConfig {
        config: Arc::new(client_cfg),
    });
    let tls_client = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .with_tls(client_tls)
        .build();

    let tls_val = tokio::time::timeout(Duration::from_secs(5), tls_client.pvget("MIXED:PV"))
        .await
        .expect("TLS pvget timed out")
        .expect("TLS pvget failed");
    match tls_val {
        PvField::Structure(s) => assert!(
            matches!(s.get_value(), Some(ScalarValue::Double(d)) if (d - 42.0).abs() < 1e-9),
            "TLS client: unexpected value"
        ),
        other => panic!("TLS client: expected NTScalar, got {other:?}"),
    }

    // ── Plain PVA client (name-server scenario) ───────────────────────────────
    // Connects with 0xCA (PVA magic); first-byte peek routes to plain path.
    let plain_client = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .build();

    let plain_val = tokio::time::timeout(Duration::from_secs(5), plain_client.pvget("MIXED:PV"))
        .await
        .expect("plain pvget timed out; mixed-mode dispatch may be broken")
        .expect("plain pvget failed; server may have rejected plain connection");
    match plain_val {
        PvField::Structure(s) => assert!(
            matches!(s.get_value(), Some(ScalarValue::Double(d)) if (d - 42.0).abs() < 1e-9),
            "plain client: unexpected value"
        ),
        other => panic!("plain client: expected NTScalar, got {other:?}"),
    }

    server.stop();
}
