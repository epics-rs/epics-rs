//! Regression test (R9-22): `caput -S` renders the READBACK as a long string.
//!
//! C's `charArrAsStr` flag is read twice: by the write-value builder
//! (`caput.c:514` — send DBR_CHAR) AND by the readback print loop
//! (`caput.c:211-222`), which escapes a CHAR-array readback back into its
//! long-string form. Both the `Old :` and the `New :` line go through that
//! same `caget()` print loop (`caput.c:535,583`), so with `-S` both render as
//! text.
//!
//! Pre-fix `caput-rs` fed `-S` only to the write-value builder and rendered
//! the readbacks with `ValueFormat::default()`, so `caput-rs -S` printed the
//! CHAR waveform back as a numeric array (`6 104 101 108 108 111`).

#![cfg(tokio_backend)]

use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::CaServer;
use tokio::process::Command;

#[tokio::test(flavor = "multi_thread")]
async fn caput_dash_s_prints_both_readbacks_as_long_strings() {
    let mut initial = b"before".to_vec();
    initial.resize(16, 0);
    // The server TAKES its port by binding it (`.port(0)` → read back
    // `udp_port()`); nothing probes a port and hands the number on.
    let server = CaServer::builder()
        .port(0)
        .pv("R922:LSTR", EpicsValue::CharArray(initial))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    tokio::spawn(async move { server.run().await });

    let out = Command::new(env!("CARGO_BIN_EXE_caput-rs"))
        .args(["-w", "2", "-S", "R922:LSTR", "hello"])
        .env("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"))
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CA_SERVER_PORT", port.to_string())
        .output()
        .await
        .expect("run caput-rs");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "caput -S must succeed: {stdout:?}"
    );

    let old_line = stdout
        .lines()
        .find(|l| l.starts_with("Old : "))
        .unwrap_or_else(|| panic!("no Old line in: {stdout:?}"));
    let new_line = stdout
        .lines()
        .find(|l| l.starts_with("New : "))
        .unwrap_or_else(|| panic!("no New line in: {stdout:?}"));

    assert!(
        old_line.ends_with("before"),
        "-S must render the pre-put CHAR array as its long string \
         (caput.c:211-222); got: {old_line:?}"
    );
    assert!(
        new_line.ends_with("hello"),
        "-S must render the post-put CHAR array as its long string; got: {new_line:?}"
    );
}
