//! A failed [`ProcessAction::WriteDbLinkOrReturn`] is `process()` returning
//! the put's status with no monitor or forward link. Where the C `return`
//! sits decides PACT: `scanparmRecord.c:206`/`:219` return ahead of
//! `pact = TRUE`, so the next put processes again (`ReturnPact::Clear`);
//! `tableRecord.c:574-576` returns after it, so the record stays active
//! (`ReturnPact::Held`).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{FieldDesc, ProcessAction, ProcessOutcome, Record, ReturnPact};
use epics_base_rs::types::{DbFieldType, DbfCode, EpicsValue};

static FIELDS: &[FieldDesc] = &[
    FieldDesc {
        pp: true,
        ..FieldDesc::new("GO", DbFieldType::Short, false)
    },
    FieldDesc::new("VAL", DbFieldType::Double, false),
    FieldDesc {
        declared_dbf: DbfCode::Outlink,
        ..FieldDesc::new("OUT", DbFieldType::String, false)
    },
];

/// Each `process()` bumps VAL and puts it through OUT, returning on failure.
struct Returning {
    pact: ReturnPact,
    go: i16,
    val: f64,
    out: String,
    processed: Arc<AtomicU32>,
}

impl Record for Returning {
    fn record_type(&self) -> &'static str {
        "or_return_test"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.processed.fetch_add(1, Ordering::Relaxed);
        self.val += 1.0;
        Ok(ProcessOutcome::complete_with(vec![
            ProcessAction::WriteDbLinkOrReturn {
                link_field: "OUT",
                value: EpicsValue::Double(self.val),
                pact: self.pact,
            },
        ]))
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "GO" => Some(EpicsValue::Short(self.go)),
            "VAL" => Some(EpicsValue::Double(self.val)),
            "OUT" => Some(EpicsValue::String(self.out.clone().into())),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "GO" => self.go = value.to_f64().unwrap_or(0.0) as i16,
            "VAL" => self.val = value.to_f64().unwrap_or(0.0),
            "OUT" => self.out = value.to_string(),
            _ => return Err(CaError::FieldNotFound(name.into())),
        }
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }
}

/// `R:FAIL` returns -1 from every process, which `dbDbPutValue` hands back
/// as the put's status (`dbDbLink.c:387-392`).
const DB: &str = r#"
record(sub, "R:FAIL") { field(SNAM, "minusOne") }
record(calc, "R:F") { field(INPA, "R:F") field(CALC, "A+1") }
record(or_return_test, "R") { field(OUT, "R:FAIL PP") field(FLNK, "R:F") }
"#;

async fn ioc(pact: ReturnPact) -> (Arc<PvDatabase>, Arc<AtomicU32>, EventReader) {
    let processed = Arc::new(AtomicU32::new(0));
    let p = processed.clone();
    let (db, _) = IocBuilder::new()
        .register_record_type("or_return_test", move || {
            Box::new(Returning {
                pact,
                go: 0,
                val: 0.0,
                out: String::new(),
                processed: p.clone(),
            })
        })
        .register_subroutine("minusOne", |_| Ok(-1))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let val = db
        .get_record("R")
        .unwrap()
        .write()
        .add_subscriber("VAL", 1, DbFieldType::Double, EventMask::VALUE.bits())
        .unwrap();
    (db, processed, val)
}

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

fn posts(rx: &mut EventReader) -> usize {
    let mut n = 0;
    while rx.try_recv().is_ok() {
        n += 1;
    }
    n
}

async fn go(db: &PvDatabase) -> Result<(), CaError> {
    db.put_record_field_from_ca_no_notify("R", "GO", EpicsValue::Short(1))
        .await
}

#[epics_macros_rs::epics_test]
async fn a_clear_return_leaves_the_record_processable() {
    let (db, processed, mut val) = ioc(ReturnPact::Clear).await;
    posts(&mut val);
    let put = go(&db).await;
    assert!(
        matches!(put, Err(CaError::ProcessStatus(s)) if s.get() == -1),
        "the put's status, got {put:?}"
    );
    assert!(
        !db.get_record("R").unwrap().read().is_processing(),
        "PACT 0"
    );
    assert_eq!(num(&db, "R:F"), 0.0, "no forward link");
    assert_eq!(posts(&mut val), 0, "no monitor");

    let _ = go(&db).await;
    assert_eq!(processed.load(Ordering::Relaxed), 2, "processed again");
    assert_eq!(num(&db, "R:FAIL"), 2.0, "and put again");
    assert_eq!(num(&db, "R:F"), 0.0);
}

#[epics_macros_rs::epics_test]
async fn a_held_return_leaves_the_record_active() {
    let (db, processed, mut val) = ioc(ReturnPact::Held).await;
    posts(&mut val);
    let put = go(&db).await;
    assert!(matches!(put, Err(CaError::ProcessStatus(s)) if s.get() == -1));
    assert!(db.get_record("R").unwrap().read().is_processing(), "PACT 1");
    assert_eq!(num(&db, "R:F"), 0.0);
    assert_eq!(posts(&mut val), 0);

    let _ = go(&db).await;
    assert_eq!(processed.load(Ordering::Relaxed), 1, "refused: active");
}
