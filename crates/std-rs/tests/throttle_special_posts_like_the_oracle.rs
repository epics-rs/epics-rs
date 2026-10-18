//! throttle `special()`'s own posts (`throttleRecord.c:315-448`, std
//! `06c6f4a`) against the std oracle IOC (oracleStdIoc), three `camonitor -m
//! v|a|l` clients on the record's fields, one `caput` per step. The streams
//! below are the oracle's with the timestamps and the connect values cut.

// RTEMS-EXEC-MODEL-ALLOW(3): checked, not waived — all 3 ran and passed on the
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

/// A caput.
enum Step {
    Put(&'static str, &'static str, &'static str),
}

/// The menus of `throttleRecord.dbd:1-28`, by the field a pv names.
fn menu(field: &str) -> &'static [&'static str] {
    match field {
        "STS" => &["Unknown", "Error", "Success"],
        "SYNC" => &["Idle", "Process"],
        "WAIT" => &["False", "True"],
        "DRVLS" => &["Normal", "Low Limit", "High Limit"],
        "OV" | "SIV" => &["Ext PV NC", "Ext PV OK", "Local PV", "Constant"],
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

/// Subscribes `fields` of each of `records` in the three classes, runs
/// `steps` and returns the value, alarm and log streams.
async fn streams(
    db: &str,
    records: &[&str],
    fields: &[(&'static str, DbFieldType)],
    steps: &[Step],
) -> Vec<String> {
    std_rs::register_std_dbd();
    let (db, _) = IocBuilder::new()
        .register_record_type("throttle", || Box::new(std_rs::ThrottleRecord::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    let mut sid = 0;
    for rec in records {
        let cell = db.get_record(rec).unwrap();
        let mut inst = cell.write();
        for (class, stream) in classes.iter().zip(&streams) {
            for &(field, ty) in fields {
                readers.push(inst.add_subscriber(field, sid, ty, class.bits()).unwrap());
                sid += 1;
                let pv = match field {
                    "VAL" => rec.to_string(),
                    field => format!("{rec}.{field}"),
                };
                assert!(inst.attach_filter_to_last_subscriber(
                    field,
                    Arc::new(Line {
                        pv: pv.leak(),
                        field,
                        stream: stream.clone(),
                    }),
                ));
            }
        }
    }
    for step in steps {
        for stream in &streams {
            stream.lock().unwrap().push_str(STEP);
        }
        let Step::Put(rec, field, value) = *step;
        db.put_record_field_from_ca(rec, field, EpicsValue::String(value.into()))
            .await
            .unwrap();
        // A link put's OV/SIV come from a classification task.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    streams.iter().map(|s| s.lock().unwrap().clone()).collect()
}

/// A DRVLH/DRVLL put posts DRVLS with DBE_VALUE when the new limits move it
/// (`:434-439`); `caput T 20; caput T.DRVLL 30; caput T.DRVLL 2; caput
/// T.DRVLH 10; caput T.DRVLH 1`.
#[tokio::test]
async fn a_limit_put_posts_the_drvls_it_moves_as_the_oracle_does() {
    const STEPS: [Step; 5] = [
        Step::Put("T", "VAL", "20"),
        Step::Put("T", "DRVLL", "30"),
        Step::Put("T", "DRVLL", "2"),
        Step::Put("T", "DRVLH", "10"),
        Step::Put("T", "DRVLH", "1"),
    ];
    const VALUE: &str = "\
--
T.DRVLS High Limit UDF INVALID
--
T.DRVLS Normal
--
T.DRVLS Low Limit
--
--
T.DRVLS Normal
";
    const ALARM: &str = "--\nT 0\n--\n--\n--\n--\n";
    const LOG: &str = "--\n--\n--\n--\n--\n";
    let got = streams(
        DB,
        &["T"],
        &[("VAL", DbFieldType::Double), ("DRVLS", DbFieldType::Enum)],
        &STEPS,
    )
    .await;
    for (got, want, class) in [
        (&got[0], VALUE, "DBE_VALUE"),
        (&got[1], ALARM, "DBE_ALARM"),
        (&got[2], LOG, "DBE_LOG"),
    ] {
        assert_eq!(steps(got), steps(want), "{class}");
    }
}

/// A negative DLY put is clamped to 0 and posts DLY with DBE_VALUE
/// (`:393-398`), ahead of dbPut's own post of the field it wrote; `caput
/// T.DLY -1; caput T.DLY 0.5; caput T.DLY -2`.
#[tokio::test]
async fn a_negative_delay_posts_its_clamp_as_the_oracle_does() {
    const STEPS: [Step; 3] = [
        Step::Put("T", "DLY", "-1"),
        Step::Put("T", "DLY", "0.5"),
        Step::Put("T", "DLY", "-2"),
    ];
    const VALUE: &str = "\
--
T.DLY 0 UDF INVALID
T.DLY 0 UDF INVALID
--
T.DLY 0.5 UDF INVALID
--
T.DLY 0 UDF INVALID
T.DLY 0 UDF INVALID
";
    const ALARM: &str = "--\n--\n--\n";
    const LOG: &str = "\
--
T.DLY 0 UDF INVALID
--
T.DLY 0.5 UDF INVALID
--
T.DLY 0 UDF INVALID
";
    let got = streams(DB, &["T"], &[("DLY", DbFieldType::Double)], &STEPS).await;
    for (got, want, class) in [
        (&got[0], VALUE, "DBE_VALUE"),
        (&got[1], ALARM, "DBE_ALARM"),
        (&got[2], LOG, "DBE_LOG"),
    ] {
        assert_eq!(steps(got), steps(want), "{class}");
    }
}

/// An OUT put re-classifies and posts OV alone, a SINP put SIV alone
/// (`:341-374`); `caput T.OUT T:O; caput T.SINP T:O; caput T.OUT 5; caput
/// T.SINP 7`.
#[tokio::test]
async fn a_link_put_posts_only_its_own_status_as_the_oracle_does() {
    const STEPS: [Step; 4] = [
        Step::Put("T", "OUT", "T:O"),
        Step::Put("T", "SINP", "T:O"),
        Step::Put("T", "OUT", "5"),
        Step::Put("T", "SINP", "7"),
    ];
    const VALUE: &str = "\
--
T.OV Local PV UDF INVALID
--
T.SIV Local PV UDF INVALID
--
T.OV Constant UDF INVALID
--
T.SIV Constant UDF INVALID
";
    const ALARM: &str = "--\n--\n--\n--\n";
    let got = streams(
        DB,
        &["T"],
        &[("OV", DbFieldType::Enum), ("SIV", DbFieldType::Enum)],
        &STEPS,
    )
    .await;
    for (got, want, class) in [
        (&got[0], VALUE, "DBE_VALUE"),
        (&got[1], ALARM, "DBE_ALARM"),
        (&got[2], VALUE, "DBE_LOG"),
    ] {
        assert_eq!(steps(got), steps(want), "{class}");
    }
}
