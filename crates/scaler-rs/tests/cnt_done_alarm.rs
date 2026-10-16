//! The control scaler `SC` of the C oracle's `scaler-c8.db` (no COUTP), driven
//! through a user count at `DLY 0` and then `DLY 0.5` in ONE IOC, compared post
//! for post against `oracleScalerIoc st-scaler.cmd` (scaler 913e2e4, base
//! 8f5015b66) under `camonitor -m va SC.CNT SC.US SC.SS SC.S1 SCF`:
//!
//! ```text
//! SC.CNT Count UDF INVALID     <- DLY 0: the CNT put
//! SC.S1  1e+07 UDF INVALID
//! SC.CNT Done  UDF INVALID     <- scalerRecord.c:372, before monitor()
//! SCF    1
//! SC.CNT Count                 <- DLY 0.5
//! SC.S1  0
//! SC.S1  1e+07
//! SC.CNT Done
//! SCF    2
//! ```
//!
//! No `SC.US` / `SC.SS` post follows the connect values. The record never
//! leaves its init alarm until the first count completes: the count-start
//! cycle skips the "done counting?" block (`:470`), so `monitor()` and the
//! `recGblResetAlarms` it opens with (`:764`) do not run, and on the
//! completion cycle `:372` posts `CNT` ahead of them.
//!
//! `DTYP "Asyn Scaler"` is served the way no production crate serves it yet:
//! `ScalerAsynDeviceSupport` over a `SoftScalerDriver`, registered here. The
//! driver's shared channel-0 count stands in for C's `dbpf c1 10000000`.
//!
//! Cross-field arrival order is not compared: the rs CA server drains each
//! subscription on its own task, so only per-field order is a property of
//! the record.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset};
use epics_base_rs::types::{DbFieldType, EpicsValue};
use scaler_rs::MAX_SCALER_CHANNELS;
use scaler_rs::device_support::scaler_asyn::ScalerAsynDeviceSupport;
use scaler_rs::device_support::scaler_soft::SoftScalerDriver;

/// `scalerSupport.dbd`'s `device(scaler,INST_IO,devScalerAsyn,"Asyn Scaler")`.
static ASYN_SCALER: DevSup = DevSup {
    name: "devScalerAsyn",
    choice: "Asyn Scaler",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(7, &[2, 4, 5, 6], false),
};

// RTEMS-EXEC-MODEL-ALLOW(2): checked, not waived — both ran on the exec
// backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p scaler-rs
// --test cnt_done_alarm --run-ignored all`): the first passed, the ignored pin
// failed on the same :470 CNT post it fails on tokio. No CA server is built;
// the reactor comes from `#[tokio::test]`, which the backend does not remove.

const UDF_ALARM: u16 = 17;
const INVALID_ALARM: u16 = 3;

const DB: &str = r#"
record(scaler, "SC") {
    field(DTYP, "Asyn Scaler")
    field(OUT,  "@asyn(scalerPortC 0 0)")
    field(FREQ, "10000000")
    field(TP,   "1")
    field(DLY,  "0")
    field(FLNK, "SCF")
}
record(calc, "SCF") {
    field(CALC, "A+1")
    field(INPA, "SCF")
}
"#;

type Counts = Arc<Mutex<[u32; MAX_SCALER_CHANNELS]>>;

async fn boot() -> (Arc<PvDatabase>, Counts) {
    let counts: Arc<Mutex<Option<Counts>>> = Arc::new(Mutex::new(None));
    let slot = counts.clone();
    let (name, factory) = scaler_rs::scaler_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_device_support("scaler", &ASYN_SCALER, move || {
            let driver = SoftScalerDriver::new(8);
            *slot.lock().unwrap() = Some(driver.shared_counts());
            Box::new(ScalerAsynDeviceSupport::new(Box::new(driver)))
        })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let counts = counts.lock().unwrap().take().expect("SC bound its DTYP");
    // `iocRun`: the done interrupt processes the record only once
    // `interruptAccept` is set.
    db.scan_control().run();
    (db, counts)
}

