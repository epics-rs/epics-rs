//! `AsyncDbHandle::name_to_addr` and `db_get` take no record's lock set
//! while the caller holds its own, as C's `dbNameToAddr` and `dbGet` take no
//! lock: sscan resolves and reads PVs from `special` and `process` under its
//! own record (`sscanRecord.c:2335`, `:2752`), so two scans naming or reading
//! each other at once, or a scan reading a detector whose output puts into
//! the scan, must not each wait on the other's set.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::{AsyncDbHandle, OwnRecord, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{FieldDesc, ProcessOutcome, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue, RecordName};

static FIELDS: &[FieldDesc] = &[
    FieldDesc::new("VAL", DbFieldType::Double, false),
    FieldDesc::new("PV", DbFieldType::String, false),
    FieldDesc::new("CNT", DbFieldType::Long, false),
    FieldDesc::new("NRD", DbFieldType::Long, false),
];

/// A scan in miniature: `special(PV)` resolves the name written to `PV`
/// (`CNT` = its element count), and `process` reads it (`NRD` counts the
/// reads answered), both under the record's own hold.
#[derive(Default)]
struct Scan {
    handle: Option<AsyncDbHandle>,
    name: String,
    val: f64,
    pv: String,
    cnt: i32,
    nrd: i32,
}

impl Record for Scan {
    fn record_type(&self) -> &'static str {
        "scan"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        let h = self.handle.clone().expect("async context set");
        let name = self.name.clone();
        if h.db_get(
            &self.pv,
            DbFieldType::Double,
            1,
            Some(OwnRecord {
                name: name.as_bytes(),
                record: self,
                common: None,
            }),
        )
        .is_some()
        {
            self.nrd += 1;
        }
        Ok(ProcessOutcome::complete())
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            "PV" => Some(EpicsValue::String(self.pv.clone().into())),
            "CNT" => Some(EpicsValue::Long(self.cnt)),
            "NRD" => Some(EpicsValue::Long(self.nrd)),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => self.val = value.to_f64().unwrap_or(0.0),
            "PV" => self.pv = value.text_g(),
            _ => return Err(CaError::FieldNotFound(name.into())),
        }
        Ok(())
    }
    fn special(
        &mut self,
        field: &str,
        after: bool,
        _common: &epics_base_rs::server::record::CommonFields,
    ) -> CaResult<()> {
        if after && field == "PV" {
            let h = self.handle.as_ref().expect("async context set");
            self.cnt = h.name_to_addr(&self.pv).map_or(-1, |n| n as i32);
        }
        Ok(())
    }
    fn set_async_context(&mut self, name: RecordName, db: AsyncDbHandle) {
        self.name = String::from_utf8_lossy(name.as_bytes()).into_owned();
        self.handle = Some(db);
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }
}

async fn ioc(db: &str) -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .register_record_type("scan", || Box::new(Scan::default()))
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db
}

fn long(db: &PvDatabase, name: &str) -> i32 {
    match db.get_pv(name).unwrap() {
        EpicsValue::Long(n) => n,
        other => panic!("{name} = {other:?}"),
    }
}

/// `n` puts on a thread of its own, the `i`th of `value(i)` to `name`.
fn puts(
    db: &Arc<PvDatabase>,
    name: &'static str,
    n: usize,
    value: impl Fn(usize) -> EpicsValue + Send + 'static,
) -> std::thread::JoinHandle<()> {
    let db = db.clone();
    std::thread::spawn(move || {
        epics_base_rs::runtime::task::test_block_on(async move {
            for i in 0..n {
                db.put_pv(name, value(i)).await.unwrap();
            }
        })
    })
}

/// `record` processed on a thread of its own, `n` times or until `stop`.
fn processes(
    db: &Arc<PvDatabase>,
    record: &'static str,
    n: usize,
    stop: Option<Arc<AtomicBool>>,
) -> std::thread::JoinHandle<()> {
    let db = db.clone();
    std::thread::spawn(move || {
        epics_base_rs::runtime::task::test_block_on(async move {
            for _ in 0..n {
                if stop.as_ref().is_some_and(|s| s.load(Ordering::Relaxed)) {
                    return;
                }
                db.process_record(record).await.unwrap();
            }
        })
    })
}

async fn name(db: &PvDatabase, field: &str, pv: &str) {
    db.put_pv(field, EpicsValue::String(pv.into()))
        .await
        .unwrap();
}

async fn join_within_a_minute(threads: Vec<std::thread::JoinHandle<()>>) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !threads.iter().all(|t| t.is_finished()) {
        assert!(Instant::now() < deadline, "the threads deadlocked");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
    for t in threads {
        t.join().unwrap();
    }
}

#[epics_macros_rs::epics_test]
async fn special_resolves_a_name_without_locking_its_record() {
    let db = ioc("record(scan, \"S1\") {}\nrecord(scan, \"S2\") {}\n").await;
    db.put_pv("S1.PV", EpicsValue::String("S2.VAL".into()))
        .await
        .unwrap();
    assert_eq!(long(&db, "S1.CNT"), 1);
    db.put_pv("S1.PV", EpicsValue::String("S2.NOSUCH".into()))
        .await
        .unwrap();
    assert_eq!(long(&db, "S1.CNT"), -1);
}

#[epics_macros_rs::epics_test]
async fn two_records_naming_each_other_at_once_do_not_deadlock() {
    let db = ioc("record(scan, \"S1\") {}\nrecord(scan, \"S2\") {}\n").await;
    let rename = |me: &'static str, other: &'static str| {
        let pv =
            move |i: usize| EpicsValue::String(if i.is_multiple_of(2) { other } else { "" }.into());
        puts(&db, me, 20000, pv)
    };
    join_within_a_minute(vec![rename("S1.PV", "S2.VAL"), rename("S2.PV", "S1.VAL")]).await;
}

#[epics_macros_rs::epics_test]
async fn two_records_reading_each_other_at_once_do_not_deadlock() {
    let db = ioc("record(scan, \"S1\") {}\nrecord(scan, \"S2\") {}\n").await;
    // Named at run time, as sscan's are: the resolve reads each target once.
    name(&db, "S1.PV", "S2.CNT").await;
    name(&db, "S2.PV", "S1.CNT").await;
    let process = |me: &'static str| processes(&db, me, 20000, None);
    join_within_a_minute(vec![process("S1"), process("S2")]).await;
    // Busy or not, each read was answered, fresh or by the last read.
    assert_eq!(long(&db, "S1.NRD"), 20000);
    assert_eq!(long(&db, "S2.NRD"), 20000);
}

#[epics_macros_rs::epics_test]
async fn a_detector_writing_into_the_scan_while_it_reads_does_not_deadlock() {
    let db = ioc("record(calcout, \"DET\") {\n\
           field(INPA, \"DET\")\n\
           field(CALC, \"A+1\")\n\
           field(OUT, \"S.VAL CA\")\n\
           field(OOPT, \"Every Time\")\n\
         }\n\
         record(scan, \"S\") {}\n")
    .await;
    name(&db, "S.PV", "DET").await;
    // DET processes under its own lock and its OUT puts into S, while S
    // holds itself and reads DET.
    let stop = Arc::new(AtomicBool::new(false));
    let hammer = processes(&db, "DET", usize::MAX, Some(stop.clone()));
    let scan = processes(&db, "S", 20000, None);
    join_within_a_minute(vec![scan]).await;
    stop.store(true, Ordering::Relaxed);
    join_within_a_minute(vec![hammer]).await;
    assert_eq!(long(&db, "S.NRD"), 20000);
    assert!(db.get_pv("DET").unwrap().to_f64().unwrap() > 0.0);
}
