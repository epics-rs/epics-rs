//! A record type's `init_record(1)` sees the links it declares in
//! `Record::init_link_reads` already read, as C's `dbGetLink` inside
//! `init_record` reads them (`tableRecord.c:386-401`): a local DB link
//! delivers its target's value, read without processing it; a link to a name
//! no record serves is a CA link that is not connected at init and a constant
//! link writes nothing, so both leave the field as loaded.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{FieldDesc, ProcessOutcome, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// What each record's `init_record(1)` saw in `RB`, by its `INP` text.
type Seen = Arc<Mutex<Vec<(String, f64)>>>;

struct InitReader {
    inp: String,
    rb: f64,
    seen: Seen,
}

static FIELDS: &[FieldDesc] = &[
    FieldDesc::new("INP", DbFieldType::String, false),
    FieldDesc::new("RB", DbFieldType::Double, false),
];

impl Record for InitReader {
    fn record_type(&self) -> &'static str {
        "initReader"
    }

    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "INP" => Some(EpicsValue::String(self.inp.clone().into())),
            "RB" => Some(EpicsValue::Double(self.rb)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "INP" => self.inp = value.to_string(),
            "RB" => self.rb = value.to_f64().unwrap_or(f64::NAN),
            _ => {}
        }
        Ok(())
    }

    fn init_link_reads(&self) -> &'static [(&'static str, &'static str)] {
        &[("INP", "RB")]
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 1 {
            self.seen.lock().unwrap().push((self.inp.clone(), self.rb));
        }
        Ok(())
    }
}

#[epics_macros_rs::epics_test]
async fn init_record_pass_one_sees_its_declared_db_links_read() {
    let seen: Seen = Arc::default();
    let factory = seen.clone();
    // The readers load before their targets: C reads at pass 1, after every
    // record's pass 0, so load order does not matter.
    let db = IocBuilder::new()
        .register_record_type("initReader", move || {
            Box::new(InitReader {
                inp: String::new(),
                rb: -1.0,
                seen: factory.clone(),
            })
        })
        .db_string(
            r#"
record(initReader, "DB") { field(INP, "SRC") }
record(initReader, "FLD") { field(INP, "SRC.HOPR") }
record(initReader, "CA") { field(INP, "NOSUCH") }
record(initReader, "CONST") { field(INP, "3") }
record(calc, "SRC") { field(CALC, "VAL+1") field(VAL, "7.5") field(HOPR, "9") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    let mut seen = seen.lock().unwrap().clone();
    seen.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        seen,
        [
            ("3".to_string(), -1.0),
            ("NOSUCH".to_string(), -1.0),
            ("SRC".to_string(), 7.5),
            ("SRC.HOPR".to_string(), 9.0),
        ]
    );
    // Read, not processed: processing would have made it 8.5.
    assert_eq!(db.get_pv("SRC").unwrap().to_f64(), Some(7.5));
}
