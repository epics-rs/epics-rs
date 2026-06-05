//! Regression R0604-BRPVALINK-NOT-SEAM-WIRED-1 — the `pva` link set
//! (pvalink) must install at the base `AfterCaLinkInit` seam via
//! [`IocApplication::register_link_set_installer`], so a `pva://` link
//! loaded before iocInit lands in the working set of the iocInit
//! external-link wait (`PvDatabase::wait_for_external_links`) — exactly
//! as CA links already do.
//!
//! Pre-fix, pvalink installed inside the Phase-3 protocol runner
//! (`run_ca_pva_qsrv_ioc`), AFTER iocInit had already snapshotted the
//! registered link sets for its wait. This test drives the PRODUCTION
//! `IocApplication::run` path with a CUSTOM runner that installs nothing
//! itself: the registered seam installer is therefore the ONLY thing
//! that can make the `pva` scheme appear and populate the wait set, so a
//! pre-fix tree (no seam installer) fails both assertions below.
//!
//! No PVA server is stood up. `PvaLink::open` registers the link's
//! monitor identity and connects in the background, so the link is a
//! wait TARGET (`link_names()` non-empty) even while `is_connected()` is
//! still false — which is all this test needs: it asserts the link is in
//! the wait's working set, not that it connects (the connect path is
//! pvalink's own, separately-tested behaviour).
//!
//! Needs both `qsrv` (the runner + the `pvalink_link_set_install` shim
//! live there) and `pvalink` (the resolver); the shim is an empty no-op
//! installer without the latter.
#![cfg(all(feature = "qsrv", feature = "pvalink"))]

use std::time::Duration;

use epics_base_rs::server::ioc_app::IocApplication;
use serial_test::serial;

#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn pvalink_link_is_in_iocinit_external_link_wait_via_run_seam() {
    // SAFETY (edition 2024): these process-env writes are serialised
    // against the other `epics_env`-group tests by `#[serial]`, so no
    // other thread reads/writes the environment concurrently.
    unsafe {
        // QSRV2 gates the shim (pvxs `enable2()`). Pin it ON and clear
        // the ignore list so the decision is deterministic regardless of
        // the ambient environment.
        std::env::set_var("PVXS_QSRV_ENABLE", "YES");
        std::env::remove_var("EPICS_IOC_IGNORE_SERVERS");
        // The link never connects (no server), so iocInit's own
        // external-link wait would otherwise block the full 10s default.
        std::env::set_var("EPICS_RS_INIT_LINK_TIMEOUT", "0.2");
        // Confine the pvalink monitor's background UDP search to
        // localhost so the test does not broadcast on the LAN.
        std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_PVA_ADDR_LIST", "127.0.0.1");
    }

    // A Passive `calc` holder whose INPA is a CP pvalink in the canonical
    // pvxs JSON longhand. `calc` is used (not `ai`) because its INPA–INPU
    // are generic String link fields that `record_link_fields` surfaces;
    // an `ai` record's INP is consumed by device support and never appears
    // as a generic String field, so the install scan would not see it.
    // `proc: 'CP'` keeps it a `PvaJson` link, which the install scan always
    // pre-opens (no bare-PV early-out) — so its monitor identity is in the
    // registry before the iocInit wait runs. PINI=NO / SCAN=Passive so the
    // record never self-processes.
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("seam.db");
    std::fs::write(
        &db_path,
        "record(calc, \"PVALINK:SEAM:HOLDER\") {\n\
         \tfield(INPA, \"{pva: { pv: 'PVALINK:SEAM:SRC', proc: 'CP' }}\")\n\
         \tfield(CALC, \"A\")\n\
         \tfield(PINI, \"NO\")\n\
         \tfield(SCAN, \"Passive\")\n\
         }\n",
    )
    .expect("write seam.db");
    let stcmd_path = dir.path().join("st.cmd");
    std::fs::write(
        &stcmd_path,
        format!("dbLoadRecords(\"{}\")\n", db_path.display()),
    )
    .expect("write st.cmd");

    // The seam under test: register the pvalink installer at IOC
    // construction. The custom runner below installs no link set, so
    // this is the only path that can register the `pva` scheme.
    let result = IocApplication::new()
        .startup_script(stcmd_path.to_str().unwrap())
        .register_link_set_installer(epics_bridge_rs::qsrv::pvalink_link_set_install)
        .run(|config| async move {
            // By here iocInit has fired the seam installer (which
            // registered the `pva` lset and pre-opened the CP link) and
            // run its own external-link wait. Re-run the wait against the
            // same database to observe the state the iocInit wait saw.

            // 1) The `pva` link set is registered — the installer fired
            //    at the `AfterCaLinkInit` seam, not in a Phase-3 runner.
            let schemes = config.db.registered_link_schemes().await;
            assert!(
                schemes.iter().any(|s| s == "pva"),
                "the `pva` link set must be registered at the iocInit \
                 AfterCaLinkInit seam (registered schemes: {schemes:?})"
            );

            // 2) The CP pvalink loaded before iocInit is in the wait's
            //    working set. `total >= 1` is the crux: the iocInit wait
            //    now treats the PVA link as something to wait on. (No
            //    server → `connected` stays 0; that is fine, we are
            //    asserting membership in the wait set, not connection.)
            let (_connected, total) = config
                .db
                .wait_for_external_links(Duration::from_millis(50))
                .await;
            assert!(
                total >= 1,
                "the pvalink CP link loaded before iocInit must be in the \
                 external-link wait's working set (waited-on link count: {total})"
            );
            Ok(())
        })
        .await;
    result.expect("IOC run returns Ok after the seam assertions");

    // Best-effort restore so a leaked `PVXS_QSRV_ENABLE=YES` does not
    // surprise a later `epics_env`-group test.
    unsafe {
        std::env::remove_var("PVXS_QSRV_ENABLE");
        std::env::remove_var("EPICS_RS_INIT_LINK_TIMEOUT");
        std::env::remove_var("EPICS_PVA_AUTO_ADDR_LIST");
        std::env::remove_var("EPICS_PVA_ADDR_LIST");
    }
}
