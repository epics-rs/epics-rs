//! [`StampPoint::AfterLinkWrites`] is C's `recGblGetTimeStamp` after the
//! last `dbPutLink` of `process()` (`tableRecord.c:594`,
//! `scanparmRecord.c:263`): TSEL sees what the puts did, and a cycle that
//! returns a failed put's status leaves before the stamp.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{
    FieldDesc, ProcessAction, ProcessOutcome, Record, ReturnPact, StampPoint,
};
use epics_base_rs::types::{DbFieldType, DbfCode, EpicsValue, WallTime};

static FIELDS: &[FieldDesc] = &[
    FieldDesc::new("VAL", DbFieldType::Double, false),
    FieldDesc {
        declared_dbf: DbfCode::Outlink,
        ..FieldDesc::new("OUT", DbFieldType::String, false)
    },
];

/// Each `process()` bumps VAL and puts it through OUT, returning on failure.
struct Stamping {
    point: StampPoint,
    val: f64,
    out: String,
}

impl Record for Stamping {
    fn record_type(&self) -> &'static str {
        "stamp_point_test"
    }
    fn stamp_point(&self) -> StampPoint {
        self.point
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.val += 1.0;
        Ok(ProcessOutcome::complete_with(vec![
            ProcessAction::WriteDbLinkOrReturn {
                link_field: "OUT",
                value: EpicsValue::Double(self.val),
                pact: ReturnPact::Clear,
            },
        ]))
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            "OUT" => Some(EpicsValue::String(self.out.clone().into())),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => self.val = value.to_f64().unwrap_or(0.0),
            "OUT" => self.out = value.text_g(),
            _ => return Err(CaError::FieldNotFound(name.into())),
        }
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }
}

/// `R` writes `T`, an ao that stamps when processed, and takes its own
/// stamp from `T.TIME`; `F` writes `R:FAIL`, whose process returns -1.
const DB: &str = r#"
record(ao, "T") {}
record(sub, "R:FAIL") { field(SNAM, "minusOne") }
record(stamp_point_test, "R") { field(OUT, "T PP") field(TSEL, "T.TIME") }
record(stamp_point_test, "F") { field(OUT, "R:FAIL PP") }
"#;

async fn ioc(point: StampPoint) -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .register_record_type("stamp_point_test", move || {
            Box::new(Stamping {
                point,
                val: 0.0,
                out: String::new(),
            })
        })
        .register_subroutine("minusOne", |_| Ok(-1))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db
}

fn time(db: &PvDatabase, rec: &str) -> WallTime {
    db.get_record(rec).unwrap().read().common.time
}

async fn process(db: &PvDatabase, rec: &str) -> Result<(), CaError> {
    // Let the clock move between stamps.
    epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    db.put_record_field_from_ca_no_notify(rec, "PROC", EpicsValue::Short(1))
        .await
}

#[epics_macros_rs::epics_test]
async fn the_stamp_sees_the_cycles_puts() {
    let db = ioc(StampPoint::AfterLinkWrites).await;
    process(&db, "R").await.unwrap();
    assert_eq!(time(&db, "R"), time(&db, "T"), "T.TIME after this put");
}

/// The point the other types stamp at reads TSEL ahead of the put.
#[epics_macros_rs::epics_test]
async fn a_stamp_before_the_puts_sees_the_previous_time() {
    let db = ioc(StampPoint::BeforeAlarms).await;
    let before = time(&db, "T");
    process(&db, "R").await.unwrap();
    assert_ne!(time(&db, "T"), before, "T processed");
    assert_eq!(time(&db, "R"), before);
}

#[epics_macros_rs::epics_test]
async fn a_returned_cycle_leaves_time() {
    let db = ioc(StampPoint::AfterLinkWrites).await;
    let before = time(&db, "F");
    assert!(process(&db, "F").await.is_err());
    assert_eq!(time(&db, "F"), before);
}
