//! C `scanAdd` calls `get_ioint_info(0)` with the record's lock held, and
//! StreamDevice starts its I/O Intr protocol there, so a reply that arrives at
//! once redirects through `dbPut`/`dbPutField` from inside the call
//! (StreamEpics.cc streamGetIointInfo, matchValue). A dset that
//! [`DeviceSupport::lends_held`] is lent its record for that call on every
//! path that adds a record to its list: `scanInit`, a SCAN put, the SIMM swap
//! and a device-link put's `restoreScan`. SCAN is posted as C posts it,
//! after the call: once by a SCAN put (dbAccess.c:1416-1418), never by the
//! SIMM swap (recGbl.c:427-437), and by a device-link put only when the
//! record ends off I/O Intr (dbAccess.c:1231-1232).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::sync::mpsc;
use epics_base_rs::server::database::{AsyncDbHandle, FieldPut, HeldInstance, PvDatabase};
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport, DeviceUdf};
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record, ScanType};
use epics_base_rs::types::{DbFieldType, EpicsValue};

#[derive(Default)]
struct Log {
    db: OnceLock<AsyncDbHandle>,
    /// Each lent Add: the record, and whether both of its puts answered Ok.
    adds: Mutex<Vec<(String, bool)>>,
}

/// Refuses the Add when its link is `@no`.
struct Intr {
    name: String,
    inp: String,
    log: Arc<Log>,
    _pulses: Option<mpsc::Sender<()>>,
}

impl DeviceSupport for Intr {
    fn bind_database(&mut self, db: AsyncDbHandle) {
        let _ = self.log.db.set(db);
    }
    fn set_record_info(&mut self, name: &str, _: ScanType) {
        self.name = name.to_owned();
    }
    fn lends_held(&self) -> bool {
        true
    }
    /// The own DESC by dbPut, and `T.VAL` by dbPutField, whose FLNK runs.
    fn get_ioint_info_add_held(&mut self, held: &HeldInstance<'_>) -> bool {
        let db = self.log.db.get().unwrap();
        let addr = |pv: &str| db.name_to_dbaddr(pv, None).unwrap();
        let own = held.db_put(
            &addr(&format!("{}.DESC", self.name)),
            EpicsValue::String("add".into()),
        );
        let other = held.db_put_field(&addr("T.VAL"), EpicsValue::Double(1.0));
        self.log.adds.lock().unwrap().push((
            self.name.clone(),
            own.is_ok() && matches!(other, FieldPut::Done(Ok(()))),
        ));
        self.inp != "@no"
    }
    fn io_intr_receiver(&mut self) -> Option<mpsc::Receiver<()>> {
        let (tx, rx) = mpsc::channel(1);
        self._pulses = Some(tx);
        Some(rx)
    }
    fn read(&mut self, _: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::computed(DeviceUdf::Untouched))
    }
    fn write(&mut self, _: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
    fn dtyp(&self) -> &str {
        "intr"
    }
}

static ROWS: [DevSup; 1] = [DevSup {
    name: "devIntr",
    choice: "intr",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(6, &[2, 3, 4], true),
}];

const DB: &str = r#"
record(ao, "T") { field(FLNK, "N") }
record(calc, "N") { field(CALC, "VAL+1") }
record(ai, "P") { field(DTYP, "intr") field(INP, "@a") }
record(ai, "PR") { field(DTYP, "intr") field(INP, "@no") }
record(ai, "S") { field(DTYP, "intr") field(INP, "@a") field(SSCN, "I/O Intr") }
record(ai, "SR") { field(DTYP, "intr") field(INP, "@no") field(SSCN, "I/O Intr") }
record(ai, "L") { field(DTYP, "intr") field(INP, "@a") field(SCAN, "I/O Intr") }
"#;

async fn ioc() -> (Arc<PvDatabase>, Arc<Log>) {
    let log = Arc::new(Log::default());
    let made = log.clone();
    let devices: Vec<(&str, &[DevSup])> = vec![("ai", &ROWS[..])];
    let ioc = IocBuilder::new()
        .register_dynamic_device_support(&devices, move |ctx: &DeviceSupportContext| {
            (ctx.dtyp == "intr").then(|| {
                Box::new(Intr {
                    name: String::new(),
                    inp: ctx.inp.to_owned(),
                    log: made.clone(),
                    _pulses: None,
                }) as Box<dyn DeviceSupport>
            })
        })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    (db, log)
}

