//! ai `monitor()` against softIoc R7.0.10: HIGH/HIHI/LOW/LOLO transitions
//! with HYST, an MDEL crossing posting `DBE_VALUE` and an ADEL crossing
//! posting `DBE_LOG`. `testdata/ai_c_stream/<case>` holds the `.db`, the
//! `steps.txt` it was driven with (one `camonitor -tn` per PV, `PV@l` for a
//! `-m l` monitor, then each `caput`), and the streams softIoc served,
//! connect events included. The port must serve the same lines byte for byte.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::recgbl::alarm_condition_string;
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::EpicsValue;

/// Render one monitor event as `camonitor -tn` prints it.
fn camonitor_line(pv: &str, snap: &Snapshot, enums: &[String]) -> String {
    let value = match &snap.value {
        EpicsValue::Double(v) if v.fract() == 0.0 => format!("{}", *v as i64),
        EpicsValue::Double(v) => format!("{v}"),
        EpicsValue::Enum(i) => enums[*i as usize].clone(),
        other => panic!("{pv}: no camonitor rendering for {other:?}"),
    };
    let stat = match snap.alarm.status {
        0 => "",
        s => alarm_condition_string(s),
    };
    let sevr = match snap.alarm.severity {
        0 => "",
        s => MENU_ALARM_SEVR[s as usize],
    };
    format!("{pv:<31} {value} {stat} {sevr}")
}

async fn softioc_pin(case: &str) {
    let dir = format!(
        "{}/tests/testdata/ai_c_stream/{case}",
        env!("CARGO_MANIFEST_DIR")
    );
    let read = |f: &str| std::fs::read_to_string(format!("{dir}/{f}")).unwrap();
    let (db, _) = IocBuilder::new()
        .db_string(&read("t.db"), &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    // softIoc's CP link connects at iocInit and processes X once.
    db.ioc_init().await;
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    db.process_cp_links_on_connect().await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let steps = read("steps.txt");
    let mut lines = steps.lines();
    let monitor = lines.next().unwrap().strip_prefix("monitor ").unwrap();
    let mut subs = Vec::new();
    for (sid, spec) in monitor.split_whitespace().enumerate() {
        let (pv, mask) = match spec.split_once('@') {
            Some((pv, "l")) => (pv, EventMask::LOG),
            None => (spec, EventMask::VALUE | EventMask::ALARM),
            Some(other) => panic!("unknown monitor mask {other:?}"),
        };
        let (rec, field) = pv.split_once('.').unwrap_or((pv, "VAL"));
        let cell = db.get_record(rec).unwrap();
        let connect = db.channel_snapshot_for_field(&cell, field, false).unwrap();
        let enums: Vec<String> = connect
            .enums
            .as_ref()
            .map(|e| e.strings.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default();
        let rx = cell
            .write()
            .add_subscriber(
                field,
                sid as u32,
                connect.value.db_field_type(),
                mask.bits(),
            )
            .unwrap();
        subs.push((pv, vec![camonitor_line(pv, &connect, &enums)], rx, enums));
    }
    for step in lines {
        let mut words = step.split_whitespace();
        assert_eq!(words.next(), Some("caput"));
        let (rec, field) = words.next().unwrap().split_once('.').unwrap();
        let value = words.next().unwrap();
        db.put_record_field_from_ca(rec, field, EpicsValue::String(value.into()))
            .await
            .unwrap();
        // The CP target processes off the put; let it land before the next.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let mut got = String::new();
    for (pv, mut out, mut rx, enums) in subs {
        while let Ok(ev) = rx.try_recv() {
            out.push(camonitor_line(pv, &ev.snapshot, &enums));
        }
        for line in out {
            got.push_str(&line);
            got.push('\n');
        }
    }
    let want = read("softioc.out");
    assert!(!want.is_empty(), "{case}: softioc.out is empty");
    assert_eq!(got, want, "{case}");
}

/// `SRC` walks X up through HIGH and HIHI and back with HYST 1, then down
/// through LOW and LOLO; MDEL 1 and ADEL 3 pick which steps post VALUE and
/// which post LOG.
#[epics_macros_rs::epics_test]
async fn ai_alarm_ladder_and_deadbands_post_as_softioc() {
    softioc_pin("alarm_deadband").await;
}
