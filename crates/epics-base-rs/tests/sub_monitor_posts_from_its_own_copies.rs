//! sub's `monitor()` (`subRecord.c:383-411`) against oracleStdIoc, whose
//! `subOracle` sets VAL = A + 1 and returns 0; three `camonitor -m v|a|l`
//! clients on `SB SB.A SB.LA SB.B SB.LB SB.MLST`, driven with `caput SB.PROC 1;
//! SB.A 1; SB.PROC 1; SB.A 2; SB.PROC 1; SB.PROC 1; SB.B 3; SB.PROC 1; SB.A 30;
//! SB.PROC 1; SB.HIGH 60; SB.PROC 1`. The streams below are the oracle's with
//! the timestamps and the connect values cut.
//!
//! The boundaries: MDEL and ADEL gate VAL; each input put posts from the put
//! and again from the next cycle, which compares it against its own LA..LU
//! copy; an unmoved input posts nothing; an alarm-only cycle posts VAL with
//! `DBE_ALARM`. LA..LU and MLST, which the cycle writes, never post; the
//! oracle reads LA 30, LB 3, LC 0 after the steps.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::{MENU_ALARM_SEVR, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(sub, "SB") {
  field(SNAM, "subOracle")
  field(MDEL, "2")
  field(ADEL, "5")
  field(HIGH, "20")
  field(HSV, "MINOR")
}
"#;

const STEPS: [(&str, &str); 12] = [
    ("PROC", "1"),
    ("A", "1"),
    ("PROC", "1"),
    ("A", "2"),
    ("PROC", "1"),
    ("PROC", "1"),
    ("B", "3"),
    ("PROC", "1"),
    ("A", "30"),
    ("PROC", "1"),
    ("HIGH", "60"),
    ("PROC", "1"),
];

const VALUE: &str = "\
SB.A 1
SB.A 1
SB.A 2
SB 3
SB.A 2
SB.B 3
SB.B 3
SB.A 30
SB 31 HIGH MINOR
SB.A 30 HIGH MINOR
";

const ALARM: &str = "\
SB 1
SB 31 HIGH MINOR
SB.A 30 HIGH MINOR
SB 31
";

const LOG: &str = "\
SB.A 1
SB.A 1
SB.A 2
SB.A 2
SB.B 3
SB.B 3
SB.A 30
SB 31 HIGH MINOR
SB.A 30 HIGH MINOR
";

/// Appends each event it passes, as `camonitor` prints it without the time.
struct Line {
    pv: &'static str,
    stream: Arc<Mutex<String>>,
}

impl SubscriptionFilter for Line {
    fn name(&self) -> &'static str {
        "line"
    }

    fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
        let e = &event.event;
        let value = match e.value().as_value() {
            Some(EpicsValue::Double(v)) => format!("{v}"),
            other => panic!("{}: {other:?}", self.pv),
        };
        let mut line = format!("{} {value}", self.pv);
        if e.sevr() != 0 {
            line += &format!(
                " {} {}",
                alarm_condition_string(e.stat()),
                MENU_ALARM_SEVR[e.sevr() as usize]
            );
        }
        let mut stream = self.stream.lock().unwrap();
        stream.push_str(&line);
        stream.push('\n');
        Some(event)
    }
}

/// The oracle's `subOracle`: VAL = A + 1, status 0.
fn sub_oracle(rec: &mut dyn Record) -> epics_base_rs::error::CaResult<i64> {
    let Some(EpicsValue::Double(a)) = rec.get_field("A") else {
        panic!("sub A is a double");
    };
    rec.put_field("VAL", EpicsValue::Double(a + 1.0))?;
    Ok(0)
}

#[epics_macros_rs::epics_test]
async fn sub_posts_inputs_from_its_own_la_as_the_oracle_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .register_subroutine("subOracle", sub_oracle)
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("SB").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field) in [
                ("SB", "VAL"),
                ("SB.A", "A"),
                ("SB.LA", "LA"),
                ("SB.B", "B"),
                ("SB.LB", "LB"),
                ("SB.MLST", "MLST"),
            ] {
                readers.push(
                    inst.add_subscriber(field, sid, DbFieldType::Double, class.bits())
                        .unwrap(),
                );
                sid += 1;
                assert!(inst.attach_filter_to_last_subscriber(
                    field,
                    Arc::new(Line {
                        pv,
                        stream: stream.clone(),
                    }),
                ));
            }
        }
    }
    for (field, value) in STEPS {
        db.put_record_field_from_ca("SB", field, EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(got[0], VALUE, "DBE_VALUE");
    assert_eq!(got[1], ALARM, "DBE_ALARM");
    assert_eq!(got[2], LOG, "DBE_LOG");
    // The oracle's `caget` after the steps.
    for (pv, v) in [("SB.LA", 30.0), ("SB.LB", 3.0), ("SB.LC", 0.0)] {
        assert_eq!(db.get_pv(pv).unwrap(), EpicsValue::Double(v), "{pv}");
    }
}
