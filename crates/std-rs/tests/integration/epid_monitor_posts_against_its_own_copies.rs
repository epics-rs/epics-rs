//! epid's `monitor()` (`epidRecord.c:346-410`) against the std oracle IOC
//! (oracleStdIoc), three `camonitor -m v|a|l` clients on `E E.I E.P E.D E.ERR
//! E.OVAL E.CVAL E.IP` (and, for the VAL pin, on `E` alone), driven with `caput E.PROC 1; caput E.PROC 1;
//! caput E.I 5; caput E.PROC 1; caput E.IP 7; caput E.PROC 1; caput E:RBV 4;
//! caput E.PROC 1`. The streams below are the oracle's with the timestamps and
//! the connect values cut.
//!
//! VAL is held at its STPL constant, so it posts once: MDEL is 0 and C's
//! deadband test is a strict `>` against MLST.
//!
//! Each secondary posts when it differs from its own old copy (OVLP PP IP DP
//! DTP ERRP CVLP), not from the value last published: with KI=0 the cycle after
//! `caput E.I 5` puts I back to 0 silently, because IP is still 0; a put to IP
//! makes the next cycle post the unchanged I. DT carries the wall clock and is
//! left out.

// RTEMS-EXEC-MODEL-ALLOW(2): checked, not waived — all 2 ran and passed on the
// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread`); no CA server is built.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(ai, "E:RBV") {
  field(VAL, "3")
}
record(epid, "E") {
  field(INP, "E:RBV")
  field(STPL, "1")
  field(KP, "2")
  field(DRVH, "200")
  field(DRVL, "-200")
}
"#;

const STEPS: [(&str, &str, &str); 8] = [
    ("E", "PROC", "1"),
    ("E", "PROC", "1"),
    ("E", "I", "5"),
    ("E", "PROC", "1"),
    ("E", "IP", "7"),
    ("E", "PROC", "1"),
    ("E:RBV", "VAL", "4"),
    ("E", "PROC", "1"),
];

const VALUE: &str = "\
--
E 1
E.OVAL -4
E.P -4
E.ERR -2
E.CVAL 3
--
--
E.I 5
--
--
E.IP 7
--
E.I 0
--
--
E.OVAL -6
E.P -6
E.ERR -3
E.CVAL 4
";

const LOG: &str = VALUE;

const ALARM: &str = "\
--
E 1
--
--
--
--
--
--
--
";

const VAL_ONLY: &str = "\
--
E 1
--
--
--
--
--
--
--
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

/// A stream as the posts of each step, in order. `ordered` keeps each step's
/// own order; otherwise a step's posts are sorted, because the walk emits a
/// cycle's secondaries in its own order where C posts them in `monitor()`'s.
/// Unordered until records-b's 4(c)(ii) post-order rule lands.
fn steps(stream: &str, ordered: bool) -> Vec<Vec<&str>> {
    let mut steps: Vec<&str> = stream.split(STEP).collect();
    assert_eq!(steps.remove(0), "", "a stream starts with its first step");
    steps
        .into_iter()
        .map(|step| {
            let mut posts: Vec<&str> = step.lines().collect();
            if !ordered {
                posts.sort_unstable();
            }
            posts
        })
        .collect()
}

/// Subscribes `fields` of E in the three classes and runs the steps.
async fn streams(fields: &[(&'static str, &str)]) -> Vec<String> {
    std_rs::register_std_dbd();
    let (db, _) = IocBuilder::new()
        .register_record_type("epid", || Box::new(std_rs::EpidRecord::default()))
        .register_record_type("ai", || Box::new(AiRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("E").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for &(pv, field) in fields {
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
    for (rec, field, value) in STEPS {
        for stream in &streams {
            stream.lock().unwrap().push_str(STEP);
        }
        db.put_record_field_from_ca(rec, field, EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    streams.iter().map(|s| s.lock().unwrap().clone()).collect()
}

#[tokio::test]
async fn epid_posts_a_held_val_once_as_the_oracle_does() {
    let got = streams(&[("E", "VAL")]).await;
    for (got, want, class) in [
        (&got[0], VAL_ONLY, "DBE_VALUE"),
        (&got[1], ALARM, "DBE_ALARM"),
        (&got[2], VAL_ONLY, "DBE_LOG"),
    ] {
        assert_eq!(steps(got, true), steps(want, true), "{class}");
    }
}

#[tokio::test]
async fn epid_posts_against_its_own_copies_as_the_oracle_does() {
    let got = streams(&[
        ("E", "VAL"),
        ("E.I", "I"),
        ("E.P", "P"),
        ("E.D", "D"),
        ("E.ERR", "ERR"),
        ("E.OVAL", "OVAL"),
        ("E.CVAL", "CVAL"),
        ("E.IP", "IP"),
    ])
    .await;
    for (got, want, class) in [
        (&got[0], VALUE, "DBE_VALUE"),
        (&got[1], ALARM, "DBE_ALARM"),
        (&got[2], LOG, "DBE_LOG"),
    ] {
        assert_eq!(steps(got, false), steps(want, false), "{class}");
    }
}
