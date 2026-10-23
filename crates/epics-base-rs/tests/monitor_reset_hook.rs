//! A [`CycleHooks::MONITOR_RESET`] type's `monitor()` runs
//! `recGblResetAlarms` only on the cycles it says (`scanparmRecord.c:280`).
//! A cycle without the reset leaves STAT/SEVR and posts no alarm; what it
//! raised waits in NSTA/NSEV for the next reset, as C's `nsev` does.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{CycleHooks, FieldDesc, ProcessOutcome, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue};

static FIELDS: &[FieldDesc] = &[
    FieldDesc::new("RST", DbFieldType::Short, false),
    FieldDesc::new("BAD", DbFieldType::Short, false),
];

/// RST says whether this cycle's `monitor()` resets; BAD leaves the value
/// undefined, so the cycle raises UDF/INVALID.
struct Resetting {
    hooks: CycleHooks,
    rst: i16,
    bad: i16,
}

impl Record for Resetting {
    fn record_type(&self) -> &'static str {
        "monreset_test"
    }
    fn cycle_hooks(&self) -> CycleHooks {
        self.hooks
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }
    fn monitor_resets_alarms(&mut self) -> bool {
        self.rst != 0
    }
    fn value_is_undefined(&self) -> bool {
        self.bad != 0
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "RST" => Some(EpicsValue::Short(self.rst)),
            "BAD" => Some(EpicsValue::Short(self.bad)),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "RST" => self.rst = value.to_f64().unwrap_or(0.0) as i16,
            "BAD" => self.bad = value.to_f64().unwrap_or(0.0) as i16,
            _ => return Err(CaError::FieldNotFound(name.into())),
        }
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }
}

async fn ioc(hooks: CycleHooks) -> (Arc<PvDatabase>, EventReader) {
    let (db, _) = IocBuilder::new()
        .register_record_type("monreset_test", move || {
            Box::new(Resetting {
                hooks,
                rst: 0,
                bad: 0,
            })
        })
        .db_string(r#"record(monreset_test, "R") {}"#, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let sevr = db
        .get_record("R")
        .unwrap()
        .write()
        .add_subscriber("SEVR", 1, DbFieldType::Short, EventMask::VALUE.bits())
        .unwrap();
    (db, sevr)
}

/// Process R once with BAD and RST as given; its SEVR after and the SEVR
/// posts the cycle made.
async fn cycle(db: &PvDatabase, sevr_rx: &mut EventReader, bad: i16, rst: i16) -> (u16, usize) {
    db.put_record_field_from_ca_no_notify("R", "BAD", EpicsValue::Short(bad))
        .await
        .unwrap();
    db.put_record_field_from_ca_no_notify("R", "RST", EpicsValue::Short(rst))
        .await
        .unwrap();
    while sevr_rx.try_recv().is_ok() {}
    db.process_record_with_links("R", &mut ProcStack::new())
        .await
        .unwrap();
    let mut posts = 0;
    while sevr_rx.try_recv().is_ok() {
        posts += 1;
    }
    let sevr = db.get_record("R").unwrap().read().common.sevr as u16;
    (sevr, posts)
}

#[epics_macros_rs::epics_test]
async fn a_cycle_without_the_reset_keeps_the_alarm_pending() {
    let (db, mut sevr) = ioc(CycleHooks::NONE.union(CycleHooks::MONITOR_RESET)).await;
    assert_eq!(
        cycle(&db, &mut sevr, 0, 1).await,
        (0, 1),
        "reset: UDF/INVALID cleared"
    );
    assert_eq!(
        cycle(&db, &mut sevr, 1, 0).await,
        (0, 0),
        "UDF raised, no reset"
    );
    assert_eq!(
        cycle(&db, &mut sevr, 0, 1).await,
        (3, 1),
        "the pending INVALID is what the next reset commits"
    );
    assert_eq!(cycle(&db, &mut sevr, 0, 1).await, (0, 1));
}

#[epics_macros_rs::epics_test]
async fn a_type_without_the_hook_resets_every_cycle() {
    let (db, mut sevr) = ioc(CycleHooks::NONE).await;
    assert_eq!(cycle(&db, &mut sevr, 0, 1).await, (0, 1));
    assert_eq!(
        cycle(&db, &mut sevr, 1, 0).await,
        (3, 1),
        "RST is not asked"
    );
    assert_eq!(cycle(&db, &mut sevr, 0, 0).await, (0, 1));
}
