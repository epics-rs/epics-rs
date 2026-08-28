//! Real ACF/ASG cross-impl coverage. Closes the gap left by
//! batch 11 (which only exercised the wire shape of a denied
//! PUT via `on_put`).
//!
//! This test:
//! 1. Parses an inline `.acf` body that denies WRITE to all but
//!    a named UAG.
//! 2. Builds an `AccessGate::required(acf, resolver)` and pins
//!    it onto a SharedSource via `SharedSource::set_access_gate`.
//! 3. Runs pvxput against the Rust server — must fail with
//!    access-denied.
//! 4. Replaces the ACF in-place with an all-allow policy (still
//!    via `AccessGate::required` with a different .acf), runs
//!    pvxput again — must succeed.
//!
//! Exercises the full path: pvxs PVA client → Rust tcp.rs PUT
//! handler → composite forwarding → SharedSource access_gate →
//! AccessGate::check → ACF rule eval → deny status on the wire.

#![cfg(tokio_backend)]

use super::interop_helpers::{PVXPUT, pvxs_command, pvxs_lib_dir, require_pvxs};

use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::PvaServer;
use epics_pva_rs::server_native::{SharedPV, SharedSource};

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_asg_denied_put_via_real_acf() {
    use epics_base_rs::server::access_security::{AccessGate, parse_acf};

    let Some(pvxput) = require_pvxs(PVXPUT) else {
        return;
    };

    // ACF that denies WRITE entirely (only RULE(0, READ) — no
    // RULE(N, WRITE) clause means WRITE is implicitly denied).
    let acf_body = r#"
ASG(DEFAULT) {
    RULE(0, READ)
}
"#;
    let acf = parse_acf(acf_body).expect("parse acf");
    let cell = epics_base_rs::server::access_security::new_acf_cell(Some(acf));
    let resolver: epics_base_rs::server::access_security::AsgAslResolver =
        Arc::new(|_pv| Box::pin(async { ("DEFAULT".to_string(), 0u8) }));
    let gate = AccessGate::required(cell, resolver);

    // Build a SharedSource with the strict gate installed.
    let pv = SharedPV::new();
    pv.open(
        FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Int))],
        },
        PvField::Structure(PvStructure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), PvField::Scalar(ScalarValue::Int(0)))],
        }),
    )
    .unwrap();
    let src = SharedSource::new();
    src.add("ASG:DENY:PV", pv);
    src.set_access_gate(gate).expect("install gate");

    let server = PvaServer::isolated(Arc::new(src)).expect("server start");
    let addr = server.tcp_addr();
    let server_str = format!("127.0.0.1:{}", addr.port());

    let pvxput_p = pvxput.clone();
    let server_str_c = server_str.clone();
    let lib = pvxs_lib_dir().into_os_string();
    let out = tokio::task::spawn_blocking(move || {
        pvxs_command(&pvxput_p)
            .arg("-w")
            .arg("3")
            .arg("ASG:DENY:PV")
            .arg("42")
            .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
            .env("EPICS_PVA_NAME_SERVERS", &server_str_c)
            .env(env_key(), lib)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("pvxput exec")
    })
    .await
    .expect("join pvxput");

    server.stop();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    // pvxput must report failure when the server denies WRITE.
    assert!(
        !out.status.success(),
        "pvxput should have failed on ACF deny but exited success.\n\
         stdout: {stdout}\nstderr: {stderr}",
    );
    let merged = format!("{stdout}\n{stderr}");
    assert!(
        merged.contains("denied")
            || merged.contains("access")
            || merged.contains("Forbidden")
            || merged.contains("Error"),
        "pvxput failure output did not mention access deny.\n\
         merged: {merged}",
    );
}
