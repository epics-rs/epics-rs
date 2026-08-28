//! Interop: pvxs C++ client builds the *typed* pipeline
//! pvRequest (`Context::request().record("pipeline", true)`) and
//! connects to a Rust PVA server hosting one PV. Previously the
//! Rust server's `monitor_pipeline_options` parser only matched
//! the parsed-string form (`record[pipeline=true]`) and silently
//! disabled flow control whenever a real pvxs program drove the
//! subscription via the typed builder.
//!
//! The cpp helper (`cpp_helpers/r20_typed_monitor.cpp`) is built
//! on the fly against the resolved pvxs tree if the binary is
//! missing, and a build that does not produce it FAILS the test.
//! The suite is opt-in (`--profile interop`), so a host without
//! pvxs never reaches here; one that does reach here and cannot
//! build the helper is a broken checkout, not a skip.

#![cfg(tokio_backend)]

use super::interop_helpers::{LogCapture, pvxs_lib_dir};

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_r20_typed_pipeline_from_pvxs_against_rust_server() {
    use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
    use epics_pva_rs::server_native::PvaServer;
    use epics_pva_rs::server_native::{SharedPV, SharedSource};
    use std::sync::atomic::{AtomicBool, Ordering};

    // The only absent-prerequisite skip on this path; everything past it
    // is our own helper, so it panics rather than skipping.
    if super::interop_helpers::require_cxx().is_none() {
        return;
    }
    let helper = super::interop_helpers::cpp_helper("r20_typed_monitor");

    // Capture the server's debug output for the discriminating assertion
    // below. The capture is this test's own; it starts empty and the
    // subscriber behind it is owned by `interop_helpers`, so it no longer
    // matters which test in this process asked for one first.
    let log = LogCapture::start();

    // Spin up a Rust server hosting a counter PV and ticking the
    // value every 100 ms so the subscriber sees a stream, not a
    // single snapshot.
    let pv = SharedPV::new();
    pv.open(
        FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".to_string(),
            fields: vec![("value".to_string(), FieldDesc::Scalar(ScalarType::Double))],
        },
        PvField::Structure(PvStructure {
            struct_id: "epics:nt/NTScalar:1.0".to_string(),
            fields: vec![(
                "value".to_string(),
                PvField::Scalar(ScalarValue::Double(0.0)),
            )],
        }),
    )
    .unwrap();
    let source = SharedSource::new();
    source.add("R20:PV", pv.clone());
    let source_arc = Arc::new(source);

    let server = PvaServer::isolated(source_arc).expect("server start");
    let addr = server.tcp_addr();

    // Ticker: post a new value every 100 ms until the helper exits.
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let pv_t = pv.clone();
    let ticker = tokio::spawn(async move {
        let mut i = 1i32;
        while !stop_t.load(Ordering::Relaxed) {
            let val = PvField::Structure(PvStructure {
                struct_id: "epics:nt/NTScalar:1.0".to_string(),
                fields: vec![(
                    "value".to_string(),
                    PvField::Scalar(ScalarValue::Double(i as f64)),
                )],
            });
            let _ = pv_t.try_post(val);
            i += 1;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });

    // Spawn the helper as a subprocess. Wait inside spawn_blocking
    // so the synchronous .wait_with_output() doesn't park the
    // runtime worker.
    let helper_str = helper.display().to_string();
    let server_str = format!("{}:{}", addr.ip(), addr.port());
    let env_key = if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    };
    let lib_dir = pvxs_lib_dir();

    let output = tokio::task::spawn_blocking(move || {
        let child = Command::new(&helper_str)
            .arg("--server")
            .arg(&server_str)
            .arg("--pv")
            .arg("R20:PV")
            .arg("--events")
            .arg("3")
            .arg("--timeout")
            .arg("6")
            .env(env_key, lib_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child.wait_with_output()
    })
    .await
    .expect("join helper");
    let output = match output {
        Ok(o) => o,
        Err(e) => {
            stop.store(true, Ordering::Relaxed);
            let _ = ticker.await;
            server.stop();
            panic!("failed to run r20 helper: {e}");
        }
    };

    stop.store(true, Ordering::Relaxed);
    let _ = ticker.await;
    server.stop();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "pvxs typed-pipeline client did not receive the expected \
         events from the Rust server. Helper exit={:?}\n\
         stdout: {stdout}\nstderr: {stderr}",
        output.status,
    );

    // Discriminating assertion: the Rust server emits a debug event
    // `MONITOR INIT pipeline negotiated` only when the parser
    // recognises the typed-Bool pipeline option. Previously the parser
    // returned None for typed-Bool, the event never fired, the server
    // still echoed events back to the client (no flow control), so
    // the helper would still exit 0 — only this log assertion
    // distinguishes a fixed parser from a regressed one.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let captured = log.text();
    assert!(
        captured.contains("MONITOR INIT pipeline negotiated"),
        "Regression: Rust server did not log \
         `MONITOR INIT pipeline negotiated` for the typed-Bool pipeline \
         pvRequest. monitor_pipeline_options either failed to match the \
         typed shape or short-circuited before installing the window. \
         Captured server output during the test window:\n{captured}",
    );
}
