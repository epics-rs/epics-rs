//! mTLS (mutual-TLS / client certificate auth) cross-impl.
//!
//! Extends batch 12: server-auth TLS was covered (server
//! presents leaf signed by CA, client trusts CA). This adds
//! client-side cert auth — the server REQUIRES a client cert
//! signed by the same CA, and the client presents one. The
//! pvxs flag is `EPICS_PVA_TLS_OPTIONS=client_cert=require`.
//!
//! SKIPs if the TLS-enabled pvxs build is not present.

// RTEMS-EXEC-MODEL-ALLOW(1): not run by the default nextest profile - this file is a module of the `interop_pvxs` binary, which `.config/nextest.toml`'s default-filter excludes.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

fn env_key() -> &'static str {
    if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    }
}

fn tls_pvxs_root() -> Option<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let root = PathBuf::from(&home).join("codes/pvxs-tls");
    if root
        .join("bin")
        .join(super::interop_helpers::pvxs_arch())
        .is_dir()
    {
        return Some(root);
    }
    None
}

fn locate_tls_binary(name: &str) -> Option<PathBuf> {
    tls_pvxs_root().and_then(|r| {
        let p = r
            .join("bin")
            .join(super::interop_helpers::pvxs_arch())
            .join(name);
        p.is_file().then_some(p)
    })
}

fn tls_lib_dir() -> Option<PathBuf> {
    tls_pvxs_root().map(|r| r.join("lib").join(super::interop_helpers::pvxs_arch()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_tls_mtls_pvxget_with_client_cert_to_rust_server() {
    let Some(pvxget) = locate_tls_binary("pvxget") else {
        eprintln!("SKIP: no TLS-enabled pvxs at ~/codes/pvxs-tls (see batch 12)");
        return;
    };
    let Some(lib) = tls_lib_dir() else {
        eprintln!("SKIP: pvxs-tls lib dir missing");
        return;
    };

    // CA + leaf (used by both server and client cert chains).
    let mut ca_params = rcgen::CertificateParams::new(Vec::new()).expect("ca params");
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "rust-pva-mtls-ca");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key = rcgen::KeyPair::generate().expect("ca key");
    let ca_cert = ca_params.self_signed(&ca_key).expect("ca self-signed");

    let mut leaf_params =
        rcgen::CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()])
            .expect("leaf params");
    leaf_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "rust-pva-mtls-leaf".to_string());
    leaf_params.is_ca = rcgen::IsCa::ExplicitNoCa;
    let leaf_key = rcgen::KeyPair::generate().expect("leaf key");
    let leaf_cert = leaf_params
        .signed_by(&leaf_key, &ca_cert, &ca_key)
        .expect("leaf signed");
    let cert_der_vec = leaf_cert.der().to_vec();
    let key_der_vec = leaf_key.serialize_der();
    let ca_der_vec = ca_cert.der().to_vec();

    let dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SKIP: tempdir: {e}");
            return;
        }
    };
    let leaf_pem = format!("{}\n{}\n", leaf_cert.pem(), leaf_key.serialize_pem());
    let leaf_pem_path = dir.path().join("leaf.pem");
    std::fs::write(&leaf_pem_path, &leaf_pem).expect("write leaf pem");
    let ca_pem_path = dir.path().join("ca.pem");
    std::fs::write(&ca_pem_path, ca_cert.pem()).expect("write ca pem");
    let p12_path = dir.path().join("client_and_ca.p12");
    let p12_out = std::process::Command::new("openssl")
        .args(["pkcs12", "-export", "-out"])
        .arg(&p12_path)
        .args(["-inkey"])
        .arg(&leaf_pem_path)
        .args(["-in"])
        .arg(&leaf_pem_path)
        .args(["-certfile"])
        .arg(&ca_pem_path)
        .args(["-passout", "pass:"])
        .output();
    match p12_out {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            eprintln!(
                "SKIP: openssl pkcs12: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            return;
        }
        Err(e) => {
            eprintln!("SKIP: openssl cli: {e}");
            return;
        }
    }

    // Rust server: TLS + require client cert (the mtls part).
    // Build a ClientCertVerifier from rustls that accepts any
    // chain rooted at our test CA.
    use rustls::RootCertStore;
    use rustls::ServerConfig;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use rustls::server::WebPkiClientVerifier;

    let leaf_cert_der = CertificateDer::from(cert_der_vec);
    let leaf_key_der: PrivateKeyDer<'static> = PrivatePkcs8KeyDer::from(key_der_vec).into();
    let ca_cert_der = CertificateDer::from(ca_der_vec);
    let mut roots = RootCertStore::empty();
    roots.add(ca_cert_der.clone()).expect("add ca to roots");
    let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .expect("client verifier");
    let rustls_cfg = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![leaf_cert_der], leaf_key_der)
        .expect("rustls server cfg");
    let tls_cfg = epics_pva_rs::auth::TlsServerConfig {
        config: Arc::new(rustls_cfg),
        require_client_cert: true,
        trust_roots: std::sync::Arc::new(RootCertStore::empty()),
    };

    use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
    use epics_pva_rs::server_native::{PvaServer, PvaServerConfig, SharedPV, SharedSource};

    let pv = SharedPV::new();
    pv.open(
        FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Int))],
        },
        PvField::Structure(PvStructure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), PvField::Scalar(ScalarValue::Int(2024)))],
        }),
    )
    .unwrap();
    let src = SharedSource::new();
    src.add("MTLS:PV", pv);

    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: {
            let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let p = s.local_addr().unwrap().port();
            drop(s);
            p
        },
        tls: Some(Arc::new(tls_cfg)),
        ..PvaServerConfig::isolated()
    };
    let server = PvaServer::start(Arc::new(src), cfg).expect("server start");
    let addr = server.tcp_addr();
    let server_udp_port = server.report().udp_port;
    let server_addr_list = format!("127.0.0.1:{server_udp_port}");
    let server_tcp_port = addr.port();

    let pvxget_p = pvxget.clone();
    let cert_path = p12_path.clone();
    let lib_os = lib.into_os_string();
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new(&pvxget_p)
            .arg("-w")
            .arg("3")
            .arg("MTLS:PV")
            .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
            .env("EPICS_PVA_ADDR_LIST", &server_addr_list)
            .env("EPICS_PVA_BROADCAST_PORT", server_udp_port.to_string())
            .env("EPICS_PVA_SERVER_PORT", server_tcp_port.to_string())
            .env("EPICS_PVA_NAME_SERVERS", "")
            .env("EPICS_PVA_TLS_KEYCHAIN", cert_path.display().to_string())
            .env("EPICS_PVA_TLS_OPTIONS", "")
            .env(env_key(), lib_os)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("pvxget exec")
    })
    .await
    .expect("join pvxget");

    server.stop();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "mTLS pvxget exit={:?}\nstdout: {stdout}\nstderr: {stderr}",
        out.status,
    );
    assert!(
        stdout.contains("value int32_t = 2024"),
        "mTLS round-trip missing expected value.\n{stdout}",
    );
}
