//! throttle's `valuePut` posts (`throttleRecord.c:542-615`, std `06c6f4a`)
//! and `process()`'s (:231-312) against the std oracle IOC (oracleStdIoc),
//! three `camonitor -m v|a|l` clients on `T T.SENT T.OSENT T.WAIT T.STS
//! T.DRVLS T.OVAL T.DRVLC`, driven with `caput T 1; caput T 1; caput T 20;
//! caput T 5; caput T.DRVLC On; caput T 30; caput T -5; caput T 3`. The
//! streams below are the oracle's with the timestamps and the connect values
//! cut, compared step by step in order.
//!
//! DLY is 0, so every accepted value goes out at once: WAIT posts True
//! (:288), SENT with a bare DBE_VALUE (:569), WAIT False (:578), STS (:592),
//! then SENT again under `valuePut`'s own `recGblResetAlarms` when it moved
//! (:614), so the UDF clear's DBE_ALARM lands on SENT and not on VAL. A
//! rejected value posts only DRVLS (:281). With DRVLC On an out-of-range
//! value is clipped and VAL posts the limit with DBE_VALUE|DBE_LOG (:251
//! low, :265 high) ahead of everything else in the cycle.
//!
//! On the first send the posts ahead of the reset carry the pre-reset `UDF
//! INVALID`.

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — ran and passed on the
// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread`); no CA server is built.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(ao, "T:O") {
}
record(throttle, "T") {
  field(OUT, "T:O")
  field(DRVLH, "10")
  field(DRVLL, "0")
}
"#;

/// The menus of `throttleRecord.dbd:1-28`, by the field a pv names.
fn menu(pv: &str) -> &'static [&'static str] {
    match pv {
        "T.WAIT" => &["False", "True"],
        "T.DRVLS" => &["Normal", "Low Limit", "High Limit"],
        "T.STS" => &["Unknown", "Error", "Success"],
        "T.OV" | "T.SIV" => &["Ext PV NC", "Ext PV OK", "Local PV", "Constant"],
        "T.DRVLC" => &["Off", "On"],
        "T.SYNC" => &["Idle", "Process"],
        pv => panic!("{pv} is not a menu"),
    }
}

const STEPS: [(&str, &str); 8] = [
    ("VAL", "1"),
    ("VAL", "1"),
    ("VAL", "20"),
    ("VAL", "5"),
    ("DRVLC", "On"),
    ("VAL", "30"),
    ("VAL", "-5"),
    ("VAL", "3"),
];

const VALUE: &str = "\
--
T.WAIT True UDF INVALID
T.SENT 1 UDF INVALID
T.WAIT False UDF INVALID
T.STS Success UDF INVALID
T.SENT 1
T 1
--
T.WAIT True
T.SENT 1
T.WAIT False
T.STS Success
--
T.DRVLS High Limit
--
T.DRVLS Normal
T.WAIT True
T.SENT 5
T.WAIT False
T.STS Success
T.SENT 5
T 5
--
T.DRVLC On
--
T 10
T.DRVLS High Limit
T.WAIT True
T.SENT 10
T.WAIT False
T.STS Success
T.SENT 10
T 10
--
T 0
T.DRVLS Low Limit
T.WAIT True
T.SENT 0
T.WAIT False
T.STS Success
T.SENT 0
T 0
--
T.DRVLS Normal
T.WAIT True
T.SENT 3
T.WAIT False
T.STS Success
T.SENT 3
T 3
";

const ALARM: &str = "--\nT.SENT 1\n--\n--\n--\n--\n--\n--\n--\n";

const LOG: &str = "\
--
T.SENT 1
T 1
--
--
--
T.SENT 5
T 5
--
T.DRVLC On
--
T 10
T.SENT 10
T 10
--
T 0
T.SENT 0
T 0
--
T.SENT 3
T 3
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
            Some(EpicsValue::Enum(v)) => menu(self.pv)[*v as usize].to_string(),
            other => panic!("{}: {other:?}", self.pv),
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
        Some(event)
    }
}

/// Starts each step's posts in a stream; [`steps`] cuts on it.
const STEP: &str = "--\n";

/// A stream as the posts of each step, in order.
fn steps(stream: &str) -> Vec<Vec<&str>> {
    let mut steps: Vec<&str> = stream.split(STEP).collect();
    assert_eq!(steps.remove(0), "", "a stream starts with its first step");
    steps
        .into_iter()
        .map(|step| step.lines().collect())
        .collect()
}

#[tokio::test]
async fn throttle_value_put_posts_as_the_oracle_does() {
    std_rs::register_std_dbd();
    let (db, _) = IocBuilder::new()
        .register_record_type("throttle", || Box::new(std_rs::ThrottleRecord::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("T").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field, ty) in [
                ("T", "VAL", DbFieldType::Double),
                ("T.SENT", "SENT", DbFieldType::Double),
                ("T.OSENT", "OSENT", DbFieldType::Double),
                ("T.WAIT", "WAIT", DbFieldType::Enum),
                ("T.STS", "STS", DbFieldType::Enum),
                ("T.DRVLS", "DRVLS", DbFieldType::Enum),
                ("T.OVAL", "OVAL", DbFieldType::Double),
                ("T.DRVLC", "DRVLC", DbFieldType::Enum),
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
        for stream in &streams {
            stream.lock().unwrap().push_str(STEP);
        }
        db.put_record_field_from_ca("T", field, EpicsValue::String(value.into()))
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
