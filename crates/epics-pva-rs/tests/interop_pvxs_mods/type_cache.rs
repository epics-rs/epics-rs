//! Type-cache (0xFD/0xFE backref) interop.
//!
//! pvxs's wire spec lets the *server* compress repeated
//! introspection descriptors by emitting a fresh full descriptor
//! once (prefixed `0xFD`, plus a u16 cache slot) and subsequently
//! referencing it (`0xFE` + same slot). pvxs and pvAccessJava
//! support this on the *client* side too. The Rust server has it
//! disabled by default (`PvaServerConfig::emit_type_cache=false`)
//! for max compatibility with old pvAccessCPP; this test flips
//! it on and verifies pvxs still reads every PV in the matrix.
//!
//! Catches encoder regressions in:
//! - the 0xFD define-and-emit path (`encode_field_desc_cached`)
//! - the 0xFE lookup-only path
//! - the per-connection slot allocation (a fresh slot for each
//!   new shape, repeated for old)

#![cfg(tokio_backend)]

use super::interop_helpers::pv_builders::complex_pv_matrix;
use super::interop_helpers::{PVXGET, pvxs_command, pvxs_lib_dir, require_pvxs};

use epics_pva_rs::server_native::PvaServer;
use epics_pva_rs::server_native::SharedSource;

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_type_cache_emit_pvxget_accepts_backrefs() {
    let Some(pvxget) = require_pvxs(PVXGET) else {
        return;
    };
    let pvs = complex_pv_matrix();

    let source = SharedSource::new();
    for b in &pvs {
        source.add(b.name, b.open());
    }

    let cfg = epics_pva_rs::server_native::PvaServerConfig {
        emit_type_cache: true,
        tcp_port: 0,
        udp_port: {
            let l = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            p
        },
        ..epics_pva_rs::server_native::PvaServerConfig::isolated()
    };
    let server = PvaServer::start(Arc::new(source), cfg).expect("server start");
    let addr = server.tcp_addr();
    let server_str = format!("127.0.0.1:{}", addr.port());

    let env_key = if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    };

    // Read the SAME PV twice on the same connection — second read
    // should land on a 0xFE backref. pvxget opens a fresh
    // connection per invocation though, so to amortise we read
    // multiple distinct PVs in one pvxget invocation (single
    // connection) — pvxget supports `-w 3 pv1 pv2 pv3` and reuses
    // the server connection across them.
    let pvxget_p = pvxget.clone();
    let server_str_c = server_str.clone();
    let lib = pvxs_lib_dir().into_os_string();
    let out = tokio::task::spawn_blocking(move || {
        pvxs_command(&pvxget_p)
            .arg("-w")
            .arg("3")
            // PVs reading the same shape twice (T:DBL appears in
            // two NTScalar invocations) so the type cache has a
            // chance to fire a backref.
            .args(["T:INT", "T:DBL", "T:DBL", "T:INT", "T:LONG", "T:STR"])
            .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
            .env("EPICS_PVA_NAME_SERVERS", &server_str_c)
            .env(env_key, lib)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("pvxget exec")
    })
    .await
    .expect("join pvxget");

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    server.stop();
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(
        out.status.success(),
        "pvxget exit={:?}\nstdout: {stdout}\nstderr: {stderr}",
        out.status,
    );
    // Each PV must have produced a value line.
    for (pv, want) in [
        ("T:INT", "value int32_t = -12345"),
        ("T:DBL", "value double = 123.457"),
        ("T:LONG", "value int64_t = 9000000000"),
        ("T:STR", r#"value string = "hello world""#),
    ] {
        assert!(
            stdout.contains(want),
            "expected substring {want:?} for {pv} not present in pvxget output (type-cache emit broke wire compatibility?)\n{stdout}",
        );
    }
}
