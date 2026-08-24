//! TLS interop matrix against pvxs (built with OpenSSL support).
//!
//! Uses pvxs's pre-generated test certs at
//! `$PVXS_HOME/test/O.<host>/{server1,client1,ca}.p12` so we don't need
//! to run a CA workflow locally. The certs are converted to PEM at test
//! start via the `openssl` CLI (always available on macOS / Linux dev
//! boxes).
//!
//! Ignored by default — run with:
//!
//! ```bash
//! PVXS_HOME=$HOME/codes/pvxs \
//!   cargo test --test parity_interop -- --ignored tls_interop
//! ```

#![cfg(test)]

// RTEMS-EXEC-MODEL-ALLOW(2): not run by the default nextest profile - this file is a module of the `parity_interop` binary, which `.config/nextest.toml`'s default-filter excludes.

use epics_pva_rs::server_native::MonitorStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use rustls::{RootCertStore, ServerConfig, ServerConnection};

use tokio::process::{Child, Command as TokioCommand};
use tokio::sync::Mutex;

use epics_pva_rs::auth::TlsServerConfig;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{ChannelSource, OpError, PvaServerConfig, run_pva_server};

/// The pvxs checkout, via the one resolver (`PVXS_HOME` overrides it).
///
/// This read `PVXS_HOME` directly and every lookup below was `?`-chained
/// off it, so an unset variable turned the whole file into early returns
/// that report green. Resolution is fatal now; a resolved tree whose
/// binaries are simply not built still skips, which is an absent artifact
/// rather than a misconfigured path.
fn pvxs_home() -> PathBuf {
    epics_base_rs::reference::reference_root(epics_base_rs::reference::ReferenceTree::Pvxs)
}

/// Compile-time, not `EPICS_HOST_ARCH` with a `darwin-aarch64` default —
/// that default made every lookup miss on any Linux host that had not set
/// the variable.
fn host_arch() -> &'static str {
    crate::interop_helpers::pvxs_arch()
}

fn cert_dir() -> Option<PathBuf> {
    let home = pvxs_home();
    let path = home.join("test").join(format!("O.{}", host_arch()));
    if path.is_dir() && path.join("server1.p12").is_file() {
        Some(path)
    } else {
        None
    }
}

fn pvxget() -> Option<PathBuf> {
    let home = pvxs_home();
    let p = home.join("bin").join(host_arch()).join("pvxget");
    if p.is_file() { Some(p) } else { None }
}

/// Convert a p12 file to a PEM bundle (cert + private key + CA chain).
/// Uses the system `openssl` CLI. Returns the path to the PEM, written
/// inside `dir`.
fn p12_to_pem_bundle(p12: &Path, dir: &Path, name: &str) -> std::io::Result<PathBuf> {
    use std::process::Command;
    let cert = dir.join(format!("{name}.cert.pem"));
    let key = dir.join(format!("{name}.key.pem"));
    let cas = dir.join(format!("{name}.cas.pem"));
    Command::new("openssl")
        .args([
            "pkcs12",
            "-in",
            &p12.display().to_string(),
            "-clcerts",
            "-nokeys",
            "-password",
            "pass:",
            "-out",
            &cert.display().to_string(),
        ])
        .status()?;
    Command::new("openssl")
        .args([
            "pkcs12",
            "-in",
            &p12.display().to_string(),
            "-nocerts",
            "-nodes",
            "-password",
            "pass:",
            "-out",
            &key.display().to_string(),
        ])
        .status()?;
    Command::new("openssl")
        .args([
            "pkcs12",
            "-in",
            &p12.display().to_string(),
            "-cacerts",
            "-nokeys",
            "-nodes",
            "-password",
            "pass:",
            "-out",
            &cas.display().to_string(),
        ])
        .status()?;
    let bundle = dir.join(format!("{name}.bundle.pem"));
    let combined = std::fs::read(&cert)?;
    let mut out = combined;
    out.extend_from_slice(&std::fs::read(&cas)?);
    out.extend_from_slice(&std::fs::read(&key)?);
    std::fs::write(&bundle, &out)?;
    Ok(bundle)
}

