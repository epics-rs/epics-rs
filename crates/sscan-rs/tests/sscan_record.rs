//! The sscan record driving records of its own database: 1-D and 2-D step
//! scans, table and fly scans, abort and pause, the after-scan move, monitors
//! posted mid-scan, and the C defects the port does not copy.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::runtime::task::sleep;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::ScanType;
use epics_base_rs::types::EpicsValue;

const PLANT: &str = r#"
record(ao, "M") {
  field(PREC, "3")
  field(DRVH, "100")
  field(DRVL, "-100")
}
record(ao, "M2") {
  field(PREC, "3")
  field(DRVH, "100")
  field(DRVL, "-100")
}
record(calc, "D") {
  field(INPA, "M NPP")
  field(INPB, "M2 NPP")
  field(CALC, "A*10+B")
}
record(calc, "PEAK") {
  field(INPA, "M NPP")
  field(CALC, "5-ABS(A-3)")
}
"#;

async fn ioc(scans: &str) -> Arc<PvDatabase> {
    let (name, factory) = sscan_rs::sscan_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .db_string(&format!("{PLANT}{scans}"), &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

fn f64_of(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

fn str_of(db: &PvDatabase, pv: &str) -> String {
    db.get_pv(pv).unwrap().text_g()
}

fn arr(db: &PvDatabase, pv: &str, n: usize) -> Vec<f64> {
    match db.get_pv(pv).unwrap() {
        EpicsValue::DoubleArray(v) => v.iter().take(n).copied().collect(),
        EpicsValue::FloatArray(v) => v.iter().take(n).map(|x| f64::from(*x)).collect(),
        other => panic!("{pv}: {other:?}"),
    }
}

async fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..1000 {
        if f() {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("{what} never held");
}

async fn put(db: &PvDatabase, pv: &str, v: EpicsValue) {
    db.put_pv(pv, v).await.unwrap();
}

async fn connected(db: &PvDatabase, scan: &str, fields: &[&str]) {
    for f in fields {
        let pv = format!("{scan}.{f}");
        wait_until(&pv, || f64_of(db, &pv) == 0.0).await;
    }
}

/// `dbPutField(scan.field, v)`: the put, and the pass `pp(TRUE)` asks for.
async fn put_field(db: &PvDatabase, scan: &str, field: &str, v: EpicsValue) {
    db.put_record_field_from_ca(scan, field, v).await.unwrap();
}

async fn start(db: &PvDatabase, scan: &str) {
    put_field(db, scan, "EXSC", EpicsValue::Short(1)).await;
}

async fn finished(db: &PvDatabase, scan: &str) {
    let data = format!("{scan}.DATA");
    let busy = format!("{scan}.BUSY");
    wait_until(&data, || f64_of(db, &data) == 1.0).await;
    wait_until(&busy, || f64_of(db, &busy) == 0.0).await;
}

/// Every value `sub` has queued, oldest first.
fn drain(sub: &mut DbSubscription) -> Vec<f64> {
    let mut out = Vec::new();
    while let Ok(event) = sub.try_recv_event() {
        if let Some(v) = event.value().as_value().and_then(|v| v.to_f64()) {
            out.push(v);
        }
    }
    out
}

const ONE_D: &str = r#"record(sscan, "S") {
  field(MPTS, "10")
  field(NPTS, "5")
  field(P1PV, "M")
  field(P1SP, "1")
  field(P1EP, "5")
  field(D01PV, "D")
  field(T1PV, "D.PROC")
}"#;

#[epics_base_rs::epics_test]
async fn a_one_dimensional_scan_fills_positioner_and_detector_arrays() {
    let db = ioc(ONE_D).await;
    connected(&db, "S", &["P1NV", "D01NV", "T1NV"]).await;
    assert_eq!(f64_of(&db, "S.P1SI"), 1.0, "adjLinParms set the step");
    start(&db, "S").await;
    finished(&db, "S").await;
    assert_eq!(f64_of(&db, "S.CPT"), 5.0);
    assert_eq!(arr(&db, "S.P1RA", 5), [1.0, 2.0, 3.0, 4.0, 5.0]);
    assert_eq!(arr(&db, "S.D01DA", 5), [10.0, 20.0, 30.0, 40.0, 50.0]);
    // PASM STAY leaves the positioner at the last point.
    assert_eq!(f64_of(&db, "M"), 5.0);
    assert_eq!(f64_of(&db, "S.EXSC"), 0.0);
}

#[epics_base_rs::epics_test]
async fn a_second_scan_reports_its_own_data() {
    let db = ioc(ONE_D).await;
    connected(&db, "S", &["P1NV", "D01NV", "T1NV"]).await;
    start(&db, "S").await;
    finished(&db, "S").await;
    put(&db, "S.P1EP", EpicsValue::Double(3.0)).await;
    put(&db, "S.NPTS", EpicsValue::Long(3)).await;
    start(&db, "S").await;
    finished(&db, "S").await;
    assert_eq!(f64_of(&db, "S.CPT"), 3.0);
    assert_eq!(arr(&db, "S.D01DA", 3), [10.0, 20.0, 30.0]);
}

#[epics_base_rs::epics_test]
async fn start_pos_returns_the_positioner_after_the_scan() {
    let db = ioc(ONE_D).await;
    connected(&db, "S", &["P1NV", "D01NV", "T1NV"]).await;
    put(&db, "S.PASM", EpicsValue::Enum(1)).await;
    start(&db, "S").await;
    finished(&db, "S").await;
    wait_until("M back at P1SP", || f64_of(&db, "M") == 1.0).await;
}

#[epics_base_rs::epics_test]
async fn a_table_scan_visits_the_table() {
    let db = ioc(ONE_D).await;
    connected(&db, "S", &["P1NV", "D01NV", "T1NV"]).await;
    put(&db, "S.NPTS", EpicsValue::Long(3)).await;
    put(&db, "S.P1SM", EpicsValue::Enum(1)).await;
    put(
        &db,
        "S.P1PA",
        EpicsValue::DoubleArray(vec![3.0, 1.0, 2.0].into()),
    )
    .await;
    start(&db, "S").await;
    finished(&db, "S").await;
    assert_eq!(arr(&db, "S.P1RA", 3), [3.0, 1.0, 2.0]);
    assert_eq!(arr(&db, "S.D01DA", 3), [30.0, 10.0, 20.0]);
}

/// The fly readback of the first point is the start position. C reads the
/// buffer at `cpt - 1` there, the element before the array.
#[epics_base_rs::epics_test]
async fn a_fly_scan_records_the_start_as_its_first_readback() {
    let db = ioc(ONE_D).await;
    connected(&db, "S", &["P1NV", "D01NV", "T1NV"]).await;
    put(&db, "S.P1SM", EpicsValue::Enum(2)).await;
    // Start 3, end 5, five points: a step of 0.5, which no stale buffer
    // element plus the step reproduces.
    put(&db, "S.P1SP", EpicsValue::Double(3.0)).await;
    start(&db, "S").await;
    finished(&db, "S").await;
    assert_eq!(arr(&db, "S.P1RA", 5), [3.0, 3.5, 4.0, 4.5, 5.0]);
}

#[epics_base_rs::epics_test]
async fn an_operator_abort_stops_the_scan() {
    let db = ioc(ONE_D).await;
    connected(&db, "S", &["P1NV", "D01NV", "T1NV"]).await;
    put(&db, "S.NPTS", EpicsValue::Long(10)).await;
    put(&db, "S.PDLY", EpicsValue::Float(0.2)).await;
    start(&db, "S").await;
    wait_until("S.CPT >= 1", || f64_of(&db, "S.CPT") >= 1.0).await;
    put_field(&db, "S", "EXSC", EpicsValue::Short(0)).await;
    wait_until("S.BUSY", || f64_of(&db, "S.BUSY") == 0.0).await;
    // endScan's "SCAN Complete" replaces the abort message, as in C.
    assert!(f64_of(&db, "S.CPT") < 10.0);
    assert_eq!(f64_of(&db, "S.EXSC"), 0.0);
}

#[epics_base_rs::epics_test]
async fn a_pause_holds_the_scan_until_rescinded() {
    let db = ioc(ONE_D).await;
    connected(&db, "S", &["P1NV", "D01NV", "T1NV"]).await;
    put(&db, "S.PDLY", EpicsValue::Float(0.1)).await;
    start(&db, "S").await;
    wait_until("S.CPT >= 1", || f64_of(&db, "S.CPT") >= 1.0).await;
    put_field(&db, "S", "PAUS", EpicsValue::Short(1)).await;
    sleep(Duration::from_millis(300)).await;
    let held = f64_of(&db, "S.CPT");
    sleep(Duration::from_millis(300)).await;
    assert_eq!(f64_of(&db, "S.CPT"), held, "no point while paused");
    assert_eq!(f64_of(&db, "S.BUSY"), 1.0);
    put_field(&db, "S", "PAUS", EpicsValue::Short(0)).await;
    finished(&db, "S").await;
    assert_eq!(arr(&db, "S.D01DA", 5), [10.0, 20.0, 30.0, 40.0, 50.0]);
}

/// The positioner monitor takes the record lock while a scan is in flight
/// (`sscanRecord.c:2646` leaves it untaken); the scan completes.
#[epics_base_rs::epics_test]
async fn a_positioner_monitor_lands_during_a_scan() {
    let db = ioc(ONE_D).await;
    connected(&db, "S", &["P1NV", "D01NV", "T1NV"]).await;
    put(&db, "S.PDLY", EpicsValue::Float(0.2)).await;
    let mut cv = DbSubscription::subscribe(&db, "S.P1CV").await.unwrap();
    start(&db, "S").await;
    wait_until("S.CPT >= 1", || f64_of(&db, "S.CPT") >= 1.0).await;
    db.put_pv_and_post("M", EpicsValue::Double(42.0))
        .await
        .unwrap();
    finished(&db, "S").await;
    sleep(Duration::from_millis(50)).await;
    let seen = drain(&mut cv);
    assert!(seen.contains(&42.0), "P1CV events: {seen:?}");
    assert_eq!(f64_of(&db, "S.CPT"), 5.0);
}

/// C stores `scan = 0` (`sscanRecord.c:1571`) without `scanDelete`: SCAN
/// reads and posts Passive while the record stays on its periodic list.
#[epics_base_rs::epics_test]
async fn clear_all_pvs_drops_every_link_and_stores_scan_passive() {
    let db = ioc(&ONE_D.replace("field(MPTS", "field(SCAN, \"10 second\")\n  field(MPTS")).await;
    connected(&db, "S", &["P1NV", "D01NV", "T1NV"]).await;
    let mut scan = DbSubscription::subscribe(&db, "S.SCAN").await.unwrap();
    sleep(Duration::from_millis(50)).await;
    drain(&mut scan);
    put(&db, "S.P1SM", EpicsValue::Enum(1)).await;
    put(&db, "S.CMND", EpicsValue::Enum(3)).await;
    for f in ["P1PV", "D01PV", "T1PV"] {
        assert_eq!(str_of(&db, &format!("S.{f}")), "", "{f}");
    }
    for f in ["P1NV", "D01NV", "T1NV"] {
        assert_eq!(f64_of(&db, &format!("S.{f}")), 1.0, "{f} is NO_PV");
    }
    assert_eq!(f64_of(&db, "S.P1SM"), 0.0);
    assert_eq!(f64_of(&db, "S.SCAN"), 0.0, "stored by the put's special");
    sleep(Duration::from_millis(100)).await;
    assert_eq!(drain(&mut scan), [0.0]);
    let on_list = db
        .records_for_scan(ScanType::SEC10)
        .await
        .iter()
        .any(|n| n.as_bytes() == b"S");
    assert!(on_list, "no scanDelete ran");
}

/// One pass posts FAZE twice — INIT_SCAN, then MOVE_MOTORS — and a monitor
/// sees both values in order, not the last value twice.
#[epics_base_rs::epics_test]
async fn two_posts_of_one_field_in_one_pass_arrive_in_order() {
    let db = ioc(ONE_D).await;
    connected(&db, "S", &["P1NV", "D01NV", "T1NV"]).await;
    let mut faze = DbSubscription::subscribe(&db, "S.FAZE").await.unwrap();
    sleep(Duration::from_millis(50)).await;
    drain(&mut faze);
    start(&db, "S").await;
    finished(&db, "S").await;
    sleep(Duration::from_millis(50)).await;
    let seen = drain(&mut faze);
    assert_eq!(seen[..2], [1.0, 4.0], "FAZE events: {seen:?}");
}

/// Centre of mass moves every valid positioner to its own centre. C's
/// nested loop over `i` gives the first positioner its centre and leaves
/// the second where the scan ended.
#[epics_base_rs::epics_test]
async fn centre_of_mass_moves_each_positioner_to_its_own_centre() {
    let db = ioc(r#"record(sscan, "S") {
  field(MPTS, "10")
  field(NPTS, "5")
  field(P1PV, "M")
  field(P1SP, "1")
  field(P1EP, "5")
  field(P2PV, "M2")
  field(P2SP, "10")
  field(P2EP, "50")
  field(D01PV, "PEAK")
  field(T1PV, "PEAK.PROC")
  field(PASM, "CNTR OF MASS")
}"#)
    .await;
    connected(&db, "S", &["P1NV", "P2NV", "D01NV", "T1NV"]).await;
    start(&db, "S").await;
    finished(&db, "S").await;
    assert_eq!(arr(&db, "S.D01DA", 5), [3.0, 4.0, 5.0, 4.0, 3.0]);
    // The sums run over float detector data: equal to a few ulps.
    let near = |pv: &str, x: f64| (f64_of(&db, pv) - x).abs() < 1e-9;
    wait_until("M at its centre", || near("M", 3.0)).await;
    wait_until("M2 at its centre", || near("M2", 30.0)).await;
}

/// An array detector's data is read with a get the scan waits for. C counts
/// the get only when queueing it fails, so the scan reads whatever the
/// link's monitor last delivered.
/// C's RDLY branch posts PDLY.
/// adjLinParms' start-changed branch with the increment and centre frozen
/// and the point count free recomputes EP; C posts CP there and EP's monitor misses the change.
/// Detector fields report the detector's units, precision and range. C
/// tests the positioner range first and every detector field falls in it.
/// An outer scan triggers an inner one through `EXSC` and waits for it.
#[epics_base_rs::epics_test]
async fn a_two_dimensional_scan_fills_the_inner_scan_per_outer_point() {
    let db = ioc(r#"record(sscan, "S1") {
  field(MPTS, "10")
  field(NPTS, "3")
  field(P1PV, "M")
  field(P1SP, "1")
  field(P1EP, "3")
  field(D01PV, "D")
  field(T1PV, "D.PROC")
}
record(sscan, "S2") {
  field(MPTS, "10")
  field(NPTS, "2")
  field(P1PV, "M2")
  field(P1SP, "1")
  field(P1EP, "2")
  field(D01PV, "D")
  field(T1PV, "S1.EXSC")
}"#)
    .await;
    connected(&db, "S1", &["P1NV", "D01NV", "T1NV"]).await;
    connected(&db, "S2", &["P1NV", "D01NV", "T1NV"]).await;
    let mut inner = DbSubscription::subscribe(&db, "S1.D01DA").await.unwrap();
    sleep(Duration::from_millis(50)).await;
    while inner.try_recv_event().is_ok() {}
    start(&db, "S2").await;
    finished(&db, "S2").await;
    assert_eq!(f64_of(&db, "S2.CPT"), 2.0);
    assert_eq!(arr(&db, "S2.P1RA", 2), [1.0, 2.0]);
    // The outer detector reads D after each inner scan: M=3, M2=1 then 2.
    assert_eq!(arr(&db, "S2.D01DA", 2), [31.0, 32.0]);
    assert_eq!(arr(&db, "S1.D01DA", 3), [12.0, 22.0, 32.0]);
}

#[epics_base_rs::epics_test]
async fn an_rdly_write_posts_rdly_not_pdly() {
    let db = ioc(ONE_D).await;
    let mut pdly = DbSubscription::subscribe(&db, "S.PDLY").await.unwrap();
    let mut rdly = DbSubscription::subscribe(&db, "S.RDLY").await.unwrap();
    sleep(Duration::from_millis(50)).await;
    drain(&mut pdly);
    drain(&mut rdly);
    put(&db, "S.RDLY", EpicsValue::Float(-1.0)).await;
    sleep(Duration::from_millis(50)).await;
    assert_eq!(drain(&mut pdly), Vec::<f64>::new());
    let seen = drain(&mut rdly);
    assert!(
        !seen.is_empty() && seen.iter().all(|v| *v == 0.0),
        "{seen:?}"
    );
}

#[epics_base_rs::epics_test]
async fn a_frozen_centre_start_change_posts_the_new_end() {
    let db = ioc(ONE_D).await;
    put(&db, "S.FPTS", EpicsValue::Enum(0)).await;
    put(&db, "S.P1FI", EpicsValue::Enum(1)).await;
    put(&db, "S.P1FC", EpicsValue::Enum(1)).await;
    let mut ep = DbSubscription::subscribe(&db, "S.P1EP").await.unwrap();
    sleep(Duration::from_millis(50)).await;
    drain(&mut ep);
    // Centre 3, step 1: SP 2 needs 3 points, ending at 4.
    put(&db, "S.P1SP", EpicsValue::Double(2.0)).await;
    sleep(Duration::from_millis(50)).await;
    assert_eq!(f64_of(&db, "S.NPTS"), 3.0);
    assert_eq!(f64_of(&db, "S.P1EP"), 4.0);
    assert_eq!(drain(&mut ep), [4.0]);
}

#[epics_base_rs::epics_test]
async fn an_array_detector_is_read_after_its_get_completes() {
    let db = ioc(r#"record(waveform, "W") {
  field(FTVL, "DOUBLE")
  field(NELM, "5")
}
record(sscan, "S") {
  field(MPTS, "10")
  field(NPTS, "5")
  field(P1PV, "M")
  field(P1SP, "1")
  field(P1EP, "5")
  field(D01PV, "W")
}"#)
    .await;
    connected(&db, "S", &["P1NV", "D01NV"]).await;
    // A bare dbPut of a pp VAL posts nothing: the link's monitor buffer
    // keeps the connect-time array and only a get sees this one.
    put(
        &db,
        "W",
        EpicsValue::DoubleArray(vec![7.0, 6.0, 5.0, 4.0, 3.0].into()),
    )
    .await;
    start(&db, "S").await;
    finished(&db, "S").await;
    assert_eq!(arr(&db, "S.D01DA", 5), [7.0, 6.0, 5.0, 4.0, 3.0]);
}

#[epics_base_rs::epics_test]
async fn detector_fields_report_the_detector_metadata() {
    let db = ioc(r#"record(sscan, "S") {
  field(D01EU, "counts_long")
  field(D01PR, "2")
  field(D01HR, "100")
  field(D01LR, "-5")
}"#)
    .await;
    let rec = db.get_record("S").unwrap();
    let inst = rec.read();
    let md = inst.record.field_metadata_override("D01CV").unwrap();
    assert_eq!(md.units.unwrap().to_string(), "counts_");
    assert_eq!(md.precision, Some(2));
    assert_eq!(md.disp_limits, Some((100.0, -5.0)));
}

#[epics_base_rs::epics_test]
async fn a_local_readback_and_detector_are_read_by_db_get() {
    let db = ioc(r#"record(ai, "DET") {
  field(EGU, "millimetre")
  field(PREC, "4")
  field(HOPR, "50")
  field(LOPR, "-50")
}
record(sscan, "S") {
  field(MPTS, "10")
  field(NPTS, "3")
  field(P1PV, "M")
  field(P1SP, "1")
  field(P1EP, "3")
}"#)
    .await;
    connected(&db, "S", &["P1NV"]).await;
    // `dbNameToAddr` found them: the search callback ran inside the put,
    // with no link to connect.
    put(&db, "S.R1PV", EpicsValue::String("M".into())).await;
    assert_eq!(f64_of(&db, "S.R1NV"), 0.0);
    put(&db, "S.D01PV", EpicsValue::String("DET".into())).await;
    assert_eq!(f64_of(&db, "S.D01NV"), 0.0);
    // `dbGet`'s DBR_UNITS, not `recDynLinkGetUnits(.., 8)`'s seven bytes.
    assert_eq!(str_of(&db, "S.D01EU"), "millimetre");
    assert_eq!(f64_of(&db, "S.D01PR"), 4.0);
    assert_eq!(
        (f64_of(&db, "S.D01LR"), f64_of(&db, "S.D01HR")),
        (-50.0, 50.0)
    );
    put(&db, "DET", EpicsValue::Double(2.5)).await;
    start(&db, "S").await;
    finished(&db, "S").await;
    assert_eq!(arr(&db, "S.P1RA", 3), [1.0, 2.0, 3.0]);
    assert_eq!(arr(&db, "S.D01DA", 3), [2.5, 2.5, 2.5]);
}

/// The errlog lines naming `who` from here on.
fn errlog_lines(who: &'static str) -> Arc<std::sync::Mutex<Vec<String>>> {
    let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = lines.clone();
    epics_base_rs::runtime::log::errlog_add_listener(move |m| {
        if m.contains(who) {
            sink.lock().unwrap().push(m.to_owned());
        }
    });
    lines
}

fn logged(lines: &std::sync::Mutex<Vec<String>>, what: &str) -> bool {
    epics_base_rs::runtime::log::errlog_flush();
    lines.lock().unwrap().iter().any(|l| l.contains(what))
}

#[epics_base_rs::epics_test]
async fn a_pv_name_written_during_a_connect_that_never_lands_waits_it_out() {
    let db = ioc(r#"record(sscan, "S") {}"#).await;
    let lines = errlog_lines("S:special:connect");
    put(&db, "S.D01PV", EpicsValue::String("NOBODY".into())).await;
    let t0 = std::time::Instant::now();
    put(&db, "S.D01PV", EpicsValue::String("D".into())).await;
    let waited = t0.elapsed();
    assert!(waited >= Duration::from_millis(900), "{waited:?}");
    assert!(logged(&lines, "Waiting..."));
    assert!(logged(&lines, "Trying new PV name."));
    assert_eq!(f64_of(&db, "S.D01NV"), 0.0);
}

#[epics_base_rs::epics_test]
async fn a_pv_name_written_during_a_connect_waits_for_it_to_land() {
    let db = ioc(r#"record(sscan, "S") {}"#).await;
    let lines = errlog_lines("S:special:connect");
    put(&db, "S.P1PV", EpicsValue::String("M".into())).await;
    let db2 = db.clone();
    let (done, renamed) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let t0 = std::time::Instant::now();
        epics_base_rs::runtime::task::test_block_on(async move {
            db2.put_pv("S.P1PV", EpicsValue::String("M2".into()))
                .await
                .unwrap()
        });
        let _ = done.send(t0.elapsed());
    });
    let waited = renamed.await.unwrap();
    // The connect lands before the rename, between C's "Waiting..." and its
    // first check (no sleep: links.rs pins it), or inside the first second.
    assert!(waited < Duration::from_millis(1900), "{waited:?}");
    assert!(!logged(&lines, "Trying new PV name."));
    connected(&db, "S", &["P1NV"]).await;
    db.put_pv_and_post("M2", EpicsValue::Double(4.0))
        .await
        .unwrap();
    wait_until("P1CV", || f64_of(&db, "S.P1CV") == 4.0).await;
}

#[epics_base_rs::epics_test]
async fn a_local_detector_writing_into_the_scan_while_it_reads_does_not_deadlock() {
    let db = ioc(r#"record(calcout, "DET") {
  field(INPA, "DET")
  field(CALC, "A+1")
  field(OUT, "S.DDLY CA")
  field(OOPT, "Every Time")
}
record(sscan, "S") {
  field(MPTS, "2000")
  field(NPTS, "2000")
  field(P1PV, "M")
  field(P1SP, "0")
  field(P1EP, "1")
  field(R1PV, "DET")
  field(D01PV, "DET")
}"#)
    .await;
    connected(&db, "S", &["P1NV", "R1NV", "D01NV"]).await;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // DET processes, under its own lock, and its OUT puts into S, while the
    // scan holds S and reads DET.
    let hammer = {
        let (db, stop) = (db.clone(), stop.clone());
        std::thread::spawn(move || {
            epics_base_rs::runtime::task::test_block_on(async move {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    db.put_record_field_from_ca("DET", "PROC", EpicsValue::Short(1))
                        .await
                        .unwrap();
                }
            })
        })
    };
    start(&db, "S").await;
    finished(&db, "S").await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    hammer.join().unwrap();
    assert!(f64_of(&db, "DET") > 0.0);
}

#[epics_base_rs::epics_test]
async fn two_scans_reading_each_other_locally_do_not_deadlock() {
    let db = ioc(r#"record(sscan, "S1") {
  field(MPTS, "2000")
  field(NPTS, "2000")
  field(P1PV, "M")
  field(P1SP, "0")
  field(P1EP, "1")
  field(D01PV, "S2.CPT")
}
record(sscan, "S2") {
  field(PRIO, "HIGH")
  field(MPTS, "2000")
  field(NPTS, "2000")
  field(P1PV, "M2")
  field(P1SP, "0")
  field(P1EP, "1")
  field(D01PV, "S1.CPT")
}"#)
    .await;
    connected(&db, "S1", &["P1NV", "D01NV"]).await;
    connected(&db, "S2", &["P1NV", "D01NV"]).await;
    start(&db, "S1").await;
    start(&db, "S2").await;
    finished(&db, "S1").await;
    finished(&db, "S2").await;
}

#[epics_base_rs::epics_test]
async fn two_scans_naming_each_other_at_once_do_not_deadlock() {
    let db = ioc(r#"record(sscan, "S1") {}
record(sscan, "S2") {}"#)
    .await;
    // Each `special` holds its own record and resolves the other's field.
    let rename = |me: &'static str, other: &'static str| {
        let db = db.clone();
        std::thread::spawn(move || {
            epics_base_rs::runtime::task::test_block_on(async move {
                for i in 0..20000 {
                    let pv = if i % 2 == 0 { other } else { "" };
                    db.put_pv(me, EpicsValue::String(pv.into())).await.unwrap();
                }
            })
        })
    };
    let a = rename("S1.D01PV", "S2.CPT");
    let b = rename("S2.D01PV", "S1.CPT");
    wait_until("renames", || a.is_finished() && b.is_finished()).await;
}

/// Every event `sub` has queued, array or scalar.
fn events(sub: &mut DbSubscription) -> usize {
    let mut n = 0;
    while sub.try_recv_event().is_ok() {
        n += 1;
    }
    n
}

const POSTED: &str = r#"
record(sscan, "S") {
  field(MPTS, "10")
  field(NPTS, "4")
  field(P1PV, "M")
  field(P1SP, "1")
  field(P1EP, "4")
  field(D01PV, "D")
  field(T1PV, "D.PROC")
  field(PDLY, "0.1")
  field(ATIME, "0.1")
}"#;

/// C's array posts name `posBufPtr[i].pBufA`/`pBufB`; `PnCA`'s address is
/// `p_ca`, which nothing sets, so no post reaches it (sscanRecord.c:545,
/// :1880, :2213-2219, :5373-5374).
#[epics_base_rs::epics_test]
async fn no_scan_or_preview_post_reaches_a_positioner_ca() {
    let db = ioc(POSTED).await;
    connected(&db, "S", &["P1NV", "D01NV", "T1NV"]).await;
    let mut ra = DbSubscription::subscribe(&db, "S.P1RA").await.unwrap();
    let mut ca = DbSubscription::subscribe(&db, "S.P1CA").await.unwrap();
    sleep(Duration::from_millis(50)).await;
    events(&mut ra);
    events(&mut ca);
    start(&db, "S").await;
    finished(&db, "S").await;
    put_field(&db, "S", "CMND", EpicsValue::Enum(2)).await;
    sleep(Duration::from_millis(100)).await;
    assert!(events(&mut ra) > 0);
    assert_eq!(events(&mut ca), 0);
}

/// C's `nullArray` post reaches only detectors without buffers; D01-D04
/// have theirs from `init_record`, so an unacquired D02 hears nothing
/// (sscanRecord.c:836-844, :2213-2221).
#[epics_base_rs::epics_test]
async fn the_null_array_post_reaches_only_detectors_without_buffers() {
    let db = ioc(POSTED).await;
    connected(&db, "S", &["P1NV", "D01NV", "T1NV"]).await;
    let mut subs = Vec::new();
    for f in ["D02DA", "D02CA", "D05DA", "D05CA"] {
        subs.push(
            DbSubscription::subscribe(&db, &format!("S.{f}"))
                .await
                .unwrap(),
        );
    }
    sleep(Duration::from_millis(50)).await;
    for s in &mut subs {
        events(s);
    }
    start(&db, "S").await;
    finished(&db, "S").await;
    put_field(&db, "S", "CMND", EpicsValue::Enum(2)).await;
    sleep(Duration::from_millis(100)).await;
    let n: Vec<usize> = subs.iter_mut().map(events).collect();
    assert_eq!(n[..2], [0, 0], "unacquired D02");
    assert!(n[2] > 0 && n[3] > 0, "bufferless D05: {n:?}");
}

/// Adding to the previous scan, C reads every array detector into one
/// shared `dataBuffer`; a failed get (a STRING waveform read as DBR_FLOAT)
/// leaves the previous detector's read there (sscanRecord.c:3777-3806).
#[epics_base_rs::epics_test]
async fn a_failed_array_get_while_adding_keeps_the_shared_buffer() {
    let db = ioc(r#"
record(waveform, "W1") {
  field(FTVL, "DOUBLE")
  field(NELM, "4")
}
record(waveform, "WS") {
  field(FTVL, "STRING")
  field(NELM, "4")
}
record(sscan, "S") {
  field(MPTS, "4")
  field(NPTS, "4")
  field(ACQT, "1D ARRAY")
  field(ACQM, "ADD TO PREV")
  field(D01PV, "W1")
  field(D02PV, "WS")
}"#)
    .await;
    put(
        &db,
        "W1",
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0].into()),
    )
    .await;
    put(
        &db,
        "WS",
        EpicsValue::StringArray(
            ["abc", "def", "ghi", "jkl"]
                .map(epics_base_rs::types::PvString::from)
                .to_vec()
                .into(),
        ),
    )
    .await;
    connected(&db, "S", &["D01NV", "D02NV"]).await;
    for sum in [[1.0, 2.0, 3.0, 4.0], [2.0, 4.0, 6.0, 8.0]] {
        start(&db, "S").await;
        finished(&db, "S").await;
        assert_eq!(arr(&db, "S.D01DA", 4), sum);
        assert_eq!(arr(&db, "S.D02DA", 4), sum);
    }
}

// The record's scanOnce sites (sscanRecord.c's 15 calls), one case each
// where a missing call or a wrong condition hangs or retimes the scan.
// Reached by every step scan above: the trigger notify (2498) and the
// positioner notify (2547); by start_pos_returns_.. the retrace (2538).
// doPuts' DO_PUTS_TRIG (4403) shares the trigger tail with 2498 and needs
// every trigger put to fail or complete inside doPuts, which a connected
// local trigger never does.

const NO_TRIG: &str = r#"record(sscan, "S") {
  field(MPTS, "10")
  field(NPTS, "3")
  field(P1PV, "M")
  field(P1SP, "1")
  field(P1EP, "3")
  field(D01PV, "M")
}"#;

/// No trigger PV: a step scan falls through to the read and never waits
/// DDLY (contScan, 3388-3395); a fly scan's first point goes through doPuts,
/// which schedules the read itself (4336), at once with DDLY 0 and through
/// delayCallback (2431) after DDLY otherwise.
#[epics_base_rs::epics_test]
async fn with_no_trigger_only_a_fly_start_waits_ddly() {
    for (fly, ddly) in [(false, 0.3f32), (true, 0.0), (true, 0.3)] {
        let db = ioc(NO_TRIG).await;
        connected(&db, "S", &["P1NV", "D01NV"]).await;
        if fly {
            put(&db, "S.P1SM", EpicsValue::Enum(2)).await;
        }
        put(&db, "S.DDLY", EpicsValue::Float(ddly)).await;
        let t0 = std::time::Instant::now();
        start(&db, "S").await;
        finished(&db, "S").await;
        let took = t0.elapsed().as_secs_f64();
        let waits = fly && ddly > 0.0;
        assert_eq!(took >= 0.3, waits, "fly {fly} DDLY {ddly}: took {took}");
    }
}

/// The trigger's notify with DDLY set goes through delayCallback (2498's
/// delayed branch, then 2431).
#[epics_base_rs::epics_test]
async fn a_trigger_notify_with_ddly_reads_after_the_delay() {
    let db = ioc(ONE_D).await;
    connected(&db, "S", &["P1NV", "D01NV", "T1NV"]).await;
    put(&db, "S.NPTS", EpicsValue::Long(3)).await;
    put(&db, "S.DDLY", EpicsValue::Float(0.3)).await;
    let t0 = std::time::Instant::now();
    start(&db, "S").await;
    finished(&db, "S").await;
    assert!(t0.elapsed().as_secs_f64() >= 0.9);
    assert_eq!(arr(&db, "S.D01DA", 3), [10.0, 30.0, 50.0]);
}

/// AWCT holds each point for a client (WTNG); the client's WAIT=0 is what
/// moves the scan on (special WAIT, 1651).
#[epics_base_rs::epics_test]
async fn a_client_wait_holds_each_point_until_the_client_releases_it() {
    let db = ioc(ONE_D).await;
    connected(&db, "S", &["P1NV", "D01NV", "T1NV"]).await;
    put(&db, "S.NPTS", EpicsValue::Long(2)).await;
    put_field(&db, "S", "AWCT", EpicsValue::Short(1)).await;
    start(&db, "S").await;
    for point in 1..=2 {
        wait_until("S.WTNG", || f64_of(&db, "S.WTNG") == 1.0).await;
        sleep(Duration::from_millis(200)).await;
        assert_eq!(f64_of(&db, "S.CPT"), f64::from(point - 1), "held");
        put_field(&db, "S", "WAIT", EpicsValue::Short(0)).await;
        wait_until("S.CPT", || f64_of(&db, "S.CPT") == f64::from(point)).await;
    }
    finished(&db, "S").await;
}

/// AWAIT holds the end of the scan in SAVE_DATA_WAIT; clearing it ends the
/// scan (special AWAIT, 1663).
#[epics_base_rs::epics_test]
async fn await_holds_the_scan_end_until_cleared() {
    let db = ioc(ONE_D).await;
    connected(&db, "S", &["P1NV", "D01NV", "T1NV"]).await;
    put(&db, "S.NPTS", EpicsValue::Long(2)).await;
    put_field(&db, "S", "AWAIT", EpicsValue::Short(1)).await;
    start(&db, "S").await;
    wait_until("S.CPT", || f64_of(&db, "S.CPT") == 2.0).await;
    sleep(Duration::from_millis(300)).await;
    assert_eq!(f64_of(&db, "S.BUSY"), 1.0, "held for the client");
    put_field(&db, "S", "AWAIT", EpicsValue::Short(0)).await;
    finished(&db, "S").await;
}

/// A pause rescinded with RDLY resumes through delayCallback after RDLY,
/// not at once (special PAUS, 1529's delayed branch).
#[epics_base_rs::epics_test]
async fn a_rescinded_pause_waits_rdly_before_resuming() {
    let db = ioc(ONE_D).await;
    connected(&db, "S", &["P1NV", "D01NV", "T1NV"]).await;
    put(&db, "S.PDLY", EpicsValue::Float(0.2)).await;
    put(&db, "S.RDLY", EpicsValue::Float(0.8)).await;
    start(&db, "S").await;
    wait_until("S.CPT >= 1", || f64_of(&db, "S.CPT") >= 1.0).await;
    put_field(&db, "S", "PAUS", EpicsValue::Short(1)).await;
    sleep(Duration::from_millis(500)).await;
    let held = f64_of(&db, "S.CPT");
    put_field(&db, "S", "PAUS", EpicsValue::Short(0)).await;
    sleep(Duration::from_millis(400)).await;
    assert_eq!(f64_of(&db, "S.CPT"), held, "RDLY not yet out");
    finished(&db, "S").await;
    assert_eq!(arr(&db, "S.D01DA", 5), [10.0, 20.0, 30.0, 40.0, 50.0]);
}

/// The array-read trigger's notify ends the scan (2518).
#[epics_base_rs::epics_test]
async fn an_array_read_trigger_notify_ends_the_scan() {
    let db = ioc(&format!(
        r#"{ONE_D}
record(ao, "AR") {{}}"#
    ))
    .await;
    put(&db, "S.A1PV", EpicsValue::String("AR".into())).await;
    put(&db, "S.A1CD", EpicsValue::Float(7.0)).await;
    connected(&db, "S", &["P1NV", "D01NV", "T1NV", "A1NV"]).await;
    start(&db, "S").await;
    finished(&db, "S").await;
    assert_eq!(f64_of(&db, "AR"), 7.0);
}

/// An after-scan link with ASWAIT=Wait ends the scan from its notify (2543).
#[epics_base_rs::epics_test]
async fn a_waited_after_scan_link_ends_the_scan_from_its_notify() {
    let db = ioc(&format!(
        r#"{ONE_D}
record(ao, "AS") {{}}"#
    ))
    .await;
    put(&db, "S.ASPV", EpicsValue::String("AS".into())).await;
    put(&db, "S.ASCD", EpicsValue::Float(3.0)).await;
    connected(&db, "S", &["P1NV", "D01NV", "T1NV", "ASNV"]).await;
    start(&db, "S").await;
    finished(&db, "S").await;
    assert_eq!(f64_of(&db, "AS"), 3.0);
}

/// An after-scan link with ASWAIT=NoWait leaves doPuts nothing to wait
/// for: doPuts itself ends the scan (4514).
#[epics_base_rs::epics_test]
async fn an_unwaited_after_scan_link_ends_the_scan_from_do_puts() {
    let db = ioc(&format!(
        r#"{ONE_D}
record(ao, "AS") {{}}"#
    ))
    .await;
    put(&db, "S.ASPV", EpicsValue::String("AS".into())).await;
    put(&db, "S.ASCD", EpicsValue::Float(3.0)).await;
    put(&db, "S.ASWAIT", EpicsValue::Enum(1)).await;
    connected(&db, "S", &["P1NV", "D01NV", "T1NV", "ASNV"]).await;
    start(&db, "S").await;
    finished(&db, "S").await;
    wait_until("AS", || f64_of(&db, "AS") == 3.0).await;
}

/// A scan started while its trigger link connects is pending; the
/// trigger's search callback starts it (pvSearchCallback, 2939).
#[epics_base_rs::epics_test]
async fn a_scan_pending_its_trigger_starts_from_the_search_callback() {
    let db = ioc(r#"record(sscan, "S") {
  field(MPTS, "10")
  field(NPTS, "3")
  field(P1PV, "M")
  field(P1SP, "1")
  field(P1EP, "3")
  field(D01PV, "D")
}"#)
    .await;
    connected(&db, "S", &["P1NV", "D01NV"]).await;
    put(&db, "S.T1PV", EpicsValue::String("D.PROC".into())).await;
    start(&db, "S").await;
    finished(&db, "S").await;
    assert_eq!(arr(&db, "S.D01DA", 3), [10.0, 20.0, 30.0]);
}

/// A scan started while its positioner connects is pending until the
/// positioner's confirming get lands (posMonCallbackGetCB, 3070).
#[epics_base_rs::epics_test]
async fn a_scan_pending_its_positioner_starts_from_the_monitor_get() {
    let db = ioc(ONE_D).await;
    connected(&db, "S", &["P1NV", "D01NV", "T1NV"]).await;
    put(&db, "S.NPTS", EpicsValue::Long(3)).await;
    put(&db, "S.P1PV", EpicsValue::String("M2".into())).await;
    start(&db, "S").await;
    finished(&db, "S").await;
    assert_eq!(arr(&db, "S.D01DA", 3), [1.0, 3.0, 5.0]);
}

/// A detector the database does not resolve by name (a filtered channel)
/// is an array read through its link; the get's callback ends the scan
/// (userGetCallback, 2612).
#[epics_base_rs::epics_test]
async fn a_linked_array_detector_ends_the_scan_from_its_get_callback() {
    let db = ioc(r#"record(waveform, "W") {
  field(FTVL, "DOUBLE")
  field(NELM, "5")
}
record(sscan, "S") {
  field(MPTS, "10")
  field(NPTS, "5")
  field(P1PV, "M")
  field(P1SP, "1")
  field(P1EP, "5")
}"#)
    .await;
    put(
        &db,
        "S.D01PV",
        EpicsValue::String(r#"W.VAL{"arr":{}}"#.into()),
    )
    .await;
    connected(&db, "S", &["P1NV", "D01NV"]).await;
    put(
        &db,
        "W",
        EpicsValue::DoubleArray(vec![7.0, 6.0, 5.0, 4.0, 3.0].into()),
    )
    .await;
    start(&db, "S").await;
    finished(&db, "S").await;
    assert_eq!(arr(&db, "S.D01DA", 5), [7.0, 6.0, 5.0, 4.0, 3.0]);
}

/// A trigger still busy with a killed scan's put-callback refuses this one,
/// so no notify will count the triggers down: doPuts schedules the read
/// itself (DO_PUTS_TRIG, 4403) and the scan runs on without the trigger.
#[epics_base_rs::epics_test]
async fn a_busy_trigger_leaves_do_puts_to_schedule_the_read() {
    let db = ioc(r#"record(calcout, "TR") {
  field(CALC, "1")
  field(ODLY, "2")
}
record(sscan, "S") {
  field(MPTS, "10")
  field(NPTS, "3")
  field(P1PV, "M")
  field(P1SP, "1")
  field(P1EP, "3")
  field(D01PV, "D")
  field(T1PV, "TR.PROC")
}"#)
    .await;
    connected(&db, "S", &["P1NV", "D01NV", "T1NV"]).await;
    start(&db, "S").await;
    // The first point's trigger put waits on TR's two-second ODLY. An abort
    // waits for it; a second EXSC=0 kills the scan without waiting.
    wait_until("TR.PACT", || f64_of(&db, "TR.PACT") == 1.0).await;
    put_field(&db, "S", "EXSC", EpicsValue::Short(0)).await;
    put_field(&db, "S", "EXSC", EpicsValue::Short(0)).await;
    wait_until("S.BUSY", || f64_of(&db, "S.BUSY") == 0.0).await;
    assert_eq!(f64_of(&db, "TR.PACT"), 1.0, "TR still owes its callback");
    let t0 = std::time::Instant::now();
    start(&db, "S").await;
    finished(&db, "S").await;
    assert!(t0.elapsed() < Duration::from_millis(1500), "waited for TR");
    assert_eq!(f64_of(&db, "S.CPT"), 3.0);
    assert_eq!(f64_of(&db, "TR.PACT"), 1.0, "TR never took this scan's put");
}
