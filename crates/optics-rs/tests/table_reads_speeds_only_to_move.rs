//! table reads its speed links only on a move that violates no limit, where
//! C's `process()` calls `SaveMotorSpeeds` (`tableRecord.c:536`), against
//! oracleTableIoc with `camonitor -m v T:N`, driven with `caput T.PROC 1; T.X
//! 1; T.PROC 1; T.Y 2; T.AX 0.5; T.ZERO 1; T.PROC 1; T.X 1; T.READ 1; T.SYNC
//! 1; T.INIT 1; T.PROC 1`. The stream is the oracle's with the timestamps and
//! the connect value cut.
//!
//! X's speed link reads `T:V` PP, whose FLNK counts each read in `T:N`: ZERO,
//! READ, SYNC, INIT and the two limit-violating cycles read no speed.

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — ran and passed on the exec
// backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread`); no CA server is built.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use optics_rs::records::table::TableRecord;

const DB: &str = r#"
record(ao, "T:RB") {
}
record(ao, "T:V") {
  field(VAL, "2")
  field(FLNK, "T:N")
}
record(calc, "T:N") {
  field(INPA, "T:N")
  field(CALC, "A+1")
}
record(table, "T") {
  field(LX, "100")
  field(LZ, "200")
  field(M0XL, "T:RB")
  field(R0XI, "T:RB")
  field(E0XI, "T:RB")
  field(V0XI, "T:V PP")
  field(V0XL, "T:V")
}
"#;

const STEPS: [(&str, &str); 12] = [
    ("PROC", "1"),
    ("X", "1"),
    ("PROC", "1"),
    ("Y", "2"),
    ("AX", "0.5"),
    ("ZERO", "1"),
    ("PROC", "1"),
    ("X", "1"),
    ("READ", "1"),
    ("SYNC", "1"),
    ("INIT", "1"),
    ("PROC", "1"),
];

const VALUE: &str = "\
--
T:N 1
--
T:N 2
--
T:N 3
--
T:N 4
--
T:N 5
--
--
--
--
--
--
--
T:N 6
";

/// Appends each event it passes, as `camonitor` prints it without the time.
struct Line {
    stream: Arc<Mutex<String>>,
}

impl SubscriptionFilter for Line {
    fn name(&self) -> &'static str {
        "line"
    }

    fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
        let value = event.event.value().as_value().and_then(EpicsValue::to_f64);
        let mut stream = self.stream.lock().unwrap();
        stream.push_str(&format!("T:N {}\n", value.unwrap()));
        Some(event)
    }
}

#[tokio::test]
async fn table_reads_speeds_only_to_move_as_the_oracle_does() {
    let (db, _) = IocBuilder::new()
        .register_record_type("table", || Box::new(TableRecord::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .register_record_type("calc", || Box::new(CalcRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let stream: Arc<Mutex<String>> = Arc::default();
    let _reader = {
        let cell = db.get_record("T:N").unwrap();
        let mut inst = cell.write();
        let reader = inst
            .add_subscriber("VAL", 0, DbFieldType::Double, EventMask::VALUE.bits())
            .unwrap();
        assert!(inst.attach_filter_to_last_subscriber(
            "VAL",
            Arc::new(Line {
                stream: stream.clone(),
            }),
        ));
        reader
    };
    for (field, value) in STEPS {
        stream.lock().unwrap().push_str("--\n");
        db.put_record_field_from_ca("T", field, EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    assert_eq!(*stream.lock().unwrap(), VALUE);
}