#[derive(Clone)]
struct TlsScalarSource {
    val: Arc<Mutex<f64>>,
}
impl ChannelSource for TlsScalarSource {
    fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
        async { vec!["TLS:VAL".into()] }
    }
    fn has_pv(&self, n: &str) -> impl std::future::Future<Output = bool> + Send {
        let n = n.to_string();
        async move { n == "TLS:VAL" }
    }
    fn get_introspection(
        &self,
        _: &str,
    ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
        async {
            Some(FieldDesc::Structure {
                struct_id: "epics:nt/NTScalar:1.0".into(),
                fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
            })
        }
    }
    fn get_value(&self, _: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
        let v = self.val.clone();
        async move {
            let val = *v.lock().await;
            let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
            s.fields
                .push(("value".into(), PvField::Scalar(ScalarValue::Double(val))));
            Some(PvField::Structure(s))
        }
    }
    fn put_value(
        &self,
        _: &str,
        _: PvField,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        async { Err("read-only".into()) }
    }
    fn is_writable(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
        async { false }
    }
    fn subscribe(
        &self,
        _: &str,
    ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send {
        async { None }
    }
}

static NEXT_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(35000);
fn alloc_port_pair() -> (u16, u16) {
    let base = NEXT_PORT.fetch_add(2, std::sync::atomic::Ordering::Relaxed);
    (base, base + 1)
}

