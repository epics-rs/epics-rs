//! histogram's `monitor()` (`histogramRecord.c:282-297`) against softIoc
//! R7.0.10, three `camonitor -m v|a|l` clients on `HG HG.MCNT HG.CSTA HG.SGNL
//! HG.WDTH`, driven with `caput HG.PROC 1; HG.SGNL 0.5; HG.PROC 1; HG.SGNL 1.5;
//! HG.PROC 1; HG.PROC 1; HG.SGNL 2.5; HG.PROC 1; HG.CMD 1; HG.SGNL 3.5;
//! HG.PROC 1; HG.PROC 1`. The streams below are softIoc's with the timestamps
//! and the connect values cut.
//!
//! The boundaries: VAL posts when MCNT passes MDEL, so a cycle that leaves
//! MCNT at MDEL posts nothing unless the alarm moved; MCNT, CSTA and WDTH, which the cycle and the
//! puts write, never post; SGNL posts from its own put.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(histogram, "HG") {
  field(NELM, "4")
  field(ULIM, "4")
  field(LLIM, "0")
  field(MDEL, "1")
}
"#;

const STEPS: [(&str, &str); 12] = [
    ("PROC", "1"),
    ("SGNL", "0.5"),
    ("PROC", "1"),
    ("SGNL", "1.5"),
    ("PROC", "1"),
    ("PROC", "1"),
    ("SGNL", "2.5"),
    ("PROC", "1"),
    ("CMD", "1"),
    ("SGNL", "3.5"),
    ("PROC", "1"),
    ("PROC", "1"),
];

const VALUE: &str = "\
HG.SGNL 0.5
HG 4 3 0 0 0
HG.SGNL 1.5
HG 4 3 2 0 0
HG.SGNL 2.5
HG 4 3 3 2 0
HG.SGNL 3.5
HG 4 0 0 0 2
";

const ALARM: &str = "\
HG 4 1 0 0 0
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
            Some(EpicsValue::Double(v)) => format!("{v}"),
            Some(EpicsValue::Short(v)) => format!("{v}"),
            Some(EpicsValue::ULong(v)) => format!("{v}"),
            Some(v @ EpicsValue::ULongArray(_)) => counts(v),
            // Read at delivery: the test fills it in after the step.
            None => "@".to_string(),
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
async fn histogram_posts_val_past_mdel_as_softioc_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("HG").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field, ty) in [
                ("HG", "VAL", DbFieldType::Long),
                ("HG.MCNT", "MCNT", DbFieldType::Short),
                ("HG.CSTA", "CSTA", DbFieldType::Short),
                ("HG.SGNL", "SGNL", DbFieldType::Double),
                ("HG.WDTH", "WDTH", DbFieldType::Double),
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
        db.put_record_field_from_ca("HG", field, EpicsValue::String(value.into()))
            .await
            .unwrap();
        // A step posts VAL at most once, so the counts read now are the ones
        // its post carried.
        let now = counts(&db.get_pv("HG").unwrap());
        for stream in &streams {
            let mut stream = stream.lock().unwrap();
            *stream = stream.replace("HG @", &format!("HG {now}"));
        }
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(got[0], VALUE, "DBE_VALUE");
    assert_eq!(got[1], ALARM, "DBE_ALARM");
    assert_eq!(got[2], LOG, "DBE_LOG");
}

/// The counts as `camonitor` prints an array: the length, then each element.
fn counts(v: &EpicsValue) -> String {
    let EpicsValue::ULongArray(v) = v else {
        panic!("HG: {v:?}");
    };
    std::iter::once(v.len().to_string())
        .chain(v.iter().map(u32::to_string))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A SGNL put before any process, against softIoc R7.0.10 (`camonitor -m
/// v|a|l HG.SGNL`, `caput HG.SGNL 0.5; HG.SGNL 1.5`): `add_count` leaves
/// stat/sevr alone on sane limits, so each post carries the born UDF INVALID.
const UNPROCESSED_VALUE: &str = "\
HG.SGNL 0.5 UDF INVALID
HG.SGNL 1.5 UDF INVALID
";

#[epics_macros_rs::epics_test]
async fn a_sgnl_put_before_any_process_posts_udf_invalid_as_softioc_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("HG").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        for (sid, (class, stream)) in classes.iter().zip(&streams).enumerate() {
            readers.push(
                inst.add_subscriber("SGNL", sid as u32, DbFieldType::Double, class.bits())
                    .unwrap(),
            );
            assert!(inst.attach_filter_to_last_subscriber(
                "SGNL",
                Arc::new(Line {
                    pv: "HG.SGNL",
                    stream: stream.clone(),
                }),
            ));
        }
    }
    for value in ["0.5", "1.5"] {
        db.put_record_field_from_ca("HG", "SGNL", EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(got[0], UNPROCESSED_VALUE, "DBE_VALUE");
    assert_eq!(got[1], "", "DBE_ALARM");
    assert_eq!(got[2], UNPROCESSED_VALUE, "DBE_LOG");
}
