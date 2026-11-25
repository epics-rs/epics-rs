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
//! C posts BUSY 0 after `asyncFinish`'s `recGblResetAlarms` with its
//! `MonitorMask` (`:471`, `:505`), so the first cycle's carries no alarm and
//! reaches `DBE_ALARM`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::{MENU_ALARM_SEVR, ProcessCompletion};
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
        completed(
            db.put_record_field_from_ca(record, field, EpicsValue::String(value.into()))
                .await
                .unwrap(),
        )
        .await;
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
        assert_eq!(by_pv(got), by_pv(want), "{class}");
    }
}

/// `asyncFinish`'s abort completion: `abort`, `aborting` and `busy` post with
/// `MonitorMask` (`sseqRecord.c:471`, `:481-482`, `:505`), so the first
/// cycle's clears reach `DBE_ALARM` and none reaches `DBE_LOG`; an idle abort
/// posts `busy` alone (`:1173-1177`). oracleStdIoc, `caput Q.PROC 1; Q.ABORT 1`
/// inside `DLY1`, then `caput Q.PROC 1` and, idle, `caput Q.ABORT 1`.
#[epics_macros_rs::epics_test]
async fn sseq_abort_posts_as_the_oracle_does() {
    const VALUE: &str = "\
Q.BUSY 1 UDF INVALID
Q.ABORTING 1 UDF INVALID
Q.ABORT 1 UDF INVALID
Q 0
Q.ABORT 0
Q.ABORTING 0
Q.BUSY 0
Q.BUSY 1
Q 0
Q.BUSY 0
Q.BUSY 0
";
    const ALARM: &str = "\
Q 0
Q.ABORT 0
Q.ABORTING 0
Q.BUSY 0
";
    const LOG: &str = "\
Q.ABORT 1 UDF INVALID
";
    let (db, _) = IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .db_string(
            r#"
record(ao, "Y") {}
record(sseq, "Q") {
  field(DLY1, "0.5")
  field(DOL1, "5")
  field(LNK1, "Y PP")
}
"#,
            &HashMap::new(),
        )
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
                ("Q.ABORT", "ABORT", DbFieldType::Short),
                ("Q.ABORTING", "ABORTING", DbFieldType::Short),
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
    // Plain `caput`, C `dbPutField`: a put-notify would wait out the
    // sequence's PACT (`dbNotify.c` restart queue) and reach an idle record.
    // The first ABORT lands 0.2 s into `DLY1`.
    for (field, inside_dly1) in [
        ("PROC", true),
        ("ABORT", false),
        ("PROC", false),
        ("ABORT", false),
    ] {
        // The idle ABORT is refused, as C's `return(-1)` is.
        let _ = db
            .put_record_field_from_ca_no_notify("Q", field, EpicsValue::String("1".into()))
            .await;
        if inside_dly1 {
            epics_base_rs::runtime::task::sleep(Duration::from_millis(200)).await;
        } else {
            idle(&db, "Q").await;
        }
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    for (got, want, class) in [
        (&got[0], VALUE, "DBE_VALUE"),
        (&got[1], ALARM, "DBE_ALARM"),
        (&got[2], LOG, "DBE_LOG"),
    ] {
        assert_eq!(by_pv(got), by_pv(want), "{class}");
    }
}

/// Until the put's cycle completes: sseq runs its links from a callback
/// (`sseqRecord.c:443-448`), after the put returns.
async fn completed(put: ProcessCompletion) {
    if let Some(rx) = put.into_handle() {
        let _ = epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, rx)
            .await
            .expect("the put's cycle never completed");
    }
}

/// Until `rec` leaves PACT.
async fn idle(db: &PvDatabase, rec: &str) {
    let cell = db.get_record(rec).unwrap();
    epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, async {
        while cell.read().is_processing() {
            epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{rec} never left PACT"));
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

/// `asyncFinish` clears `busy` after `recGblFwdLink` (`sseqRecord.c:499-504`),
/// so the forward-link target still reads `BUSY == 1`.
#[epics_macros_rs::epics_test]
async fn the_forward_link_target_reads_busy_still_set() {
    let (db, _) = IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .db_string(
            r#"
record(sseq, "Q") { field(FLNK, "C") }
record(calc, "C") { field(INPA, "Q.BUSY NPP") field(CALC, "A") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db.put_record_field_from_ca("Q", "PROC", EpicsValue::String("1".into()))
        .await
        .unwrap();
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    let read = |rec: &str, field: &str| db.get_record(rec).unwrap().read().record.get_field(field);
    assert_eq!(read("C", "VAL"), Some(EpicsValue::Double(1.0)));
    assert_eq!(read("Q", "BUSY"), Some(EpicsValue::Short(0)));
}
