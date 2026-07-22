//! TLS handshake cross-impl.
//!
//! The master pvxs build (used by every other interop test) is
//! linked WITHOUT OpenSSL — the TLS feature lives on the
//! `origin/tls` branch and is built into a separate worktree at
//! `~/codes/pvxs-tls`. We prefer that build when present; if
//! neither install exposes the `EPICS_PVA_TLS_KEYCHAIN` env var
//! in `pvxinfo -D` output, the test SKIPs cleanly.
//!
//! When pvxs DOES have TLS:
//! - Generate a self-signed cert + private key via `rcgen`.
//! - Write a PKCS#12 keychain (pvxs's only on-disk format).
//! - Build a Rust `TlsServerConfig` from the rustls primitives.
//! - Spin up a Rust PVA server with that config.
//! - Run pvxs `pvxget` with `EPICS_PVA_TLS_KEYCHAIN=<p12>`.
//! - Assert pvxget succeeds and round-trips the value.
//!
//! The Rust↔Rust handshake is already covered by `tests/tls.rs`.

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
    if let Some(root) = tls_pvxs_root() {
        let p = root
            .join("bin")
            .join(super::interop_helpers::pvxs_arch())
            .join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    super::interop_helpers::locate_pvxs(name)
}

fn tls_lib_dir() -> Option<PathBuf> {
    tls_pvxs_root().map(|r| r.join("lib").join(super::interop_helpers::pvxs_arch()))
}

fn effective_lib_dir() -> PathBuf {
    tls_lib_dir().unwrap_or_else(super::interop_helpers::pvxs_lib_dir)
}

/// Probe the located pvxs binary for TLS support. `pvxinfo -D` of
/// a TLS-enabled build shows `EPICS_PVA_TLS_KEYCHAIN=` (env var
/// printed even when empty); the non-TLS build has neither the
/// env var nor any TLS markers in its config dump.
fn pvxs_has_tls() -> bool {
    let Some(pvxinfo) = locate_tls_binary("pvxinfo") else {
        return false;
    };
    let mut cmd = std::process::Command::new(&pvxinfo);
    cmd.arg("-D")
        .env(env_key(), effective_lib_dir())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let Ok(out) = cmd.output() else { return false };
    let s =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    s.contains("EPICS_PVA_TLS_KEYCHAIN")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_tls_a_pvxget_over_tls_to_rust_server() {
    if !pvxs_has_tls() {
        eprintln!(
            "SKIP: no TLS-enabled pvxs found (looked in ~/codes/pvxs-tls/bin/<arch>/). \
             Rust-side TLS is covered by tests/tls.rs; rebuild pvxs on the `origin/tls` \
             branch with libevent+OpenSSL to enable this interop test."
        );
        return;
    }
    let Some(pvxget) = locate_tls_binary("pvxget") else {
        eprintln!("SKIP: pvxget not found");
        return;
    };

    // CA cert + leaf cert signed by CA. pvxs's TLS client refuses
    // a bare self-signed leaf ("invalid cert chain"); both sides
    // must trust a root CA, and the leaf must chain to it.
    let mut ca_params = rcgen::CertificateParams::new(Vec::new()).expect("ca params");
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "rust-pva-test-ca");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key = rcgen::KeyPair::generate().expect("ca key");
    let ca_cert = ca_params.self_signed(&ca_key).expect("ca self-signed");

    let mut leaf_params =
        rcgen::CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()])
            .expect("leaf params");
    leaf_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "rust-pva-server".to_string());
    leaf_params.is_ca = rcgen::IsCa::ExplicitNoCa;
    let leaf_key = rcgen::KeyPair::generate().expect("leaf key");
    let leaf_cert = leaf_params
        .signed_by(&leaf_key, &ca_cert, &ca_key)
        .expect("leaf signed by ca");
    let cert_der_vec = leaf_cert.der().to_vec();
    let key_der_vec = leaf_key.serialize_der();

    let dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SKIP: tempdir failed: {e}");
            return;
        }
    };

    // pvxs's keychain loader expects PKCS#12 — its OpenSSL backend
    // calls `d2i_PKCS12_bio` (see ossl.cpp), which rejects PEM
    // input with `wrong tag`. Write the leaf PEM (cert + key) and
    // the CA cert PEM, then bundle into PKCS#12 via openssl CLI.
    let leaf_pem_path = dir.path().join("leaf.pem");
    let leaf_pem = format!("{}\n{}\n", leaf_cert.pem(), leaf_key.serialize_pem());
    std::fs::write(&leaf_pem_path, &leaf_pem).expect("write leaf pem");
    let ca_pem_path = dir.path().join("ca.pem");
    std::fs::write(&ca_pem_path, ca_cert.pem()).expect("write ca pem");
    let p12_path = dir.path().join("server.p12");
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
                "SKIP: openssl pkcs12 conversion failed: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            return;
        }
        Err(e) => {
            eprintln!("SKIP: openssl CLI not available: {e}");
            return;
        }
    }

    // Build Rust TlsServerConfig from the in-memory cert/key.
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use rustls::{RootCertStore, ServerConfig};
    let cert_der = CertificateDer::from(cert_der_vec);
    let key_der: PrivateKeyDer<'static> = PrivatePkcs8KeyDer::from(key_der_vec).into();
    let rustls_cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .expect("rustls server cfg");
    let tls_cfg = epics_pva_rs::auth::TlsServerConfig {
        config: Arc::new(rustls_cfg),
        require_client_cert: false,
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
            fields: vec![("value".into(), PvField::Scalar(ScalarValue::Int(777)))],
        }),
    )
    .unwrap();
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

    // pvxs client name-server flow needs a plain-TCP query
    // BEFORE doing TLS, which our TLS-only Rust listener refuses.
    // Switch to UDP search: Rust's UDP responder advertises "tls"
    // protocol + tcp_port; pvxs client then opens a fresh TCP →
    // TLS connection to that port.
    let pvxget_p = pvxget.clone();
    let server_udp_port = server.report().udp_port;
    let server_addr_list = format!("127.0.0.1:{server_udp_port}");
    let server_tcp_port = addr.port();
    let _ = server_str;
    let cert_path = p12_path.clone();
    let lib = effective_lib_dir().into_os_string();
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new(&pvxget_p)
            .arg("-w")
            .arg("3")
            .arg("TLS:PV")
            .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
            .env("EPICS_PVA_ADDR_LIST", &server_addr_list)
            .env("EPICS_PVA_BROADCAST_PORT", server_udp_port.to_string())
            .env("EPICS_PVA_SERVER_PORT", server_tcp_port.to_string())
            .env("EPICS_PVA_NAME_SERVERS", "")
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
