//! `aCalcoutRecord.c:1024-1029` posts A..L with `monitor_mask|DBE_VALUE|DBE_LOG`
//! when the input moved or the cycle carries DBE_ALARM, so a DBE_VALUE monitor
//! sees an unchanged input on every alarm change. C IOC with libcalc,
//! `camonitor -m va` and `-m v` on A, B, C, VAL of
//! `record(acalcout,"AC"){ field(CALC,"A+B") field(A,"1") field(HIGH,"5") field(HSV,"MINOR") }`.

use std::collections::HashMap;

use epics_base_rs::server::database::ProcStack;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const FIELDS: [&str; 4] = ["A", "B", "C", "VAL"];

#[epics_macros_rs::epics_test]
async fn an_alarm_change_posts_every_input_with_value() {
    let (db, _) = IocBuilder::new()
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::default()))
        .db_string(
            r#"record(acalcout, "AC") { field(CALC, "A+B") field(A, "1") field(HIGH, "5") field(HSV, "MINOR") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut subs = Vec::new();
    {
        let r = db.get_record("AC").unwrap();
        let mut g = r.write();
        let mut id = 0;
        for mask in [EventMask::VALUE | EventMask::ALARM, EventMask::VALUE] {
            for f in FIELDS {
                id += 1;
                subs.push(
                    g.add_subscriber(f, id, DbFieldType::Double, mask.bits())
                        .unwrap(),
                );
            }
        }
    }
    // `None` processes; both masks see the same counts.
    for (n, (put, want)) in [
        (None, [1, 1, 1, 1]),
        (Some(7.0), [1, 2, 1, 1]),
        (Some(8.0), [0, 1, 0, 1]),
        (Some(0.0), [1, 2, 1, 1]),
        (None, [0; 4]),
    ]
    .into_iter()
    .enumerate()
    {
        match put {
            Some(b) => {
                db.put_record_field_from_ca("AC", "B", EpicsValue::Double(b))
                    .await
                    .unwrap();
            }
            None => {
                db.process_record_with_links("AC", &mut ProcStack::new())
                    .await
                    .unwrap();
            }
        }
        let mut got = [0; 8];
        for (rx, g) in subs.iter_mut().zip(&mut got) {
            while rx.try_recv().is_ok() {
                *g += 1;
            }
        }
        assert_eq!(got[..4], want, "step {n} -m va {FIELDS:?}");
        assert_eq!(got[4..], want, "step {n} -m v {FIELDS:?}");
    }
}