fn scan_posts(db: &PvDatabase, rec: &str) -> EventReader {
    let mut rx = db
        .get_record(rec)
        .unwrap()
        .write()
        .add_subscriber("SCAN", 1, DbFieldType::Enum, EventMask::VALUE.bits())
        .unwrap();
    while rx.try_recv().is_ok() {}
    rx
}

fn count(rx: &mut EventReader) -> usize {
    std::iter::from_fn(|| rx.try_recv().ok()).count()
}

fn get(db: &PvDatabase, pv: &str) -> String {
    match db.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.to_string(),
        v => v.to_f64().unwrap().to_string(),
    }
}

fn scan(db: &PvDatabase, rec: &str) -> ScanType {
    db.get_record(rec).unwrap().read().common.scan
}

async fn listed(db: &PvDatabase, rec: &str) -> bool {
    db.records_for_scan(ScanType::IoIntr)
        .await
        .iter()
        .any(|n| n == rec)
}

fn adds(log: &Log) -> Vec<(String, bool)> {
    std::mem::take(&mut *log.adds.lock().unwrap())
}

#[epics_macros_rs::epics_test]
async fn scan_init_lends_the_record() {
    let (db, log) = ioc().await;
    assert_eq!(adds(&log), [("L".to_string(), true)]);
    assert_eq!(get(&db, "L.DESC"), "add");
    assert!(listed(&db, "L").await);
}

#[epics_macros_rs::epics_test]
async fn a_scan_put_lends_the_record_and_posts_scan_once() {
    let (db, log) = ioc().await;
    adds(&log);
    for (rec, joined) in [("P", true), ("PR", false)] {
        let mut posts = scan_posts(&db, rec);
        let n = get(&db, "N");
        db.put_record_field_from_ca_no_notify(rec, "SCAN", EpicsValue::String("I/O Intr".into()))
            .await
            .unwrap();
        assert_eq!(adds(&log), [(rec.to_string(), true)]);
        assert_eq!(get(&db, &format!("{rec}.DESC")), "add");
        assert_ne!(get(&db, "N"), n, "T's FLNK ran inside {rec}'s Add");
        let want = if joined {
            ScanType::IoIntr
        } else {
            ScanType::Passive
        };
        assert_eq!(scan(&db, rec), want);
        assert_eq!(listed(&db, rec).await, joined);
        assert_eq!(count(&mut posts), 1, "{rec}: one SCAN post, after the Add");
    }
}

#[epics_macros_rs::epics_test]
async fn the_simm_swap_lends_the_record_and_posts_no_scan() {
    let (db, log) = ioc().await;
    adds(&log);
    for (rec, joined) in [("S", true), ("SR", false)] {
        let mut posts = scan_posts(&db, rec);
        db.put_record_field_from_ca_no_notify(rec, "SIMM", EpicsValue::String("YES".into()))
            .await
            .unwrap();
        assert_eq!(adds(&log), [(rec.to_string(), true)]);
        assert_eq!(get(&db, &format!("{rec}.DESC")), "add");
        let want = if joined {
            ScanType::IoIntr
        } else {
            ScanType::Passive
        };
        assert_eq!(scan(&db, rec), want);
        assert_eq!(listed(&db, rec).await, joined);
        assert_eq!(count(&mut posts), 0, "{rec}: recGblCheckSimm posts no SCAN");
    }
}

#[epics_macros_rs::epics_test]
async fn a_device_link_put_lends_the_restored_add() {
    let (db, log) = ioc().await;
    adds(&log);
    let mut posts = scan_posts(&db, "L");
    db.put_record_field_from_ca_no_notify("L", "INP", EpicsValue::String("@b".into()))
        .await
        .unwrap();
    assert_eq!(adds(&log), [("L".to_string(), true)]);
    assert_eq!(scan(&db, "L"), ScanType::IoIntr);
    assert!(listed(&db, "L").await);
    assert_eq!(count(&mut posts), 0, "back on I/O Intr: no SCAN post");

    db.put_record_field_from_ca_no_notify("L", "INP", EpicsValue::String("@no".into()))
        .await
        .unwrap();
    assert_eq!(adds(&log), [("L".to_string(), true)]);
    assert_eq!(scan(&db, "L"), ScanType::Passive);
    assert!(!listed(&db, "L").await);
    assert_eq!(count(&mut posts), 1, "off I/O Intr: one SCAN post");
}
