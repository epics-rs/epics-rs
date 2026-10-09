//! A runtime put to a calcout `INPn` loads nothing into the value field: C's
//! `special()` calls `recGblInitConstantLink` (`calcoutRecord.c:373-374`), but
//! `dbPutFieldLink` runs that pass before `dbAddLink` installs the new `lset`
//! (`dbAccess.c:1181-1208`), so `dbLoadLink` returns `S_db_noLSET`. The status
//! reads Constant for every put, and the value field is posted as it stands.
//!
//! The pin is softIoc R7.0.10: each case under `testdata/calcout_c_stream` is
//! one `.db`, the `steps.txt` it was driven with (one `camonitor -tn` per PV,
//! then each `caput`), and the per-PV streams softIoc served, connect events
//! included. The port must serve the same lines byte for byte.
//!
//! The sCalcout, aCalcout and transform cases below still pin the framework
//! re-seed (`special_reseed_input_links`), which their C `special()` does not
//! perform either; that decision is deferred.

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::recgbl::alarm_condition_string;
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::{DbFieldType, EpicsValue};

async fn field(db: &PvDatabase, rec: &str, f: &str) -> EpicsValue {
    db.get_record(rec)
        .unwrap_or_else(|| panic!("{rec} missing"))
        .read()
        .record
        .get_field(f)
        .unwrap_or_else(|| panic!("{rec}.{f} missing"))
}

async fn put(db: &PvDatabase, rec: &str, f: &str, v: &str) {
    db.put_record_field_from_ca(rec, f, EpicsValue::String(v.into()))
        .await
        .unwrap();
}

