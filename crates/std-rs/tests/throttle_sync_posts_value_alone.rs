//! throttle's `valueSync` posts (`throttleRecord.c:616-656`) against the std
//! oracle IOC (oracleStdIoc), three `camonitor -m v|a|l` clients on `T T.STS
//! T.SYNC U U.STS U.SYNC`, driven with `caput T.SYNC Process; caput T:I 6;
//! caput T.SYNC Process; caput U.SYNC Process`. The streams below are the
//! oracle's with the timestamps and the connect values cut.
//!
//! VAL, STS and SYNC each post with DBE_VALUE alone (:639, :650, :654); the
//! second SYNC on the value client, and the only one on the log client, is
//! dbPut's own post of the field it wrote. U's SINP fails its DBR_DOUBLE
//! read, so it posts STS Error and no VAL.

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — ran and passed on the
// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread`); no CA server is built.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::stringin::StringinRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(ao, "T:I") {
  field(VAL, "4")
}
record(stringin, "T:S") {
  field(VAL, "abc")
}
record(ao, "T:O") {
}
record(throttle, "T") {
  field(OUT, "T:O")
  field(SINP, "T:I")
}
record(throttle, "U") {
  field(OUT, "T:O")
  field(SINP, "T:S")
}
"#;

const STEPS: [(&str, &str, &str); 4] = [
    ("T", "SYNC", "Process"),
    ("T:I", "VAL", "6"),
    ("T", "SYNC", "Process"),
    ("U", "SYNC", "Process"),
];

const VALUE: &str = "\
--
T 4 UDF INVALID
T.STS Success UDF INVALID
T.SYNC Idle UDF INVALID
T.SYNC Idle UDF INVALID
--
--
T 6 UDF INVALID
T.STS Success UDF INVALID
T.SYNC Idle UDF INVALID
T.SYNC Idle UDF INVALID
--
U.STS Error UDF INVALID
U.SYNC Idle UDF INVALID
U.SYNC Idle UDF INVALID
";

const ALARM: &str = "--\n--\n--\n--\n";

const LOG: &str = "\
--
T.SYNC Idle UDF INVALID
--
--
T.SYNC Idle UDF INVALID
--
U.SYNC Idle UDF INVALID
";

/// The menus of `throttleRecord.dbd:1-28`, by the field a pv names.
fn menu(field: &str) -> &'static [&'static str] {
    match field {
        "STS" => &["Unknown", "Error", "Success"],
        "SYNC" => &["Idle", "Process"],
        field => panic!("{field} is not a menu"),
    }
}

/// Appends each event it passes, as `camonitor` prints it without the time.
struct Line {
    pv: &'static str,
    field: &'static str,
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
            Some(EpicsValue::Enum(v)) => menu(self.field)[*v as usize].to_string(),
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

/// Starts each step's posts in a stream; [`steps`] cuts on it.
const STEP: &str = "--\n";

/// A stream as the posts of each step, sorted: the walk emits a cycle's posts
/// in its own order.
fn steps(stream: &str) -> Vec<Vec<&str>> {
    let mut steps: Vec<&str> = stream.split(STEP).collect();
    assert_eq!(steps.remove(0), "", "a stream starts with its first step");
    steps
        .into_iter()
        .map(|step| {
            let mut posts: Vec<&str> = step.lines().collect();
            posts.sort_unstable();
            posts
        })
        .collect()
}

#[tokio::test]
async fn throttle_sync_posts_value_alone_as_the_oracle_does() {
    std_rs::register_std_dbd();
    let (db, _) = IocBuilder::new()
        .register_record_type("throttle", || Box::new(std_rs::ThrottleRecord::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .register_record_type("stringin", || Box::new(StringinRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    let mut sid = 0;
    for rec in ["T", "U"] {
        let cell = db.get_record(rec).unwrap();
        let mut inst = cell.write();
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field, ty) in [
                ("", "VAL", DbFieldType::Double),
                (".STS", "STS", DbFieldType::Enum),
                (".SYNC", "SYNC", DbFieldType::Enum),
            ] {
                readers.push(inst.add_subscriber(field, sid, ty, class.bits()).unwrap());
                sid += 1;
                let pv: &'static str = format!("{rec}{pv}").leak();
                assert!(inst.attach_filter_to_last_subscriber(
                    field,
                    Arc::new(Line {
                        pv,
                        field,
                        stream: stream.clone(),
                    }),
                ));
            }
        }
    }
    for (rec, field, value) in STEPS {
        for stream in &streams {
            stream.lock().unwrap().push_str(STEP);
        }
        db.put_record_field_from_ca(rec, field, EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    for (got, want, class) in [
        (&got[0], VALUE, "DBE_VALUE"),
        (&got[1], ALARM, "DBE_ALARM"),
        (&got[2], LOG, "DBE_LOG"),
    ] {
        assert_eq!(steps(got), steps(want), "{class}");
    }
}
