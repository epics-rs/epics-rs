//! Access-denied PUT cross-impl. The pure ACF / ASG parsing
//! paths are unit-tested in `epics-base-rs`; this test proves
//! the WIRE side of a denied PUT round-trips correctly with a
//! real pvxs client. We deny via a `SharedPV::on_put` handler
//! that always returns `Err`; the server's PUT-error reply
//! must reach pvxput as a non-zero exit + an error message on
//! stderr.

// RTEMS-EXEC-MODEL-ALLOW(1): not run by the default nextest profile - this file is a module of the `interop_pvxs` binary, which `.config/nextest.toml`'s default-filter excludes.

use super::interop_helpers::{PVXPUT, pvxs_command, pvxs_lib_dir, require_pvxs};

use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};

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
async fn interop_access_denied_a_pvxput_to_rejecting_handler_fails() {
    let Some(pvxput) = require_pvxs(PVXPUT) else {
        return;
    };

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
    // Reject every PUT — same wire-shape as an ASG deny.
    pv.on_put(|_pv, _val| Err("not allowed by test policy".into()));

    let src = SharedSource::new();
    src.add("A:DENY:PV", pv);
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
            .arg("A:DENY:PV")
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

    // pvxput must report failure. The exact wording is "Error:
    // <server reason>" on stderr; exit status non-zero.
    assert!(
        !out.status.success(),
        "pvxput should have failed on rejecting handler but exited success.\n\
         stdout: {stdout}\nstderr: {stderr}",
    );
    let merged = format!("{stdout}\n{stderr}");
    assert!(
        merged.contains("not allowed by test policy"),
        "pvxput did not surface the server-side rejection reason.\n\
         merged output: {merged}",
    );
}