/// Render one monitor event as `camonitor -tn` prints it.
fn camonitor_line(pv: &str, snap: &Snapshot, enums: &[String]) -> String {
    let value = match &snap.value {
        EpicsValue::Double(v) => {
            assert!(v.fract() == 0.0, "the fixtures serve integral values");
            format!("{}", *v as i64)
        }
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

/// Drive `testdata/calcout_c_stream/<case>` through the port and compare the
/// per-PV streams with what softIoc served.
async fn softioc_pin(case: &str) {
    let dir = format!(
        "{}/tests/testdata/calcout_c_stream/{case}",
        env!("CARGO_MANIFEST_DIR")
    );
    let read = |f: &str| std::fs::read_to_string(format!("{dir}/{f}")).unwrap();
    let (db, _) = IocBuilder::new()
        .db_string(&read("t.db"), &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let steps = read("steps.txt");
    let mut lines = steps.lines();
    let monitor = lines.next().unwrap().strip_prefix("monitor ").unwrap();
    let mut subs = Vec::new();
    for (sid, pv) in monitor.split_whitespace().enumerate() {
        let (rec, field) = pv.split_once('.').unwrap();
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
                (EventMask::VALUE | EventMask::ALARM).bits(),
            )
            .unwrap();
        subs.push((pv, vec![camonitor_line(pv, &connect, &enums)], rx, enums));
    }
    for step in lines {
        let mut words = step.split_whitespace();
        assert_eq!(words.next(), Some("caput"));
        let (rec, field) = words.next().unwrap().split_once('.').unwrap();
        let value = match words.next().unwrap() {
            "\"\"" => "",
            v => v,
        };
        db.put_record_field_from_ca(rec, field, EpicsValue::String(value.into()))
            .await
            .unwrap();
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

/// `caput CO.INPB 7`: B keeps 2 and is posted, INBV posts Constant, and the
/// next process computes with B = 2.
#[epics_macros_rs::epics_test]
async fn calcout_put_of_a_constant_inp_posts_b_unloaded() {
    softioc_pin("inp_constant_put").await;
}

/// `caput CO.INPB ""` then `caput CO.INPB 9`: each put posts B and INBV as
/// they stand.
#[epics_macros_rs::epics_test]
async fn calcout_put_of_an_empty_inp_posts_b_and_inbv() {
    softioc_pin("inp_empty_put").await;
}

/// `caput CO.INPB SRC`: INBV reads Constant although SRC is local, and the
/// next process reads SRC into B.
#[epics_macros_rs::epics_test]
async fn calcout_put_of_a_pv_link_reads_constant_until_the_link_reads() {
    softioc_pin("inp_pv_link_put").await;
}

/// calc's `special()` has no INP arm (`calcRecord.c:139-157`): the put posts
/// nothing on B.
#[epics_macros_rs::epics_test]
async fn calc_put_of_a_constant_inp_posts_nothing() {
    softioc_pin("calc_inp_put").await;
}

/// sCalcout: the NUMERIC input re-seeds (C `sCalcoutRecord.c:514-516`), the
/// STRING input does not — C guards the load with
/// `if (fieldIndex <= scalcoutRecordINPL)`, and `pvalue = &pcalc->a + lnkIndex`
/// is a `double *` that cannot address AA at all.
#[epics_macros_rs::epics_test]
async fn scalcout_reseeds_numeric_inputs_only() {
    let db = PvDatabase::new();
    let mut sc = ScalcoutRecord::new();
    sc.put_field("CALC", EpicsValue::String("A+B".into()))
        .unwrap();
    sc.put_field("INPB", EpicsValue::String("2".into()))
        .unwrap();
    sc.put_field("INAA", EpicsValue::String("xx".into()))
        .unwrap();
    db.add_record("SC", Box::new(sc)).await.unwrap();

    put(&db, "SC", "INPB", "7").await;
    put(&db, "SC", "INAA", "yy").await;

    assert_eq!(field(&db, "SC", "B").await, EpicsValue::Double(7.0));
    assert_eq!(
        field(&db, "SC", "AA").await,
        EpicsValue::String("".into()),
        "C's fieldIndex <= INPL guard excludes the string inputs from the re-seed"
    );
}

/// aCalcout: same guard (`aCalcoutRecord.c:534`) — the numeric input re-seeds,
/// the ARRAY input does not.
#[epics_macros_rs::epics_test]
async fn acalcout_reseeds_numeric_inputs_only() {
    let db = PvDatabase::new();
    let mut ac = AcalcoutRecord::new();
    ac.put_field("NELM", EpicsValue::ULong(4)).unwrap();
    ac.put_field("CALC", EpicsValue::String("A+B".into()))
        .unwrap();
    ac.put_field("INPB", EpicsValue::String("2".into()))
        .unwrap();
    db.add_record("AC", Box::new(ac)).await.unwrap();

    put(&db, "AC", "INPB", "7").await;
    put(&db, "AC", "INAA", "3").await;

    assert_eq!(field(&db, "AC", "B").await, EpicsValue::Double(7.0));
    assert_eq!(
        field(&db, "AC", "AA").await,
        EpicsValue::DoubleArray(vec![0.0; 4].into()),
        "C's fieldIndex <= INPL guard excludes the array inputs from the re-seed"
    );
}

/// transform: the FOURTH record whose C `special()` re-seeds
/// (`transformRecord.c:715-723`) — the same defect family, found by searching
/// the anchor (`recGblInitConstantLink` inside a `special()` body) across every
/// C record in base and synApps calc. Its post carries `DBE_VALUE | DBE_LOG`
/// (`:719`), unlike the calcout family's bare `DBE_VALUE`.
#[epics_macros_rs::epics_test]
async fn transform_put_to_a_constant_inp_reseeds_and_posts_value_log() {
    use epics_base_rs::server::records::transform::TransformRecord;

    let db = PvDatabase::new();
    let mut t = TransformRecord::default();
    t.put_field("INPB", EpicsValue::String("2".into())).unwrap();
    db.add_record("T", Box::new(t)).await.unwrap();
    assert_eq!(field(&db, "T", "B").await, EpicsValue::Double(2.0));

    let inst = db.get_record("T").unwrap();
    let mut b_rx = inst
        .write()
        .add_subscriber(
            "B",
            1,
            DbFieldType::Double,
            (EventMask::VALUE | EventMask::LOG).bits(),
        )
        .expect("a B subscription must be accepted");

    put(&db, "T", "INPB", "7").await;

    assert_eq!(field(&db, "T", "B").await, EpicsValue::Double(7.0));
    let event = b_rx.try_recv().expect("the re-seed must post B");
    assert_eq!(event.snapshot.value, EpicsValue::Double(7.0));
}
