//! An `Async Soft Channel` output's put to a remote CA link completes through
//! libca's put callback (`dbCa.c:537-542`), not dbNotify: only a local
//! target's completion is pinned to priorityLow, so a remote one keeps the
//! record's own band.

#![cfg(feature = "client-core")]
#![cfg(tokio_backend)]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::{CallbackPriority, spawn_blocking_background};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::calink::install_calink_resolver;
use epics_ca_rs::server::CaServer;
use serial_test::serial;

/// Boundary — the low band is busy for the whole put: a remote target's
/// completing pass runs on OUT's HIGH band regardless.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_remote_completion_does_not_wait_for_the_low_band() {
    let server = CaServer::builder()
        .port(0)
        .pv("REMOTE:TGT", EpicsValue::Double(0.0))
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    let _server = tokio::spawn(async move { server.run().await });
    // SAFETY: `#[serial(epics_env)]` keeps every env-touching test apart.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }

    let (db, _h) = IocBuilder::new()
        .db_string(
            r#"
record(ao, "OUT") {
    field(DTYP, "Async Soft Channel")
    field(OUT,  "REMOTE:TGT CA")
    field(PRIO, "HIGH")
}
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let resolver = install_calink_resolver(
        &db,
        epics_base_rs::runtime::task::Reactor::current().expect("test runtime"),
    )
    .await;
    db.initialize_link_locality().await;
    // The build ran with no `ca` lset: its `dbCaAddLink` pass is this one.
    db.setup_external_link_opens().await;
    db.setup_cp_links().await;
    db.scan_control().run();
    db.process_cp_links_on_connect().await;
    assert!(
        resolver
            .wait_for_link_connected("REMOTE:TGT", fact_budget::FACT_BUDGET)
            .await,
        "the remote link connects"
    );

    let (release, held) = std::sync::mpsc::channel::<()>();
    let (busy, is_busy) = std::sync::mpsc::channel();
    let hold = spawn_blocking_background(CallbackPriority::Low, move |_| {
        busy.send(()).unwrap();
        let _ = held.recv_timeout(Duration::from_secs(30));
    });
    is_busy.recv_timeout(Duration::from_secs(10)).unwrap();

    db.put_record_field_from_ca("OUT", "VAL", EpicsValue::Double(4.0))
        .await
        .unwrap();
    let out = db.get_record("OUT").unwrap();
    // Well inside the hold, which a wait on the low band would outlast.
    let deadline = Instant::now() + Duration::from_secs(5);
    while out.read().is_processing() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let completed = !out.read().is_processing();
    let _ = release.send(());
    let _ = hold.await;

    assert!(completed, "OUT's remote completion waited for the low band");
}
