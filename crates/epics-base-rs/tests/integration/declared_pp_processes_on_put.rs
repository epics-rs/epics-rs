//! A record type no base `.dbd` declares takes its `pp(TRUE)` set from the
//! declaration it hands back, as C reads `dbFldDes::process_passive`: with no
//! `process_passive_fields` override, a CA put to a `pp` field processes the
//! Passive record and a put to any other field does not.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::*;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// What a downstream crate's generated table says: GO is `pp(TRUE)`, X is not.
static FIELDS: &[FieldDesc] = &[
    FieldDesc {
        pp: true,
        ..FieldDesc::new("GO", DbFieldType::Short, false)
    },
    FieldDesc::new("X", DbFieldType::Short, false),
];

struct Contributed {
    go: i16,
    x: i16,
    processed: Arc<AtomicU32>,
}

impl Record for Contributed {
    fn record_type(&self) -> &'static str {
        "contributed_pp"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.processed.fetch_add(1, Ordering::Relaxed);
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "GO" => Some(EpicsValue::Short(self.go)),
            "X" => Some(EpicsValue::Short(self.x)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        let v = value.to_f64().unwrap_or(0.0) as i16;
        match name {
            "GO" => self.go = v,
            "X" => self.x = v,
            _ => return Err(CaError::FieldNotFound(name.to_string())),
        }
        Ok(())
    }

    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }
}

async fn record(processed: Arc<AtomicU32>) -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    db.add_record(
        "REC",
        Box::new(Contributed {
            go: 0,
            x: 0,
            processed,
        }),
    )
    .await
    .unwrap();
    db
}

#[epics_macros_rs::epics_test]
async fn a_put_to_a_declared_pp_field_processes() {
    let processed = Arc::new(AtomicU32::new(0));
    let db = record(processed.clone()).await;
    db.put_record_field_from_ca_no_notify("REC", "GO", EpicsValue::Short(1))
        .await
        .unwrap();
    assert_eq!(processed.load(Ordering::Relaxed), 1);
}

#[epics_macros_rs::epics_test]
async fn a_put_to_a_field_without_pp_does_not_process() {
    let processed = Arc::new(AtomicU32::new(0));
    let db = record(processed.clone()).await;
    db.put_record_field_from_ca_no_notify("REC", "X", EpicsValue::Short(1))
        .await
        .unwrap();
    assert_eq!(processed.load(Ordering::Relaxed), 0);
}
