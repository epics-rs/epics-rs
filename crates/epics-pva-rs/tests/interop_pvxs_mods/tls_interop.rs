//! TLS handshake cross-impl.
//!
//! The locally installed pvxs binary is typically built WITHOUT
//! `PVXS_ENABLE_SSL=YES` (the OpenSSL-linked TLS feature). We
//! probe `pvxinfo -D` for an OpenSSL/TLS marker and SKIP cleanly
//! when it's absent.
//!
//! When pvxs DOES have TLS:
//! - Generate a self-signed cert + private key via `rcgen`.
//! - Build a Rust `TlsServerConfig` from the rustls primitives.
//! - Spin up a Rust PVA server with that config.
//! - Run `pvxget` with `EPICS_PVA_TLS_KEYCHAIN=<pem-bundle>`.
//! - Assert pvxget succeeds.
//!
//! The Rust↔Rust handshake is already covered by `tests/tls.rs`;
//! this test only adds the cross-impl runtime check.

use super::interop_helpers::{PVXGET, pvxs_command, pvxs_lib_dir, require_pvxs};

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

/// Detect TLS support in the locally installed pvxs. `pvxinfo -D`
/// prints an OpenSSL version line iff the build linked OpenSSL.
fn pvxs_has_tls() -> bool {
    let Some(pvxinfo) = super::interop_helpers::locate_pvxs("pvxinfo") else {
        return false;
    };
    let Ok(out) = pvxs_command(&pvxinfo)
        .arg("-D")
        .env(env_key(), pvxs_lib_dir())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    else {
        return false;
    };
    let s =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    s.contains("OpenSSL") || s.contains("TLS=YES") || s.contains("pvxs.tls")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_tls_a_pvxget_over_tls_to_rust_server() {
    let Some(pvxget) = require_pvxs(PVXGET) else {
        return;
    };
    if !pvxs_has_tls() {
        eprintln!(
            "SKIP: locally installed pvxs build lacks TLS support \
             (no OpenSSL marker in `pvxinfo -D`). Rebuild pvxs with \
             `PVXS_ENABLE_SSL=YES` to enable. Rust-side TLS \
             handshake is fully covered by tests/tls.rs."
        );
        return;
    }

    // Self-signed leaf cert + key. `rcgen` defaults give a 1-year
    // validity, ECDSA-P256 key, which both rustls + OpenSSL accept.
    let mut params = rcgen::CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()])
        .expect("rcgen params");
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "rust-pva-server".to_string());
    params.is_ca = rcgen::IsCa::ExplicitNoCa;
    let key = rcgen::KeyPair::generate().expect("rcgen key");
    let cert = params.self_signed(&key).expect("rcgen self-signed");
    let cert_pem = cert.pem();
    let key_pem = key.serialize_pem();

    let dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SKIP: tempdir failed: {e}");
            return;
        }
    };
    let pem_path = dir.path().join("server.pem");
    std::fs::write(&pem_path, format!("{cert_pem}\n{key_pem}\n")).expect("write PEM");

    // Build Rust TlsServerConfig from the in-memory cert/key.
    use rustls::ServerConfig;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der: PrivateKeyDer<'static> = PrivatePkcs8KeyDer::from(key.serialize_der()).into();
    let rustls_cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .expect("rustls server cfg");
    let tls_cfg = epics_pva_rs::auth::TlsServerConfig {
        config: Arc::new(rustls_cfg),
        require_client_cert: false,
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
            fields: vec![("value".into(), PvField::Scalar(ScalarValue::Int(777)))],
        }),
    );
    let src = SharedSource::new();
    src.add("TLS:PV", pv);

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
    let server_str = format!("127.0.0.1:{}", addr.port());

    let pvxget_p = pvxget.clone();
    let server_str_c = server_str.clone();
    let cert_path = pem_path.clone();
    let lib = pvxs_lib_dir().into_os_string();
    let out = tokio::task::spawn_blocking(move || {
        pvxs_command(&pvxget_p)
            .arg("-w")
            .arg("3")
            .arg("TLS:PV")
            .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
            .env("EPICS_PVA_NAME_SERVERS", &server_str_c)
            .env("EPICS_PVA_TLS_KEYCHAIN", cert_path.display().to_string())
            .env("EPICS_PVA_TLS_OPTIONS", "")
            .env(env_key(), lib)
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
        "pvxget over TLS failed exit={:?}\nstdout: {stdout}\nstderr: {stderr}",
        out.status,
    );
    assert!(
        stdout.contains("value int32_t = 777"),
        "TLS round-trip missing expected value.\n{stdout}",
    );
}
