//! sseq's posts — `processCallback`'s DOn/STRn link reads (`sseqRecord.c:
//! 650-700`), `asyncFinish`'s VAL and BUSY (`:470-505`) and `special()`'s
//! DOn/STRn re-derivation — against oracleStdIoc, three `camonitor -m v|a|l`
//! clients on `Q Q.DO1 Q.STR1 Q.DO2 Q.STR2 Q.BUSY`, driven with `caput Q.PROC
//! 1; X 3; Q.PROC 1; Q.PROC 1; SI 4.5; Q.PROC 1; Q.DO1 7; Q.PROC 1; Q.STR2 abc;
//! Q.PROC 1`. The streams below are the oracle's with the timestamps and the
//! connect values cut.
//!
//! The boundaries: a numeric DOL read posts DOn with `DBE_VALUE|DBE_LOG` and
//! the STRn it re-renders with `DBE_VALUE`, a string one the reverse; an
//! unmoved read posts neither; the first cycle's posts carry the born UDF
//! alarm, which `asyncFinish` then clears; a DOn/STRn put posts both from
//! `special()` and the next cycle's read restores and posts them again.
//!
//! Compared per PV with BUSY's alarm cut: C posts BUSY 0 after `asyncFinish`'s
//! `recGblResetAlarms` with its `MonitorMask` (`:471`, `:505`), so the first
//! cycle's carries no alarm and reaches `DBE_ALARM`; the port publishes it
//! from `publish_post_write_fields` before the reset at `DBE_VALUE` alone, so
//! its first BUSY 0 carries UDF INVALID. That stage is a framework hunk.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(ao, "X") {}
record(stringin, "SI") {}
record(ao, "Y") {}
record(sseq, "Q") {
  field(DOL1, "X")
  field(LNK1, "Y PP")
  field(DOL2, "SI")
}
"#;

const STEPS: [(&str, &str, &str); 10] = [
    ("Q", "PROC", "1"),
    ("X", "VAL", "3"),
    ("Q", "PROC", "1"),
    ("Q", "PROC", "1"),
    ("SI", "VAL", "4.5"),
    ("Q", "PROC", "1"),
    ("Q", "DO1", "7"),
    ("Q", "PROC", "1"),
    ("Q", "STR2", "abc"),
    ("Q", "PROC", "1"),
];

const VALUE: &str = "\
Q.BUSY 1 UDF INVALID
Q.STR2 UDF INVALID
Q 0
Q.BUSY 0
Q.BUSY 1
Q.DO1 3
Q.STR1 3
Q 0
Q.BUSY 0
Q.BUSY 1
Q 0
Q.BUSY 0
Q.BUSY 1
Q.STR2 4.5
Q.DO2 4.5
Q 0
Q.BUSY 0
Q.STR1 7
Q.DO1 7
Q.BUSY 1
Q.DO1 3
Q.STR1 3
Q 0
Q.BUSY 0
Q.DO2 0
Q.STR2 abc
Q.BUSY 1
Q.STR2 4.5
Q.DO2 4.5
Q 0
Q.BUSY 0
";

const ALARM: &str = "\
Q 0
Q.BUSY 0
";

const LOG: &str = "\
Q.STR2 UDF INVALID
Q.DO1 3
Q.STR2 4.5
Q.DO1 7
Q.DO1 3
Q.STR2 abc
Q.STR2 4.5
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
            Some(EpicsValue::Long(v)) => format!("{v}"),
            Some(EpicsValue::Short(v)) => format!("{v}"),
            Some(EpicsValue::String(v)) => v.to_string(),
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
async fn sseq_posts_its_link_reads_as_the_oracle_does() {
    let (db, _) = IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("Q").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field, ty) in [
                ("Q", "VAL", DbFieldType::Long),
                ("Q.DO1", "DO1", DbFieldType::Double),
                ("Q.STR1", "STR1", DbFieldType::String),
                ("Q.DO2", "DO2", DbFieldType::Double),
                ("Q.STR2", "STR2", DbFieldType::String),
                ("Q.BUSY", "BUSY", DbFieldType::Short),
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
    for (record, field, value) in STEPS {
        db.put_record_field_from_ca(record, field, EpicsValue::String(value.into()))
            .await
            .unwrap();
        epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
        // A step posts each string at most once, so the one read now is the
        // one its post carried.
        for (pv, field) in [("Q.STR1", "STR1"), ("Q.STR2", "STR2")] {
            let now = match cell.read().record.get_field(field) {
                Some(EpicsValue::String(v)) => v.to_string(),
                other => panic!("{pv}: {other:?}"),
            };
            for stream in &streams {
                let mut stream = stream.lock().unwrap();
                *stream = stream
                    .replace(&format!("{pv} @"), &format!("{pv} {now}"))
                    .replace("  ", " ");
            }
        }
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    for (got, want, class) in [
        (&got[0], VALUE, "DBE_VALUE"),
        (&got[1], ALARM, "DBE_ALARM"),
        (&got[2], LOG, "DBE_LOG"),
    ] {
        let (mut got, mut want) = (by_pv(got), by_pv(want));
        let (got_busy, want_busy) = (got.remove("Q.BUSY"), want.remove("Q.BUSY"));
        assert_eq!(got, want, "{class}");
        if class != "DBE_ALARM" {
            assert_eq!(cut_alarm(got_busy), cut_alarm(want_busy), "{class} Q.BUSY");
        }
    }
}

/// Each PV's lines in the order it received them.
fn by_pv(stream: &str) -> std::collections::BTreeMap<&str, Vec<&str>> {
    let mut m = std::collections::BTreeMap::<&str, Vec<&str>>::new();
    for line in stream.lines() {
        m.entry(line.split(' ').next().unwrap())
            .or_default()
            .push(line);
    }
    m
}

/// BUSY's values without the alarm its post carried.
fn cut_alarm(lines: Option<Vec<&str>>) -> Vec<String> {
    lines
        .unwrap_or_default()
        .iter()
        .map(|l| l.split(' ').take(2).collect::<Vec<_>>().join(" "))
        .collect()
}
