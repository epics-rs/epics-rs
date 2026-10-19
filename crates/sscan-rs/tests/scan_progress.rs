//! `scanProg.st` run against the shipped `scanProgress.db`, with stand-in
//! `sscan` records carrying the four fields the program reads.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::task::{Reactor, sleep};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{FieldDesc, ProcessOutcome, Record};
use epics_base_rs::server::records::{
    ao::AoRecord, bo::BoRecord, longout::LongoutRecord, mbbo::MbboRecord,
    stringout::StringoutRecord,
};
use epics_base_rs::types::{DbFieldType, EpicsValue};
use sscan_rs::scan_prog::{self, ScanProgressConfig};

const P: &str = "prog:";
const S: &str = "S:";

/// BUSY, PAUS, NPTS and CPT of an sscan record, and nothing else.
static SCAN_STUB_FIELDS: &[FieldDesc] = &[
    FieldDesc::new("BUSY", DbFieldType::Char, false),
    FieldDesc::new("PAUS", DbFieldType::Enum, false),
    FieldDesc::new("NPTS", DbFieldType::Long, false),
    FieldDesc::new("CPT", DbFieldType::Long, false),
];

#[derive(Default)]
struct ScanStub {
    busy: u8,
    paus: u16,
    npts: i32,
    cpt: i32,
}

impl Record for ScanStub {
    fn record_type(&self) -> &'static str {
        "sscan"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        Some(match name {
            "BUSY" => EpicsValue::Char(self.busy),
            "PAUS" => EpicsValue::Enum(self.paus),
            "NPTS" => EpicsValue::Long(self.npts),
            "CPT" => EpicsValue::Long(self.cpt),
            _ => return None,
        })
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        let v = value.to_f64().unwrap_or(0.0);
        match name {
            "BUSY" => self.busy = v as u8,
            "PAUS" => self.paus = v as u16,
            "NPTS" => self.npts = v as i32,
            "CPT" => self.cpt = v as i32,
            _ => {}
        }
        Ok(())
    }

    fn declared_fields(&self) -> &'static [FieldDesc] {
        SCAN_STUB_FIELDS
    }
}