/// `camonitor`'s default mask.
fn watch(db: &PvDatabase, rec: &str, field: &str, sid: u32) -> EventReader {
    let cell = db.get_record(rec).unwrap();
    let mut inst = cell.write();
    inst.add_subscriber(
        field,
        sid,
        DbFieldType::Double,
        (EventMask::VALUE | EventMask::ALARM).bits(),
    )
    .expect("subscription accepted")
}

/// Every pending event as `(value, stat, sevr)`.
fn drain(rx: &mut EventReader) -> Vec<(f64, u16, u16)> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        let v = ev.value().as_value().and_then(EpicsValue::to_f64).unwrap();
        out.push((v, ev.stat(), ev.sevr()));
    }
    out
}

async fn put(db: &PvDatabase, rec: &str, field: &str, v: EpicsValue) {
    db.put_record_field_from_ca(rec, field, v).await.unwrap();
}

/// Every post of `SC.CNT`, `SC.US`, `SC.SS`, `SC.S1` and `SCF` across the
/// two counts.
struct Posts {
    cnt: Vec<(f64, u16, u16)>,
    us: Vec<(f64, u16, u16)>,
    ss: Vec<(f64, u16, u16)>,
    s1: Vec<(f64, u16, u16)>,
    scf: Vec<(f64, u16, u16)>,
}

async fn two_counts() -> Posts {
    let (db, counts) = boot().await;
    let mut cnt = watch(&db, "SC", "CNT", 1);
    let mut us = watch(&db, "SC", "US", 2);
    let mut ss = watch(&db, "SC", "SS", 3);
    let mut s1 = watch(&db, "SC", "S1", 4);
    let mut scf = watch(&db, "SCF", "VAL", 5);

    for dly in [0.0, 0.5] {
        put(&db, "SC", "DLY", EpicsValue::Double(dly)).await;
        put(&db, "SC", "CNT", EpicsValue::Short(1)).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        // C `dbpf c1 10000000`: the count source reaches PR1.
        counts.lock().unwrap()[0] = 10_000_000;
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
    Posts {
        cnt: drain(&mut cnt),
        us: drain(&mut us),
        ss: drain(&mut ss),
        s1: drain(&mut s1),
        scf: drain(&mut scf),
    }
}

/// C reads the counters at `:453`, after the count start's reset and arm
/// (`:391-433`), so a count start shows the cleared counters: the DLY 0.5
/// start posts `0`, never the previous count's `1e+07`. The second `1e+07` is
/// the first done cycle's alarm-reset sweep (CBUG-B19, 46be9a081).
#[tokio::test(flavor = "multi_thread")]
async fn a_count_start_reads_the_counters_its_arm_cleared() {
    let s1: Vec<f64> = two_counts().await.s1.iter().map(|p| p.0).collect();
    assert_eq!(s1, [1e7, 1e7, 0.0, 1e7], "S1");
}

#[tokio::test(flavor = "multi_thread")]
async fn control_scaler_matches_the_c_oracle_post_for_post() {
    let posts = two_counts().await;
    let udf = (UDF_ALARM, INVALID_ALARM);
    let ok = (0, 0);
    let ev = |v: f64, (st, sv): (u16, u16)| (v, st, sv);
    assert_eq!(
        posts.cnt,
        [ev(1.0, udf), ev(0.0, udf), ev(1.0, ok), ev(0.0, ok)],
        "CNT: Count, Done UDF INVALID (:372 precedes monitor()), Count, Done"
    );
    assert_eq!(posts.us, [], "US is never posted");
    assert_eq!(posts.ss, [], "SS is never posted");
    assert_eq!(
        posts.s1,
        [ev(1e7, udf), ev(1e7, ok), ev(0.0, ok), ev(1e7, ok)],
        "S1, with the alarm-reset sweep post C drops (CBUG-B19, 46be9a081)"
    );
    assert_eq!(posts.scf, [ev(1.0, ok), ev(2.0, ok)], "SCF (FLNK)");
}
