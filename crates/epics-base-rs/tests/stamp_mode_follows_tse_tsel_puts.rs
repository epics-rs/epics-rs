//! A TSE or TSEL put made while a record waits in PACT decides how the
//! completion stamps, because C's `asyncFinish` calls `recGblGetTimeStamp`,
//! which reads `tse` and `tsel` as they are then (`recGbl.c:310-342`).

use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::record::{FieldDesc, ProcessOutcome, Record, StampMode};
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::types::{EpicsValue, WallTime};

/// Goes async on every process, so each cycle ends in `complete_async_record`.
struct Pending;

impl Record for Pending {
    fn record_type(&self) -> &'static str {
        "pending"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::async_pending())
    }
    fn get_field(&self, _: &str) -> Option<EpicsValue> {
        None
    }
    fn put_field(&mut self, name: &str, _: EpicsValue) -> CaResult<()> {
        Err(epics_base_rs::error::CaError::FieldNotFound(name.into()))
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        &[]
    }
}

const STALE: Duration = Duration::from_secs(1_000);

async fn db() -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("R", Box::new(Pending)).await.unwrap();
    for (name, calc) in [("ZERO", "0"), ("FIVE", "5")] {
        let mut src = CalcRecord::new(calc);
        src.init_record(0).unwrap();
        db.add_record(name, Box::new(src)).await.unwrap();
        db.process_record_with_links(name, &mut ProcStack::new())
            .await
            .unwrap();
    }
    db
}

async fn put(db: &PvDatabase, field: &str, value: EpicsValue) {
    db.put_pv_no_process(format!("R.{field}"), value)
        .await
        .unwrap();
}

/// Enter PACT, make `puts` inside the window, complete, and answer whether
/// the completion replaced a stale stamp, with the mode it left.
async fn cycle(db: &PvDatabase, puts: &[(&str, EpicsValue)]) -> (bool, StampMode, i16) {
    let stale = WallTime::UNIX_EPOCH + STALE;
    db.get_record("R").unwrap().write().common.time = stale;
    db.process_record_with_links("R", &mut ProcStack::new())
        .await
        .unwrap();
    assert!(
        db.get_record("R").unwrap().read().is_processing(),
        "in PACT"
    );
    for (field, value) in puts {
        put(db, field, value.clone()).await;
    }
    db.complete_async_record("R").await.unwrap();
    let rec = db.get_record("R").unwrap();
    let inst = rec.read();
    (
        inst.common.time != stale,
        inst.common.stamp_mode(),
        inst.common.tse(),
    )
}

/// TSE 5 names an event no provider answers, so its stamp keeps the stale
/// time (`epicsTimeGetEvent` fails, `recGbl.c:325-327`); TSE 0 reads the clock;
/// TSE -2 keeps whatever device support left.
#[epics_macros_rs::epics_test]
async fn a_tse_put_during_pact_moves_the_completion_stamp() {
    let db = db().await;
    let tse = |v| [("TSE", EpicsValue::Short(v))];
    // Plain -> Event.
    assert_eq!(cycle(&db, &tse(5)).await, (false, StampMode::Event, 5));
    // Event -> Plain.
    assert_eq!(cycle(&db, &tse(0)).await, (true, StampMode::Plain, 0));
    // Plain -> DeviceTime.
    assert_eq!(
        cycle(&db, &tse(-2)).await,
        (false, StampMode::DeviceTime, -2)
    );
    // DeviceTime -> Event (BestTime).
    assert_eq!(cycle(&db, &tse(-1)).await, (true, StampMode::Event, -1));
    // Event -> DeviceTime.
    assert_eq!(
        cycle(&db, &tse(-2)).await,
        (false, StampMode::DeviceTime, -2)
    );
    // DeviceTime -> Plain.
    assert_eq!(cycle(&db, &tse(0)).await, (true, StampMode::Plain, 0));
}

/// A TSEL link put during PACT is read by the completion and loads TSE
/// (`recGbl.c:322`); the mode stays `FromTsel` whatever it loads. Back to a
/// constant TSEL, the loaded TSE decides.
#[epics_macros_rs::epics_test]
async fn a_tsel_put_during_pact_moves_the_completion_stamp() {
    let db = db().await;
    let s = |v: &str| EpicsValue::String(v.into());
    put(&db, "TSE", EpicsValue::Short(5)).await;
    // Constant -> link: ZERO loads TSE 0, so the clock is read.
    assert_eq!(
        cycle(&db, &[("TSEL", s("ZERO"))]).await,
        (true, StampMode::FromTsel, 0)
    );
    // Link -> link: FIVE loads TSE 5, an unanswered event.
    assert_eq!(
        cycle(&db, &[("TSEL", s("FIVE"))]).await,
        (false, StampMode::FromTsel, 5)
    );
    // Link -> constant: TSE keeps the 5 the link loaded.
    assert_eq!(
        cycle(&db, &[("TSEL", s(""))]).await,
        (false, StampMode::Event, 5)
    );
    // Constant -> link with a constant TSE put in the same window.
    assert_eq!(
        cycle(&db, &[("TSE", EpicsValue::Short(-2)), ("TSEL", s("ZERO"))]).await,
        (true, StampMode::FromTsel, 0)
    );
}
