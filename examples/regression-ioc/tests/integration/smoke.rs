//! Harness smoke test: prove the in-process CA+PVA regression IOC boots and is
//! reachable over both protocols before the family-specific tests rely on it.
// The harness crate is `tokio_backend`-only, so this file is too:
// `regression_ioc::RegressionIoc` does not exist on the reactor-free backend.
#![cfg(tokio_backend)]

use std::time::Duration;

use epics_ca_rs::EpicsValue;
use regression_ioc::RegressionIoc;
use serial_test::serial;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn ioc_boots_and_answers_ca_and_pva() {
    let ioc = RegressionIoc::boot().await.expect("boot regression IOC");

    // --- CA: caput then caget round-trips through the live server ---
    let ca = ioc.ca_client().await;
    ca.caput("REG:H:AI", "12.5").await.expect("caput REG:H:AI");
    let (_dbf, val) = ca.caget("REG:H:AI").await.expect("caget REG:H:AI");
    match val {
        EpicsValue::Double(v) => assert_eq!(v, 12.5, "CA caget should read back the value"),
        other => panic!("expected Double, got {other:?}"),
    }

    // --- PVA: pvget reaches the same record over the native server ---
    let pva = ioc.pva_client();
    let field = tokio::time::timeout(Duration::from_secs(5), pva.pvget("REG:H:AI"))
        .await
        .expect("pvget did not time out")
        .expect("pvget REG:H:AI");
    // NTScalar comes back as a structure carrying a `value` leaf.
    let dbg = format!("{field:?}");
    assert!(
        dbg.contains("12.5"),
        "PVA pvget should reflect the value, got {dbg}"
    );
}
