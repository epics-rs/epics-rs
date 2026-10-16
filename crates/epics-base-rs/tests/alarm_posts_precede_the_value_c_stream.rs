//! The order a record's posts reach the event queue, against one softIoc
//! R7.0.10 `camonitor -tn` client watching every field (`dbEvent.c` drains
//! that client's ring in post order). C's `monitor()` calls
//! `recGblResetAlarms` (recGbl.c:178-224: SEVR, STAT, AMSG, then ACKS when
//! the acknowledge rule fires) before its own VAL post, and `dbProcess`'s
//! MAX_LOCK refusal (dbAccess.c:549-555) does the same before posting VAL.
//!
//! The port delivers each subscription separately, so the order is read where
//! it is decided: a filter on every subscriber records each event as it is
//! posted. `testdata/alarm_post_order/<case>` holds the `.db`, the
//! `steps.txt` it was driven with and the stream softIoc served.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::recgbl::alarm_condition_string;
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::EpicsValue;

/// Render one monitor event as `camonitor -tn` prints it.
fn camonitor_line(pv: &str, snap: &Snapshot, enums: &[String]) -> String {
    camonitor_parts(
        pv,
        &snap.value,
        snap.alarm.status,
        snap.alarm.severity,
        enums,
    )
}

fn camonitor_parts(pv: &str, value: &EpicsValue, stat: u16, sevr: u16, enums: &[String]) -> String {
    let value = match value {
        EpicsValue::Double(v) if v.fract() == 0.0 => format!("{}", *v as i64),
        EpicsValue::Double(v) => format!("{v}"),
        EpicsValue::UChar(v) => format!("{v}"),
        EpicsValue::Enum(i) => enums[*i as usize].clone(),
        EpicsValue::String(s) => s.to_string(),
        other => panic!("{pv}: no camonitor rendering for {other:?}"),
    };
    let stat = match stat {
        0 => "",
        s => alarm_condition_string(s),
    };
    let sevr = match sevr {
        0 => "",
        s => MENU_ALARM_SEVR[s as usize],
    };
    format!("{pv:<31} {value} {stat} {sevr}")
}

/// Appends each event it passes to the one stream every subscriber shares.
struct Record {
    pv: String,
    enums: Vec<String>,
    stream: Arc<Mutex<Vec<String>>>,
}

impl SubscriptionFilter for Record {
    fn name(&self) -> &'static str {
        "record"
    }

    fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
        let e = &event.event;
        // AMSG's log is a reference (a 40-byte string fails `useValque`);
        // the field it would read is the amsg the log copied.
        let amsg;
        let value = match e.value().as_value() {
            Some(v) => v.clone(),
            None if self.pv.ends_with(".AMSG") => {
                amsg = EpicsValue::String(e.amsg().into());
                amsg
            }
            None => panic!("{}: a reference log", self.pv),
        };
        self.stream.lock().unwrap().push(camonitor_parts(
            &self.pv,
            &value,
            e.stat(),
            e.sevr(),
            &self.enums,
        ));
        Some(event)
    }
}

