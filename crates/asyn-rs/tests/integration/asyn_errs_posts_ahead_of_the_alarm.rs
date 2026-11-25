//! ERRS's post from `reportError` (`asynRecord.c:2044-2048`) against the asyn
//! R4-46 oracle IOC (oracleIoc), three `camonitor -m v|a|l` clients on `A.ERRS
//! A.SEVR A.STAT` of a record with no port, driven with `caput A.PROC 1`
//! twice. The streams below are the oracle's with the timestamps and the
//! connect values cut.
//!
//! The `stateNoDevice` arm calls `reportError` at :357, which posts ERRS with
//! DBE_VALUE|DBE_LOG there and then: ahead of `recGblSetSevr` (:361), the
//! stamp (:366) and `monitor()`'s alarm reset. So ERRS goes out first, with
//! NO_ALARM and the stamp the record had before the cycle (`camonitor -tsI`
//! shows it undefined, the record's never-stamped TIME), and SEVR and STAT
//! follow with the new stamp. The second cycle's text is unchanged and posts
//! nothing.
//!
//! ERRS posts by reference, so a line carries no value for it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use asyn_rs::asyn_record::AsynRecord;
use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const VALUE: &str = "\
--
A.ERRS
A.SEVR MINOR STATE MINOR
A.STAT STATE STATE MINOR
--
";

const ALARM: &str = "\
--
A.STAT STATE STATE MINOR
--
";

const LOG: &str = "\
--
A.ERRS
--
";

/// Appends each event it passes, as `camonitor` prints it without the time,
/// and keeps each event's stamp.
struct Line {
    pv: &'static str,
    stream: Arc<Mutex<String>>,
    stamps: Arc<Mutex<Vec<(&'static str, epics_base_rs::types::WallTime)>>>,
}

impl SubscriptionFilter for Line {
    fn name(&self) -> &'static str {
        "line"
    }

    fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
        let e = &event.event;
        let value = match (self.pv, e.value().as_value()) {
            ("A.ERRS", _) => String::new(),
            ("A.SEVR", Some(EpicsValue::Enum(v))) => MENU_ALARM_SEVR[*v as usize].to_string(),
            ("A.STAT", Some(EpicsValue::Enum(v))) => alarm_condition_string(*v).to_string(),
            (pv, other) => panic!("{pv}: {other:?}"),
        };
        let mut line = format!("{} {value}", self.pv).trim_end().to_string();
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
        self.stamps.lock().unwrap().push((self.pv, e.time()));
        Some(event)
    }
}

/// Starts each step's posts in a stream.
const STEP: &str = "--\n";

#[epics_base_rs::epics_test]
async fn errs_posts_ahead_of_the_alarm_and_the_stamp_as_the_oracle_does() {
    let (db, _) = IocBuilder::new()
        .register_record_type("asyn", || Box::new(AsynRecord::default()))
        .db_string("record(asyn, \"A\") {\n}\n", &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("A").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let stamps: Arc<Mutex<Vec<_>>> = Arc::default();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field, ty) in [
                ("A.ERRS", "ERRS", DbFieldType::Char),
                ("A.SEVR", "SEVR", DbFieldType::Enum),
                ("A.STAT", "STAT", DbFieldType::Enum),
            ] {
                readers.push(inst.add_subscriber(field, sid, ty, class.bits()).unwrap());
                sid += 1;
                assert!(inst.attach_filter_to_last_subscriber(
                    field,
                    Arc::new(Line {
                        pv,
                        stream: stream.clone(),
                        stamps: stamps.clone(),
                    }),
                ));
            }
        }
    }
    // TIME before and after each step.
    let mut times = Vec::new();
    for _ in 0..2 {
        for stream in &streams {
            stream.lock().unwrap().push_str(STEP);
        }
        let before = cell.read().common.time;
        db.put_record_field_from_ca("A", "PROC", EpicsValue::String("1".into()))
            .await
            .unwrap();
        times.push((before, cell.read().common.time));
    }
    for (got, want, class) in [
        (&streams[0], VALUE, "DBE_VALUE"),
        (&streams[1], ALARM, "DBE_ALARM"),
        (&streams[2], LOG, "DBE_LOG"),
    ] {
        assert_eq!(*got.lock().unwrap(), want, "{class}");
    }
    // Every post is the first step's.
    let (before, after) = times[0];
    assert_ne!(before, after, "the cycle stamped the record");
    for (pv, stamp) in stamps.lock().unwrap().iter() {
        let want = if *pv == "A.ERRS" { before } else { after };
        assert_eq!(*stamp, want, "{pv}'s stamp");
    }
}
