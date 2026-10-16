//! compress's `monitor()` (`compressRecord.c:101-111`) against softIoc
//! R7.0.10, three `camonitor -m v|a|l` clients on `CB.NUSE CB.OUSE CB.RES
//! CB.OFF CB.INX`, driven with `caput CB.PROC 1; SRC 2; CB.PROC 1 (x3); SRC 9;
//! CB.PROC 1 (x2); SRC 1; CB.PROC 1; CB.RES 1; CB.PROC 1`. The streams below
//! are softIoc's with the timestamps and the connect values cut.
//!
//! The boundaries: NUSE posts on `nuse != ouse` or an alarm move, so a full
//! buffer posts it again only when INP's MS severity moves the alarm;
//! `RES`'s `special()` posts NUSE 0 through `monitor()`. OUSE, OFF and INX,
//! which the cycle writes, never post.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(ao, "SRC") {
  field(HIGH, "5")
  field(HSV, "MINOR")
}
record(compress, "CB") {
  field(ALG, "Circular Buffer")
  field(NSAM, "3")
  field(INP, "SRC MS")
}
"#;

const STEPS: [(&str, &str, &str); 12] = [
    ("CB", "PROC", "1"),
    ("SRC", "VAL", "2"),
    ("CB", "PROC", "1"),
    ("CB", "PROC", "1"),
    ("CB", "PROC", "1"),
    ("SRC", "VAL", "9"),
    ("CB", "PROC", "1"),
    ("CB", "PROC", "1"),
    ("SRC", "VAL", "1"),
    ("CB", "PROC", "1"),
    ("CB", "RES", "1"),
    ("CB", "PROC", "1"),
];

const VALUE: &str = "\
CB.NUSE 1 LINK INVALID
CB.NUSE 2
CB.NUSE 3
CB.NUSE 3 LINK MINOR
CB.NUSE 3
CB.NUSE 0
CB.RES 0
CB.NUSE 1
";

const ALARM: &str = "\
CB.NUSE 1 LINK INVALID
CB.NUSE 2
CB.NUSE 3 LINK MINOR
CB.NUSE 3
";

const LOG: &str = VALUE;

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
            Some(EpicsValue::Long(v)) => format!("{v}"),
            Some(EpicsValue::ULong(v)) => format!("{v}"),
            Some(EpicsValue::Short(v)) => format!("{v}"),
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

#[epics_macros_rs::epics_test]
async fn compress_posts_nuse_from_its_own_ouse_as_softioc_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("CB").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field) in [
                ("CB.NUSE", "NUSE"),
                ("CB.OUSE", "OUSE"),
                ("CB.RES", "RES"),
                ("CB.OFF", "OFF"),
                ("CB.INX", "INX"),
            ] {
                readers.push(
                    inst.add_subscriber(field, sid, DbFieldType::Long, class.bits())
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
    for (record, field, value) in STEPS {
        db.put_record_field_from_ca(record, field, EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(got[0], VALUE, "DBE_VALUE");
    assert_eq!(got[1], ALARM, "DBE_ALARM");
    assert_eq!(got[2], LOG, "DBE_LOG");
}

/// `INP` names no record, so every cycle raises LINK INVALID. Against
/// oracleStdIoc, `camonitor -m v|a|l CP2.NUSE`, driven with `caput CP2.PROC
/// 1; CP2.PROC 1; ACR 1; LCN 2; CP2.PROC 1`: the RES and N `special()`s run
/// `monitor()`, whose alarm reset clears the severity and posts NUSE, and the
/// PP cycle after each raises it again; an unmoved alarm posts no NUSE.
const LINK_DB: &str = r#"
record(compress, "CP2") {
  field(ALG, "Circular Buffer")
  field(NSAM, "3")
  field(INP, "NOPE_ABSENT")
}
record(ao, "ACR") {
  field(OUT, "CP2.RES PP")
}
record(longout, "LCN") {
  field(OUT, "CP2.N PP")
}
"#;

const LINK_STEPS: [(&str, &str, &str); 5] = [
    ("CP2", "PROC", "1"),
    ("CP2", "PROC", "1"),
    ("ACR", "VAL", "1"),
    ("LCN", "VAL", "2"),
    ("CP2", "PROC", "1"),
];

const LINK_NUSE: &str = "\
CP2.NUSE 0 LINK INVALID
CP2.NUSE 0
CP2.NUSE 0 LINK INVALID
CP2.NUSE 0
CP2.NUSE 0 LINK INVALID
";

#[epics_macros_rs::epics_test]
async fn a_failed_inp_posts_nuse_on_each_alarm_move_as_the_oracle_does() {
    let (db, _) = IocBuilder::new()
        .db_string(LINK_DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("CP2").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        for (sid, (class, stream)) in classes.iter().zip(&streams).enumerate() {
            readers.push(
                inst.add_subscriber("NUSE", sid as u32, DbFieldType::Long, class.bits())
                    .unwrap(),
            );
            assert!(inst.attach_filter_to_last_subscriber(
                "NUSE",
                Arc::new(Line {
                    pv: "CP2.NUSE",
                    stream: stream.clone(),
                }),
            ));
        }
    }
    for (record, field, value) in LINK_STEPS {
        db.put_record_field_from_ca(record, field, EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(got[0], LINK_NUSE, "DBE_VALUE");
    assert_eq!(got[1], LINK_NUSE, "DBE_ALARM");
    assert_eq!(got[2], LINK_NUSE, "DBE_LOG");
}