async fn ioc() -> Arc<PvDatabase> {
    sscan_rs::register_sscan_dbd();
    let template = std::fs::read_to_string(format!("{}/scanProgress.db", sscan_rs::SSCAN_DB_DIR))
        .expect("scanProgress.db");
    let scans: String = (1..=4)
        .map(|n| format!("record(sscan, \"{S}scan{n}\") {{}}\n"))
        .collect();
    let macros: HashMap<String, String> = [("P".to_string(), P.to_string())].into();
    let (db, _) = IocBuilder::new()
        .register_record_type("sscan", || Box::new(ScanStub::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .register_record_type("bo", || Box::new(BoRecord::default()))
        .register_record_type("longout", || Box::new(LongoutRecord::default()))
        .register_record_type("mbbo", || Box::new(MbboRecord::default()))
        .register_record_type("stringout", || Box::new(StringoutRecord::default()))
        .db_string(&format!("{template}\n{scans}"), &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db
}

fn start(db: &Arc<PvDatabase>) {
    let db = (**db).clone();
    // Detached: the program runs for the rest of the test.
    drop(
        Reactor::current()
            .expect("a runtime")
            .spawn(scan_prog::run(ScanProgressConfig::new(P, S), db)),
    );
}

fn f(db: &PvDatabase, leaf: &str) -> f64 {
    db.get_pv(format!("{P}{leaf}")).unwrap().to_f64().unwrap()
}

fn s(db: &PvDatabase, leaf: &str) -> String {
    match db.get_pv(format!("{P}{leaf}")).unwrap() {
        EpicsValue::String(v) => v.to_string(),
        other => panic!("{leaf}: {other:?}"),
    }
}

/// A scan record field changing, posted as the record would.
async fn scan(db: &PvDatabase, n: usize, field: &str, v: f64) {
    db.put_pv_and_post(format!("{S}scan{n}.{field}"), EpicsValue::Double(v))
        .await
        .unwrap();
}

/// Poll until `probe` holds, for up to `secs`.
async fn eventually(secs: u64, mut probe: impl FnMut() -> bool) -> bool {
    for _ in 0..secs * 100 {
        if probe() {
            return true;
        }
        sleep(Duration::from_millis(10)).await;
    }
    probe()
}

/// `init` resets every progress PV and publishes the version (:124-155).
#[epics_base_rs::epics_test]
async fn init_publishes_the_version_and_clears_the_progress() {
    let db = ioc().await;
    db.put_pv(&format!("{P}Ntotal"), EpicsValue::Long(5))
        .await
        .unwrap();
    db.put_pv(&format!("{P}hungTimeStr"), EpicsValue::String("x".into()))
        .await
        .unwrap();
    start(&db);
    assert!(eventually(5, || f(&db, "version") == 2.3).await);
    assert!(eventually(5, || f(&db, "Ntotal") == 0.0).await);
    assert_eq!(s(&db, "hungTimeStr"), "");
    assert_eq!(f(&db, "running"), 0.0);
}

/// A busy outer scan sets the dimension: Ntotal is the product of the
/// active scans' NPTS and nothing past them (:172-211); the periodic update
/// weights each outer point by the points inside it (:225-285).
#[epics_base_rs::epics_test]
async fn a_two_dimensional_scan_counts_points_across_both() {
    let db = ioc().await;
    for (n, npts) in [(1, 10.0), (2, 3.0), (3, 7.0)] {
        scan(&db, n, "NPTS", npts).await;
    }
    start(&db);
    assert!(eventually(5, || f(&db, "version") == 2.3).await);

    // Outer first, as an sscan sets BUSY before its triggers start the
    // inner one (sscanRecord.c:1143): `idle` fixes scanDim from the BUSYs
    // set when it wakes (scanProg.st:172-176).
    scan(&db, 2, "BUSY", 1.0).await;
    scan(&db, 1, "BUSY", 1.0).await;
    assert!(eventually(5, || f(&db, "running") == 1.0).await);
    assert!(eventually(5, || f(&db, "Ntotal") == 30.0).await);
    assert_eq!(s(&db, "pauseTimeStr"), "00:00:00");
    assert!(!s(&db, "startingTimeStr").is_empty());

    scan(&db, 1, "CPT", 5.0).await;
    scan(&db, 2, "CPT", 1.0).await;
    // The update runs UPDATE_DELAY after the state is entered, and puts
    // percentDone last of the three.
    assert!(eventually(5, || f(&db, "percentDone") == 50.0).await);
    assert_eq!(f(&db, "Nfinished"), 15.0);
    assert_eq!(f(&db, "fractionDone"), 0.5);
    assert!(!s(&db, "endingTimeStr").is_empty());

    scan(&db, 1, "CPT", 10.0).await;
    scan(&db, 2, "CPT", 3.0).await;
    scan(&db, 1, "BUSY", 0.0).await;
    scan(&db, 2, "BUSY", 0.0).await;
    // scanFinish takes the final CPT at once, not after a delay (:335-368).
    assert!(eventually(1, || f(&db, "running") == 0.0).await);
    assert!(eventually(1, || f(&db, "percentDone") == 100.0).await);
    assert_eq!(f(&db, "Nfinished"), 40.0);
    assert_eq!(f(&db, "fractionDone"), 1.0, "limited to 1");
    assert_eq!(s(&db, "remainingTimeStr"), "00:00:00");
}

/// A pause on a running scan raises `paused` and counts once; lifting it
/// clears `paused` and books the pause time (:220-223, :289-312, :417-435).
#[epics_base_rs::epics_test]
async fn a_pause_is_counted_once_and_released() {
    let db = ioc().await;
    scan(&db, 1, "NPTS", 10.0).await;
    start(&db);
    assert!(eventually(5, || f(&db, "version") == 2.3).await);
    // initPause sleeps a second before it watches the flags.
    sleep(Duration::from_millis(1200)).await;

    scan(&db, 1, "BUSY", 1.0).await;
    assert!(eventually(5, || f(&db, "running") == 1.0).await);
    scan(&db, 1, "PAUS", 1.0).await;
    assert!(eventually(5, || f(&db, "paused") == 1.0).await);
    // scanProgress sees `paused` at its next wake: the UPDATE_DELAY tick.
    assert!(eventually(5, || f(&db, "Npauses") == 1.0).await);

    scan(&db, 1, "PAUS", 0.0).await;
    assert!(eventually(5, || f(&db, "paused") == 0.0).await);
    assert!(eventually(5, || s(&db, "pauseTimeStr").starts_with("00:00:")).await);
    assert_eq!(f(&db, "Npauses"), 1.0);
}

/// A pause on the outer scan while the inner one still runs is not yet a
/// pause: `paused` waits for the inner BUSY to drop (:401-409, :538-547).
#[epics_base_rs::epics_test]
async fn an_outer_pause_waits_for_the_inner_scan() {
    let db = ioc().await;
    scan(&db, 1, "NPTS", 10.0).await;
    scan(&db, 2, "NPTS", 3.0).await;
    start(&db);
    assert!(eventually(5, || f(&db, "version") == 2.3).await);
    sleep(Duration::from_millis(1200)).await;

    // Outer first, as an sscan sets BUSY before its triggers start the
    // inner one (sscanRecord.c:1143): `idle` fixes scanDim from the BUSYs
    // set when it wakes (scanProg.st:172-176).
    scan(&db, 2, "BUSY", 1.0).await;
    scan(&db, 1, "BUSY", 1.0).await;
    assert!(eventually(5, || f(&db, "Ntotal") == 30.0).await);

    scan(&db, 2, "PAUS", 1.0).await;
    sleep(Duration::from_millis(300)).await;
    assert_eq!(f(&db, "paused"), 0.0, "scan1 is still busy");

    scan(&db, 1, "BUSY", 0.0).await;
    assert!(eventually(2, || f(&db, "paused") == 1.0).await);
}
