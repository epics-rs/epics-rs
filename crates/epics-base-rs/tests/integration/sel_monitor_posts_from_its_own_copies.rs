//! sel's `monitor()` (`selRecord.c:308-340`) against softIoc R7.0.10, three
//! `camonitor -m v|a|l` clients on `SL SL.SELN SL.A SL.B SL.LA SL.NLST`,
//! driven with `caput SL.PROC 1; SL.SELN 1; SL.B 9; SL.PROC 1; SL.A 3;
//! SL.PROC 1; SL.SELN 0; SL.B 2; SL.PROC 1`. The streams below are softIoc's
//! with the timestamps and the connect values cut.
//!
//! The boundaries: SELN posts against `nlst` and A..L against LA..LL, with
//! VAL's mask widened by `DBE_VALUE | DBE_LOG` whether or not VAL posts. A..L
//! start as NaN (`:88-110`), and `*pnew != *pprev` holds for NaN, so A posts
//! nan on every cycle until it is set. LA and NLST never post.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(sel, "SL") {
  field(SELM, "Specified")
  field(SELN, "0")
  field(A, "1")
  field(B, "2")
  field(HIGH, "5")
  field(HSV, "MINOR")
}
"#;

const STEPS: [(&str, &str); 9] = [
    ("PROC", "1"),
    ("SELN", "1"),
    ("B", "9"),
    ("PROC", "1"),
    ("A", "3"),
    ("PROC", "1"),
    ("SELN", "0"),
    ("B", "2"),
    ("PROC", "1"),
];

const VALUE: &str = "\
SL nan UDF INVALID
SL.A nan UDF INVALID
SL.B nan UDF INVALID
SL.SELN 1 UDF INVALID
SL.B 9 UDF INVALID
SL 9 HIGH MINOR
SL.SELN 1 HIGH MINOR
SL.A nan HIGH MINOR
SL.B 9 HIGH MINOR
SL.A nan HIGH MINOR
SL.A 3 HIGH MINOR
SL.A 3 HIGH MINOR
SL.SELN 0 HIGH MINOR
SL.B 2 HIGH MINOR
SL 3
SL.SELN 0
SL.B 2
";

const ALARM: &str = "\
SL 9 HIGH MINOR
SL.SELN 1 HIGH MINOR
SL.A nan HIGH MINOR
SL.B 9 HIGH MINOR
SL 3
SL.SELN 0
SL.B 2
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
            Some(EpicsValue::Double(v)) if v.is_nan() => "nan".to_string(),
            Some(EpicsValue::Double(v)) => format!("{v}"),
            Some(EpicsValue::UShort(v)) => format!("{v}"),
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
async fn sel_posts_seln_and_inputs_from_its_own_copies_as_softioc_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("SL").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field, ty) in [
                ("SL", "VAL", DbFieldType::Double),
                ("SL.SELN", "SELN", DbFieldType::Short),
                ("SL.A", "A", DbFieldType::Double),
                ("SL.B", "B", DbFieldType::Double),
                ("SL.LA", "LA", DbFieldType::Double),
                ("SL.NLST", "NLST", DbFieldType::Short),
            ] {
                readers.push(inst.add_subscriber(field, sid, ty, class.bits()).unwrap());
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
        db.put_record_field_from_ca("SL", field, EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    let got: Vec<String> = streams.iter().map(|s| by_pv(&s.lock().unwrap())).collect();
    assert_eq!(got[0], by_pv(VALUE), "DBE_VALUE");
    assert_eq!(got[1], by_pv(ALARM), "DBE_ALARM");
    assert_eq!(got[2], by_pv(LOG), "DBE_LOG");
}

/// `stream` with each PV's lines kept in order and the PVs apart: the walk
/// visits A..L in `last_posted`'s hash order, not C's index order.
fn by_pv(stream: &str) -> String {
    let mut lines: Vec<&str> = stream.lines().collect();
    lines.sort_by_key(|l| l.split(' ').next().unwrap_or_default());
    lines.iter().map(|l| format!("{l}\n")).collect()
}