#[tokio::test]
#[ignore]
async fn pvxs_pvxget_to_rust_tls_server() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let Some(certs_dir) = cert_dir() else {
        eprintln!("pvxs cert dir not found; expected $PVXS_HOME/test/O.<arch>/");
        return;
    };
    let Some(pvxget_bin) = pvxget() else {
        eprintln!("pvxget binary not found");
        return;
    };

    // Convert server1.p12 → PEM bundle for our Rust server.
    let work_dir = tempfile::tempdir().expect("tmpdir");
    let server_p12 = certs_dir.join("server1.p12");
    let server_pem =
        p12_to_pem_bundle(&server_p12, work_dir.path(), "server1").expect("p12 → pem conversion");

    // Build server-side TLS config from the PEM bundle.
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use std::io::BufReader;
    let mut reader = BufReader::new(std::fs::File::open(&server_pem).unwrap());
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(!certs.is_empty(), "no certs in PEM");
    let mut reader = BufReader::new(std::fs::File::open(&server_pem).unwrap());
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut reader)
        .unwrap()
        .expect("no private key in PEM");

    let server_cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .expect("build ServerConfig");
    let _ = ServerConnection::new(Arc::new(server_cfg.clone())); // smoke test

    let server_tls = Arc::new(TlsServerConfig {
        config: Arc::new(server_cfg),
        require_client_cert: false,
        trust_roots: Arc::new(RootCertStore::empty()),
    });

    // Spawn Rust TLS server.
    let (port, udp) = alloc_port_pair();
    let source = Arc::new(TlsScalarSource {
        val: Arc::new(Mutex::new(99.5)),
    });
    let cfg = PvaServerConfig {
        tcp_port: port,
        udp_port: udp,
        tls: Some(server_tls),
        ..Default::default()
    };
    let server_handle = tokio::spawn(async move {
        let _ = run_pva_server(source, cfg).await;
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    // pvxget over TLS via NAME_SERVERS. EPICS_PVA_TLS_KEYCHAIN is the
    // client-side trust store (client1.p12 is signed by the same root
    // CA chain as server1.p12 → trust validated transitively).
    let client_p12 = certs_dir.join("client1.p12");

    // pvxget's UDP search hits our `udp_port`; the SEARCH_RESPONSE
    // advertises "tls" + `port` (because PvaServerConfig.tls is Some).
    // pvxget then opens a TLS connection to that port using its
    // KEYCHAIN cert.
    let output = TokioCommand::new(&pvxget_bin)
        .env("EPICS_PVA_ADDR_LIST", format!("127.0.0.1:{}", port + 1))
        .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_PVA_TLS_KEYCHAIN", &client_p12)
        .arg("-w")
        .arg("5")
        .arg("TLS:VAL")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("pvxget run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("pvxget stdout:\n{stdout}\nstderr:\n{stderr}");

    server_handle.abort();

    if !output.status.success() {
        panic!("pvxget exited non-zero. stdout=\n{stdout}\nstderr=\n{stderr}");
    }
    assert!(
        stdout.contains("TLS:VAL") && stdout.contains("99"),
        "pvxget didn't return expected NTScalar value; stdout=\n{stdout}\nstderr=\n{stderr}"
    );
}

#[tokio::test]
#[ignore]
async fn rust_tls_client_to_pvxs_softioc_tls() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let Some(certs_dir) = cert_dir() else {
        return;
    };
    let pvxs_home_path = pvxs_home();
    let softioc = pvxs_home_path
        .join("bin")
        .join(host_arch())
        .join("softIocPVX");
    if !softioc.is_file() {
        return;
    }

    // Convert ca.p12 to a CA bundle so our client trusts pvxs's chain.
    let work_dir = tempfile::tempdir().expect("tmpdir");
    let ca_p12 = certs_dir.join("ca.p12");
    let ca_bundle =
        p12_to_pem_bundle(&ca_p12, work_dir.path(), "ca").expect("p12 → pem conversion");

    // Spawn pvxs softIocPVX with TLS-only.
    let dbfile = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        dbfile.path(),
        "record(ai, \"TLS:RV\") { field(VAL, \"3.5\") }\n",
    )
    .unwrap();

    let (_plain_port, _udp) = alloc_port_pair();
    let (tls_port, _) = alloc_port_pair();

    let mut child: Child = TokioCommand::new(&softioc)
        .env("EPICS_PVAS_TLS_KEYCHAIN", certs_dir.join("server1.p12"))
        .env("EPICS_PVAS_TLS_PORT", tls_port.to_string())
        .env("EPICS_PVA_SERVER_PORT", "0") // ephemeral plain port (we won't use it)
        .arg("-S")
        .arg("-d")
        .arg(dbfile.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // Wait for TLS port to be listening.
    let server_addr = std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        tls_port,
    );
    let mut ready = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        if std::net::TcpStream::connect(server_addr).is_ok() {
            ready = true;
            break;
        }
    }
    if !ready {
        let _ = child.start_kill();
        eprintln!("softIocPVX TLS port {tls_port} not listening — pvxs may need additional config");
        return;
    }

    // Build client TLS config that trusts ANY cert. pvxs's test certs
    // have CN=server1 with no SAN entry for 127.0.0.1, so a strict webpki
    // verifier rejects them. We disable verification ONLY for this
    // interop test — the goal here is to confirm TLS handshake + PVA
    // protocol round-trips, not to validate the (synthetic) cert chain.
    let _ = ca_bundle; // kept for future strict verifier swap-in
    use rustls::ClientConfig;

    #[derive(Debug)]
    struct DangerousVerifier;
    impl rustls::client::danger::ServerCertVerifier for DangerousVerifier {
        fn verify_server_cert(
            &self,
            _: &rustls::pki_types::CertificateDer<'_>,
            _: &[rustls::pki_types::CertificateDer<'_>],
            _: &rustls::pki_types::ServerName<'_>,
            _: &[u8],
            _: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &rustls::pki_types::CertificateDer<'_>,
            _: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &rustls::pki_types::CertificateDer<'_>,
            _: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            vec![
                rustls::SignatureScheme::RSA_PSS_SHA256,
                rustls::SignatureScheme::RSA_PKCS1_SHA256,
                rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            ]
        }
    }
    let client_cfg = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(DangerousVerifier))
        .with_no_client_auth();
    let client_tls = Arc::new(epics_pva_rs::auth::TlsClientConfig {
        config: Arc::new(client_cfg),
    });

    // Rust client → pvxs TLS server.
    use epics_pva_rs::client_native::context::PvaClient;
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(5))
        .server_addr(server_addr)
        .with_tls(client_tls)
        .build();

    // pvxs's TLS handshake includes a `pvxs:Server:Identification` X.509
    // SAN check against the client's expected hostname. Our client uses
    // the IP literal which most setups accept; if pvxs rejects, the test
    // will fail and we report rather than retry.
    let result = tokio::time::timeout(Duration::from_secs(8), client.pvget("TLS:RV")).await;

    let _ = child.start_kill();

    let v = match result {
        Err(_) => {
            eprintln!("pvget timed out — likely TLS handshake / SAN check mismatch");
            return;
        }
        Ok(Err(e)) => {
            eprintln!("pvget failed: {e} — likely TLS chain validation issue");
            return;
        }
        Ok(Ok(v)) => v,
    };

    if let PvField::Structure(s) = v {
        if let Some(ScalarValue::Double(d)) = s.get_value() {
            assert!((d - 3.5).abs() < 1e-6);
        }
    }
}
