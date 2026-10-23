//! A record type registered at runtime scans among the declared ones by name,
//! as C's name-sorted `recordTypeList` puts it. oracleScalerIoc (`st-scaler.cmd`
//! with the records below, `s1`..`s8` for the soft driver), one period:
//!
//! ```text
//! scan-1: dbProcess of 'X3'
//! scan-1: dbProcess of 'X2'
//! scan-1: dbProcess of 'X1'
//! ```
//!
//! `ai` < `scaler` < `sel`, against load order.

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset};
use scaler_rs::device_support::scaler_asyn::ScalerAsynDeviceSupport;
use scaler_rs::device_support::scaler_soft::SoftScalerDriver;

/// `scalerSupport.dbd`'s `device(scaler,INST_IO,devScalerAsyn,"Asyn Scaler")`.
static ASYN_SCALER: DevSup = DevSup {
    name: "devScalerAsyn",
    choice: "Asyn Scaler",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(7, &[2, 4, 5, 6], false),
};

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — it ran and passed on the
// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p
// scaler-rs --all-features`, 123 passed): ScanOwner runs the scan thread, and
// the reactor the test sleeps on comes from `#[tokio::test]` itself.

const CHILD: &str = "SCALER_SCAN_ORDER_CHILD";

const DB: &str = r#"
record(sel, "X1") { field(SCAN, "1 second") field(TPRO, "1") }
record(scaler, "X2") {
    field(DTYP, "Asyn Scaler") field(OUT, "@asyn(scalerPort 0 0)")
    field(FREQ, "10000000") field(SCAN, "1 second") field(TPRO, "1")
}
record(ai, "X3") { field(SCAN, "1 second") field(TPRO, "1") }
"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_runtime_registered_type_scans_between_two_declared_ones() {
    if std::env::var_os(CHILD).is_none() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "a_runtime_registered_type_scans_between_two_declared_ones",
                "--exact",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let out = String::from_utf8(out.stdout).unwrap();
        let trace: Vec<&str> = out.lines().filter(|l| l.contains(": dbProcess")).collect();
        assert_eq!(
            trace,
            [
                "scan-1: dbProcess of 'X3'",
                "scan-1: dbProcess of 'X2'",
                "scan-1: dbProcess of 'X1'",
            ],
            "{out}"
        );
        return;
    }
    let (name, factory) = scaler_rs::scaler_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_device_support("scaler", &ASYN_SCALER, || {
            Box::new(ScalerAsynDeviceSupport::new(Box::new(
                SoftScalerDriver::new(8),
            )))
        })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let owner = epics_base_rs::server::scan::ScanOwner::start(db.clone());
    tokio::time::sleep(Duration::from_millis(1500)).await;
    drop(owner);
}