/// How a case reaches the stream softIoc served: every step of `steps.txt`,
/// or its first `steps` steps and then `record` refused in place through
/// `RecordInstance::process_local` while PACT is held, as the FLNK steps
/// refuse it in softIoc.
enum Drive {
    Steps,
    LocalRefusal { steps: usize, record: &'static str },
}

async fn softioc_pin(case: &str, drive: Drive) {
    let dir = format!(
        "{}/tests/testdata/alarm_post_order/{case}",
        env!("CARGO_MANIFEST_DIR")
    );
    let read = |f: &str| std::fs::read_to_string(format!("{dir}/{f}")).unwrap();
    let (db, _) = IocBuilder::new()
        .db_string(&read("t.db"), &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let steps = read("steps.txt");
    let mut lines = steps.lines();
    let monitor = lines.next().unwrap().strip_prefix("monitor ").unwrap();
    let stream = Arc::new(Mutex::new(Vec::new()));
    let mut readers = Vec::new();
    for (sid, pv) in monitor.split_whitespace().enumerate() {
        let (rec, field) = pv.split_once('.').unwrap_or((pv, "VAL"));
        let cell = db.get_record(rec).unwrap();
        let connect = db.channel_snapshot_for_field(&cell, field, false).unwrap();
        let enums: Vec<String> = connect
            .enums
            .as_ref()
            .map(|e| e.strings.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default();
        stream
            .lock()
            .unwrap()
            .push(camonitor_line(pv, &connect, &enums));
        let mut inst = cell.write();
        readers.push(
            inst.add_subscriber(
                field,
                sid as u32,
                connect.value.db_field_type(),
                (EventMask::VALUE | EventMask::ALARM).bits(),
            )
            .unwrap(),
        );
        assert!(inst.attach_filter_to_last_subscriber(
            field,
            Arc::new(Record {
                pv: pv.to_string(),
                enums,
                stream: stream.clone(),
            }),
        ));
    }
    let steps = match drive {
        Drive::Steps => usize::MAX,
        Drive::LocalRefusal { steps, .. } => steps,
    };
    for step in lines.take(steps) {
        let mut words = step.split_whitespace();
        if let Some(secs) = step.strip_prefix("sleep ") {
            epics_base_rs::runtime::task::sleep(std::time::Duration::from_secs_f64(
                secs.parse().unwrap(),
            ))
            .await;
            continue;
        }
        assert_eq!(words.next(), Some("caput"));
        let pv = words.next().unwrap();
        let (rec, field) = pv.split_once('.').unwrap_or((pv, "VAL"));
        let value = words.next().unwrap();
        db.put_record_field_from_ca(rec, field, EpicsValue::String(value.into()))
            .await
            .unwrap();
        // Outlasts the 0.1 s ODLY the refusal case's first cycle runs.
        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(250)).await;
    }
    if let Drive::LocalRefusal { record, .. } = drive {
        let cell = db.get_record(record).unwrap();
        let mut inst = cell.write();
        inst.enter_pact();
        // C `dbProcess` refuses MAX_LOCK = 10 entries silently and raises on
        // the eleventh.
        for _ in 0..10 {
            assert!(inst.process_local().unwrap().0.is_empty());
        }
        let (posts, alarm_posts) = inst.process_local().unwrap();
        assert!(alarm_posts.is_empty());
        inst.notify_from_snapshot(&posts);
    }
    let mut got = stream.lock().unwrap().join("\n");
    got.push('\n');
    assert_eq!(got, read("softioc.out"), "{case}");
}

/// X leaves UDF (ACKS follows NO_ALARM), rises to HIHI (ACKS rises to MAJOR),
/// falls to HIGH (ACKT holds ACKS at MAJOR, so no ACKS post) and clears.
#[epics_macros_rs::epics_test]
async fn alarm_fields_post_ahead_of_val_in_monitor_order() {
    softioc_pin("raise", Drive::Steps).await;
}

/// T is held active by a 60 s ODLY while SRC's FLNK reaches it eleven times;
/// the eleventh refusal raises SCAN_ALARM and posts SEVR, STAT, AMSG, ACKS and
/// then VAL.
#[epics_macros_rs::epics_test]
async fn a_max_lock_refusal_posts_its_alarm_fields_ahead_of_val() {
    softioc_pin("refusal", Drive::Steps).await;
}

/// A watched LCNT: the refusals count it to 11, and the cycle that finishes
/// clears it, but C posts it from neither — no `monitor()` posts a `dbCommon`
/// field. The finishing cycle drops SCAN_ALARM as any cycle drops an alarm,
/// and a second round of eleven refusals raises it again, INVALID as before.
#[epics_macros_rs::epics_test]
async fn a_refused_record_never_posts_its_lock_count() {
    softioc_pin("relock", Drive::Steps).await;
}

/// The same refusal reached through `process_local`: it posts what softIoc
/// served, in the same order, ACKS included.
#[epics_macros_rs::epics_test]
async fn a_process_local_refusal_posts_as_the_scan_refusal_does() {
    softioc_pin(
        "refusal",
        Drive::LocalRefusal {
            steps: 1,
            record: "T",
        },
    )
    .await;
}
