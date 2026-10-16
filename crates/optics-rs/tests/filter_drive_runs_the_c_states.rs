#![cfg(tokio_backend)]
//! filterDrive's runner against `filterDrive.db` and two
//! `filterBladeNoSensor.db` blades: the `needRecalc`, `externalIO` and
//! `STATUS_BUSY` clauses of `waiting` (`filterDrive.st:396-450`).

use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::scan::ScanOwner;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::{CaServer, CaServerBuilder};
use optics_rs::snl::filter_drive::{self, FilterDriveConfig};
use std::collections::HashMap;
use std::time::Duration;

const DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/db/");
const OUTS: &str = r#"
record(stringout, "F:o1") { field(VAL, "out") }
record(stringout, "F:o2") { field(VAL, "out") }
record(ao, "F:kev") { field(VAL, "10") }
"#;

fn macros(kv: &[(&str, &str)]) -> HashMap<String, String> {
    kv.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The IOC, booted past C's four seconds of `start`/`init` sleeps.
async fn boot() -> (CaServer, ScanOwner) {
    let mut b = CaServerBuilder::new()
        .port(0)
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .db_string(OUTS, &macros(&[]))
        .unwrap()
        .db_file(
            &format!("{DB}filterDrive.db"),
            &macros(&[("P", "F:"), ("R", "f:"), ("DESC", "fd"), ("KEV", "F:kev")]),
        )
        .unwrap();
    for i in ["1", "2"] {
        let out = format!("F:o{i}");
        b = b
            .db_file(
                &format!("{DB}filterBladeNoSensor.db"),
                &macros(&[
                    ("P", "F:"),
                    ("R", "f:"),
                    ("N", i),
                    ("DESC", "b"),
                    ("OUT", &out),
                    ("OUT_STRING", "out"),
                    ("IN_STRING", "in"),
                ]),
            )
            .unwrap();
    }
    let server = b.build().await.unwrap();
    let scan = ScanOwner::start(server.database().clone());
    let db = (**server.database()).clone();
    tokio::spawn(filter_drive::run(FilterDriveConfig::new("F:", "f:", 2), db));
    tokio::time::sleep(Duration::from_millis(5000)).await;
    (server, scan)
}

async fn put(server: &CaServer, pv: &str, v: EpicsValue) {
    let (rec, field) = pv.split_once('.').unwrap_or((pv, "VAL"));
    server
        .database()
        .put_record_field_from_ca(rec, field, v)
        .await
        .unwrap();
}

async fn f64_of(server: &CaServer, pv: &str) -> f64 {
    server.get(pv).await.unwrap().to_f64().unwrap()
}

async fn string_of(server: &CaServer, pv: &str) -> String {
    match server.get(pv).await.unwrap() {
        EpicsValue::String(s) => s.to_string(),
        v => format!("{v:?}"),
    }
}

/// `init` raises `needRecalc` and the connect monitors raise the rest, so
/// boot ends in `update_filters`: the count is published and the busy record
/// the init puts set is cleared.
#[tokio::test(flavor = "multi_thread")]
async fn boot_publishes_the_count_and_clears_busy() {
    let (server, _scan) = boot().await;
    assert_eq!(f64_of(&server, "F:f:NumFilters").await, 2.0);
    assert_eq!(f64_of(&server, "F:f:Status").await, 0.0);
    assert_eq!(string_of(&server, "F:f:Message").await, "OK");
}

/// `sync THICK needRecalc`: a thickness change recomputes that blade.
#[tokio::test(flavor = "multi_thread")]
async fn a_thickness_change_recomputes_the_blade() {
    let (server, _scan) = boot().await;
    let before = f64_of(&server, "F:f:Fi1:Transmission").await;
    put(&server, "F:f:Fi1:Thickness", EpicsValue::Double(400.0)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after = f64_of(&server, "F:f:Fi1:Transmission").await;
    assert!(after < before, "{after} after 400 um, {before} before");
}

/// `sync OUTGET externalIO`: a blade moved by something else takes its `Set`
/// from the readback, and says so.
#[tokio::test(flavor = "multi_thread")]
async fn an_external_out_change_takes_the_blade_set() {
    let (server, _scan) = boot().await;
    put(&server, "F:o2", EpicsValue::String("in".into())).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(f64_of(&server, "F:f:Fi2:Set").await, 1.0);
    assert_eq!(f64_of(&server, "F:f:FilterMask").await, 2.0);
    assert_eq!(
        string_of(&server, "F:f:Message").await,
        "Filter state set by external agent."
    );
}

/// `when (STATUS_BUSY)`: a busy record set from outside runs
/// `update_filters`, whose `changeFlag` clears it.
#[tokio::test(flavor = "multi_thread")]
async fn an_outside_busy_is_cleared_by_update_filters() {
    let (server, _scan) = boot().await;
    put(&server, "F:f:Status", EpicsValue::Enum(1)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(f64_of(&server, "F:f:Status").await, 0.0);
}
