//! `mbbiDirectRecord.c:209-239` derives each bit in `monitor()` and posts it
//! with `events|DBE_VALUE|DBE_LOG` when it differs from the bit the record
//! held, else with `events` alone. softIoc, `camonitor -m va` and `-m v` on
//! VAL, B0-B3, RVAL: a `caput I.B0 1` on a cleared bit posts B0 twice, once
//! from `dbPut` and once when `monitor()` clears it again.

use std::collections::HashMap;

use epics_base_rs::server::database::ProcStack;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const FIELDS: [&str; 6] = ["VAL", "B0", "B1", "B2", "B3", "RVAL"];

enum Step {
    Process,
    Put(&'static str, &'static str, EpicsValue),
}

#[epics_macros_rs::epics_test]
async fn bits_post_on_their_own_change_and_on_an_alarm_change() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(longin, "L") { field(VAL, "5") field(HIHI, "100") field(HHSV, "MAJOR") }
record(mbbiDirect, "I") { field(DTYP, "Soft Channel") field(INP, "L NPP MS") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let masks = [EventMask::VALUE | EventMask::ALARM, EventMask::VALUE];
    let mut subs = Vec::new();
    {
        let r = db.get_record("I").unwrap();
        let mut g = r.write();
        let mut id = 0;
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
    // Events per FIELDS entry, `-m va` then `-m v`, from the softIoc run.
    let steps = [
        (Step::Process, [[1, 1, 1, 1, 1, 0], [1, 1, 0, 1, 0, 0]]),
        (
            Step::Put("I", "B0", EpicsValue::Long(1)),
            [[0, 1, 0, 0, 0, 0]; 2],
        ),
        (Step::Put("L", "VAL", EpicsValue::Long(6)), [[0; 6]; 2]),
        (Step::Process, [[1, 1, 1, 0, 0, 0]; 2]),
        (
            Step::Put("I", "B0", EpicsValue::Long(1)),
            [[0, 2, 0, 0, 0, 0]; 2],
        ),
        (Step::Process, [[0; 6]; 2]),
        (Step::Put("L", "HIHI", EpicsValue::Long(2)), [[0; 6]; 2]),
        (Step::Process, [[1, 1, 1, 1, 1, 0], [0; 6]]),
        (Step::Process, [[0; 6]; 2]),
    ];
    for (n, (step, want)) in steps.into_iter().enumerate() {
        match step {
            Step::Process => {
                db.process_record_with_links("I", &mut ProcStack::new())
                    .await
                    .unwrap();
            }
            Step::Put(rec, field, v) => {
                db.put_record_field_from_ca(rec, field, v).await.unwrap();
            }
        }
        let mut got = [[0; 6]; 2];
        for (rx, g) in subs.iter_mut().zip(got.iter_mut().flatten()) {
            while rx.try_recv().is_ok() {
                *g += 1;
            }
        }
        assert_eq!(got, want, "step {n}, fields {FIELDS:?}");
    }
}
