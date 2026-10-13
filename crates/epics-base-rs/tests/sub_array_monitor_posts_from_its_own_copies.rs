//! subArray's `process()` NORD post and `monitor()` (`subArrayRecord.c:152-153,
//! 292-301`) against softIoc R7.0.10: a subArray reading waveform W through
//! INP, three `camonitor -m v|a|l` clients on `S S.NORD S.INDX`, driven with
//! `caput S.PROC 1; caput -a W 3 1 2 3; S.PROC 1; S.PROC 1; S.INDX 1;
//! S.PROC 1; S.NELM 3; S.PROC 1; caput -a W 1 9; S.PROC 1; S.PROC 1; S.INP
//! NOPE; S.PROC 1`. The streams below are softIoc's with the timestamps and the
//! connect values cut.
//!
//! The boundaries: VAL posts on every cycle; a read that moves NORD posts it
//! before `monitor()` resets the alarm, so it carries the previous cycle's;
//! an empty slice raises UDF, a failed INP LINK.
//!
//! Compared per PV with NORD's alarm cut: the port posts NORD from its
//! `monitor_cycle` walk, after the stamp and the alarm reset, so it carries
//! this cycle's alarm (`process()` posting it would carry the previous stamp,
//! which epics-base PR #359 fixed).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(waveform, "W") {
  field(FTVL, "DOUBLE")
  field(NELM, "4")
}
record(subArray, "S") {
  field(FTVL, "DOUBLE")
  field(MALM, "4")
  field(NELM, "2")
  field(INP, "W")
}
"#;

enum Step {
    Field(&'static str, &'static str),
    Put(&'static str, &'static [f64]),
}
use Step::*;

const STEPS: [Step; 13] = [
    Field("PROC", "1"),
    Put("W", &[1.0, 2.0, 3.0]),
    Field("PROC", "1"),
    Field("PROC", "1"),
    Field("INDX", "1"),
    Field("PROC", "1"),
    Field("NELM", "3"),
    Field("PROC", "1"),
    Put("W", &[9.0]),
    Field("PROC", "1"),
    Field("PROC", "1"),
    Field("INP", "NOPE"),
    Field("PROC", "1"),
];

const VALUE: &str = "\
S UDF INVALID
S.NORD 2 UDF INVALID
S 2 1 2
S 2 1 2
S.INDX 1
S 2 2 3
S 2 2 3
S 2 2 3
S 2 2 3
S.NORD 0
S UDF INVALID
S UDF INVALID
S LINK INVALID
";

const ALARM: &str = "\
S 2 1 2
S UDF INVALID
S LINK INVALID
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
            Some(v @ EpicsValue::DoubleArray(_)) => array(v),
            // Read at delivery: the test fills it in after the step.
            None => "@".to_string(),
            Some(EpicsValue::Double(v)) => format!("{v}"),
            Some(EpicsValue::Long(v)) => format!("{v}"),
            Some(EpicsValue::ULong(v)) => format!("{v}"),
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
        // `camonitor` prints no gap for an empty array before its alarm.
        let line = line.replace("  ", " ");
        let mut stream = self.stream.lock().unwrap();
        stream.push_str(&line);
        stream.push('\n');
        Some(event)
    }
}

#[epics_macros_rs::epics_test]
async fn sub_array_posts_val_and_nord_as_softioc_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("S").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field, ty) in [
                ("S", "VAL", DbFieldType::Double),
                ("S.NORD", "NORD", DbFieldType::Long),
                ("S.INDX", "INDX", DbFieldType::Long),
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
    for step in STEPS {
        let (record, field, value) = match step {
            Field(field, v) => ("S", field, EpicsValue::String(v.into())),
            Put(record, a) => (record, "VAL", EpicsValue::DoubleArray(a.to_vec().into())),
        };
        db.put_record_field_from_ca(record, field, value)
            .await
            .unwrap();
        // A step posts S's VAL at most once, so the array read now is the
        // one its post carried.
        let now = array(&db.get_pv("S").unwrap());
        for stream in &streams {
            let mut stream = stream.lock().unwrap();
            *stream = stream
                .replace("S @", &format!("S {now}"))
                .replace("  ", " ");
        }
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(by_pv(&got[0]), by_pv(VALUE), "DBE_VALUE");
    assert_eq!(by_pv(&got[1]), by_pv(ALARM), "DBE_ALARM");
    assert_eq!(by_pv(&got[2]), by_pv(LOG), "DBE_LOG");
}

/// Each PV's lines in the order it received them, NORD's without its alarm.
fn by_pv(stream: &str) -> std::collections::BTreeMap<&str, Vec<&str>> {
    let mut m = std::collections::BTreeMap::<&str, Vec<&str>>::new();
    for line in stream.lines() {
        let pv = line.split(' ').next().unwrap();
        let line = match pv {
            "S.NORD" => line
                .match_indices(' ')
                .nth(1)
                .map_or(line, |(i, _)| &line[..i]),
            _ => line,
        };
        m.entry(pv).or_default().push(line);
    }
    m
}

/// The array as `camonitor` prints it: the length, then each element; one
/// element alone, as a scalar; nothing at all for an empty one.
fn array(v: &EpicsValue) -> String {
    let EpicsValue::DoubleArray(v) = v else {
        panic!("S: {v:?}");
    };
    match v.len() {
        0 => return String::new(),
        1 => return v[0].to_string(),
        _ => {}
    }
    std::iter::once(v.len().to_string())
        .chain(v.iter().map(f64::to_string))
        .collect::<Vec<_>>()
        .join(" ")
}

/// subArray's BUSY is a device's flag C posts nowhere: neither
/// `subArrayRecord.c` (its one read, `:140`, gates an async completion) nor
/// `devSASoft.c` calls `db_post_events` on it, so a cycle that finds it moved
/// stays silent on every class.
#[epics_macros_rs::epics_test]
async fn a_moved_busy_posts_nothing_on_the_cycle() {
    use epics_base_rs::server::records::waveform::WaveformRecord;
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("S").unwrap();
    let all = EventMask::VALUE | EventMask::ALARM | EventMask::LOG;
    let mut busy = cell
        .write()
        .add_subscriber("BUSY", 1, DbFieldType::Short, all.bits())
        .unwrap();
    for set in [true, false] {
        let mut inst = cell.write();
        let wf = inst.record.as_any_mut();
        wf.and_then(|a| a.downcast_mut::<WaveformRecord>())
            .unwrap()
            .busy = set;
        drop(inst);
        db.put_record_field_from_ca("S", "PROC", EpicsValue::String("1".into()))
            .await
            .unwrap();
    }
    assert!(busy.try_recv().is_err(), "subArray BUSY posted on a cycle");
}
