//! Field projection cross-impl.
//!
//! pvRequest `field(<a>,<b>)` tells the server to only emit the
//! listed leaves. Both sides must:
//! - Server: decode pvRequest, mask the encoded bitset/value to
//!   only the requested fields.
//! - Client: send the right pvRequest shape, decode the
//!   server's partial response without expecting the full struct.
//!
//! Tests:
//! - **Direction A**: pvxget -r 'field(value)' against Rust
//!   server hosting NTScalar Double — assert only `value` appears
//!   in the response (no alarm/timeStamp lines).
//! - **Direction B**: Rust `pvget_fields(["value"])` against pvxs
//!   `reverse_server` — assert decoded struct only carries the
//!   `value` leaf populated (other fields default).

// RTEMS-EXEC-MODEL-ALLOW(1): not run by the default nextest profile - this file is a module of the `interop_pvxs` binary, which `.config/nextest.toml`'s default-filter excludes.

use super::interop_helpers::pv_builders::complex_pv_matrix;
use super::interop_helpers::{PVXGET, pvxs_command, pvxs_lib_dir, require_pvxs};

use epics_pva_rs::server_native::{PvaServer, SharedSource};

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
async fn interop_field_projection_a_pvxget_field_filter_against_rust_server() {
    let Some(pvxget) = require_pvxs(PVXGET) else {
        return;
    };

    let source = SharedSource::new();
    for b in complex_pv_matrix() {
        source.add(b.name, b.open());
    }
    let server = PvaServer::isolated(Arc::new(source)).expect("server start");
    let addr = server.tcp_addr();
    let server_str = format!("127.0.0.1:{}", addr.port());

    // Ask pvxget for only the `value` field of T:DBL via pvRequest
    // `field(value)`. The Rust server must mask its emit to that
    // leaf — alarm.severity / timeStamp.* lines must NOT appear.
    let pvxget_p = pvxget.clone();
    let server_str_c = server_str.clone();
    let lib = pvxs_lib_dir().into_os_string();
    let out = tokio::task::spawn_blocking(move || {
        pvxs_command(&pvxget_p)
            .arg("-w")
            .arg("3")
            .arg("-r")
            .arg("field(value)")
            .arg("T:DBL")
            .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
            .env("EPICS_PVA_NAME_SERVERS", &server_str_c)
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
        "pvxget exit={:?}\nstdout: {stdout}\nstderr: {stderr}",
        out.status,
    );
    assert!(
        stdout.contains("value double = 123.457"),
        "field(value) projection missing the value leaf.\n{stdout}",
    );
    // Negative assertions — alarm/timeStamp lines must NOT appear
    // when projection is restricted to `value`. pvxs prints
    // alarm-severity always-zero etc. only if the wire carried it.
    assert!(
        !stdout.contains("alarm.severity"),
        "field(value) projection still emitted alarm.severity — server didn't mask its emit\n{stdout}",
    );
    assert!(
        !stdout.contains("timeStamp.secondsPastEpoch"),
        "field(value) projection still emitted timeStamp.secondsPastEpoch\n{stdout}",
    );
}
