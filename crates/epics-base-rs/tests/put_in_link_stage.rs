//! [`ProcessAction::PutInLinkStage`] stores and posts its field where it sits
//! among the cycle's link writes: scanparm puts OGO, clears and posts GO,
//! then puts OLOAD (`scanparmRecord.c:248-262`), so a target of the later
//! put reads GO already 0.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{CycleValue, FieldDesc, ProcessAction, ProcessOutcome, Record};
use epics_base_rs::types::{DbFieldType, DbfCode, EpicsValue};

static FIELDS: &[FieldDesc] = &[
    FieldDesc {
        pp: true,
        ..FieldDesc::new("GO", DbFieldType::Short, false)
    },
    FieldDesc {
        declared_dbf: DbfCode::Outlink,
        ..FieldDesc::new("OGO", DbFieldType::String, false)
    },
    FieldDesc {
        declared_dbf: DbfCode::Outlink,
        ..FieldDesc::new("OLD", DbFieldType::String, false)
    },
];

#[derive(Default)]
struct Clearing {
    go: i16,
    ogo: String,
    old: String,
}

impl Record for Clearing {
    fn record_type(&self) -> &'static str {
        "put_in_link_stage_test"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        if self.go == 0 {
            return Ok(ProcessOutcome::complete());
        }
        Ok(ProcessOutcome::complete_with(vec![
            ProcessAction::WriteDbLink {
                link_field: "OGO",
                value: EpicsValue::Short(self.go),
            },
            ProcessAction::PutInLinkStage {
                field: "GO",
                value: CycleValue::Short(0),
            },
            ProcessAction::WriteDbLink {
                link_field: "OLD",
                value: EpicsValue::Short(1),
            },
        ]))
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "GO" => Some(EpicsValue::Short(self.go)),
            "OGO" => Some(EpicsValue::String(self.ogo.clone().into())),
            "OLD" => Some(EpicsValue::String(self.old.clone().into())),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "GO" => self.go = value.to_f64().unwrap_or(0.0) as i16,
            "OGO" => self.ogo = value.to_string(),
            "OLD" => self.old = value.to_string(),
            _ => return Err(CaError::FieldNotFound(name.into())),
        }
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }
}

/// `R:GO` takes OGO's put; `R:SEEN` is processed by OLD's and reads R.GO.
const DB: &str = r#"
record(ao, "R:GO") {}
record(calc, "R:SEEN") { field(INPA, "R.GO NPP") field(CALC, "A") field(VAL, "-1") }
record(put_in_link_stage_test, "R") { field(OGO, "R:GO") field(OLD, "R:SEEN.PROC") }
"#;

async fn ioc() -> (Arc<PvDatabase>, EventReader) {
    let (db, _) = IocBuilder::new()
        .register_record_type("put_in_link_stage_test", || Box::new(Clearing::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let go = db
        .get_record("R")
        .unwrap()
        .write()
        .add_subscriber("GO", 1, DbFieldType::Short, EventMask::VALUE.bits())
        .unwrap();
    (db, go)
}

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

#[epics_macros_rs::epics_test]
async fn the_put_after_the_clear_sees_it() {
    let (db, mut go) = ioc().await;
    db.put_record_field_from_ca_no_notify("R", "GO", EpicsValue::Short(1))
        .await
        .unwrap();
    while go.try_recv().is_ok() {}
    // Process again with GO set, outside the put's own GO post.
    db.put_record_field_from_ca_no_notify("R", "GO", EpicsValue::Short(1))
        .await
        .unwrap();
    assert_eq!(num(&db, "R:GO"), 1.0, "OGO carried GO before the clear");
    assert_eq!(num(&db, "R:SEEN"), 0.0, "OLD's target read GO cleared");
    assert_eq!(num(&db, "R.GO"), 0.0);
    let mut values = Vec::new();
    while let Ok(ev) = go.try_recv() {
        values.push(ev.value().as_value().and_then(EpicsValue::to_f64).unwrap());
    }
    assert_eq!(
        values.last(),
        Some(&0.0),
        "the clear posted, got {values:?}"
    );
}

/// The foreign-call path runs no link writes, but still owes the store.
#[epics_macros_rs::epics_test]
async fn process_local_still_clears() {
    let (db, mut go) = ioc().await;
    let rec = db.get_record("R").unwrap();
    {
        let mut inst = rec.write();
        inst.record.put_field("GO", EpicsValue::Short(1)).unwrap();
        inst.process_local().unwrap();
    }
    assert_eq!(num(&db, "R.GO"), 0.0);
    assert!(go.try_recv().is_ok(), "the clear posted");
}
