//! `mbboDirectRecord.c:307-335` posts a bit only when `obit ^ val` has it
//! or the cycle carries an alarm event; `special` moves OBIT with VAL so a
//! `caput X.B0 1` posts B0 once (from `dbPut`), not again from `monitor()`.
//! softIoc, `camonitor -m va` and `-m v` on VAL, B0-B4, RVAL, OBIT of each
//! record: the bits carry DBE_VALUE on an alarm-only cycle, VAL does not.

use std::collections::HashMap;

use epics_base_rs::server::database::ProcStack;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const FIELDS: [&str; 8] = ["VAL", "B0", "B1", "B2", "B3", "B4", "RVAL", "OBIT"];

enum Step {
    Process(&'static str),
    Put(&'static str, &'static str, EpicsValue),
}

#[epics_macros_rs::epics_test]
async fn a_bit_put_posts_that_bit_once_and_alarms_post_every_bit() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(mbboDirect, "X") { field(VAL, "6") }
record(ai, "SRC") { field(VAL, "3") field(HHSV, "MAJOR") field(HIHI, "10") }
record(mbboDirect, "Y") { field(OMSL, "closed_loop") field(DOL, "SRC NPP MS") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let masks = [EventMask::VALUE | EventMask::ALARM, EventMask::VALUE];
    // Indexed by record (X, Y), then mask, then FIELDS.
    let mut subs = Vec::new();
    let mut id = 0;
    for rec in ["X", "Y"] {
        let r = db.get_record(rec).unwrap();
        let mut g = r.write();
        for mask in masks {
            for f in FIELDS {
                id += 1;
                subs.push(
                    g.add_subscriber(f, id, DbFieldType::Long, mask.bits())
                        .unwrap(),
                );
            }
        }
    }
    // Events per FIELDS entry of the stepped record, `-m va` then `-m v`,
    // from the softIoc run.
    let steps = [
        (
            Step::Process("X"),
            [[1, 1, 1, 1, 1, 1, 1, 0], [0, 1, 1, 1, 1, 1, 1, 0]],
        ),
        (
            Step::Put("X", "B0", EpicsValue::Long(1)),
            [[1, 1, 0, 0, 0, 0, 1, 0]; 2],
        ),
        (Step::Process("X"), [[0; 8]; 2]),
        (Step::Process("Y"), [[1, 1, 1, 1, 1, 1, 1, 0]; 2]),
        (
            Step::Put("SRC", "HIHI", EpicsValue::Double(2.0)),
            [[0; 8]; 2],
        ),
        (
            Step::Process("Y"),
            [[1, 1, 1, 1, 1, 1, 0, 0], [0, 1, 1, 1, 1, 1, 0, 0]],
        ),
        (Step::Process("Y"), [[0; 8]; 2]),
    ];
    let per_record = masks.len() * FIELDS.len();
    for (n, (step, want)) in steps.into_iter().enumerate() {
        let rec = match step {
            Step::Process(rec) => {
                db.process_record_with_links(rec, &mut ProcStack::new())
                    .await
                    .unwrap();
                rec
            }
            Step::Put(rec, field, v) => {
                db.put_record_field_from_ca(rec, field, v).await.unwrap();
                if rec == "SRC" { "Y" } else { rec }
            }
        };
        let base = if rec == "X" { 0 } else { per_record };
        let mut got = [[0; 8]; 2];
        for (rx, g) in subs[base..base + per_record]
            .iter_mut()
            .zip(got.iter_mut().flatten())
        {
            while rx.try_recv().is_ok() {
                *g += 1;
            }
        }
        assert_eq!(got, want, "step {n} on {rec}, fields {FIELDS:?}");
    }
    let obit = db.get_record("X").unwrap().read().record.get_field("OBIT");
    assert_eq!(obit, Some(EpicsValue::Long(7)));
}

/// qa's case: the first `caput M.B0 1` on an undefined record clears UDF, so
/// `monitor()` posts every bit with the alarm events and B0 twice. softIoc,
/// `-m va` and `-m v` alike.
#[epics_macros_rs::epics_test]
async fn the_first_bit_put_posts_every_bit_on_the_udf_alarm_change() {
    let (db, _) = IocBuilder::new()
        .db_string(r#"record(mbboDirect, "M") {}"#, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let fields = ["VAL", "B0", "B1", "B2", "B3", "RVAL"];
    let mut subs = Vec::new();
    {
        let r = db.get_record("M").unwrap();
        let mut g = r.write();
        let mut id = 0;
        for mask in [EventMask::VALUE | EventMask::ALARM, EventMask::VALUE] {
            for f in fields {
                id += 1;
                subs.push(
                    g.add_subscriber(f, id, DbFieldType::Long, mask.bits())
                        .unwrap(),
                );
            }
        }
    }
    for (field, want) in [
        ("B0", [1, 2, 1, 1, 1, 1]),
        ("B1", [1, 0, 1, 0, 0, 1]),
        ("PROC", [0; 6]),
    ] {
        db.put_record_field_from_ca("M", field, EpicsValue::Long(1))
            .await
            .unwrap();
        let mut got = [0; 12];
        for (rx, g) in subs.iter_mut().zip(&mut got) {
            while rx.try_recv().is_ok() {
                *g += 1;
            }
        }
        assert_eq!(got[..6], want, "M.{field} -m va");
        assert_eq!(got[6..], want, "M.{field} -m v");
    }
}
