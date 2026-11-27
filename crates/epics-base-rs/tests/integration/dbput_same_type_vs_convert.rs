//! Spec §5 test 1: an NPP OUT link into calc.VAL stores the same value
//! whether the source type is the target's (ao, the lean `F64` arm) or needs
//! a conversion (longout, stringout: the general arm). Pinned to
//! oracleStdIoc (C ground truth §5.1): the put posts calc.VAL, which is not
//! `pp(TRUE)`, carrying the target's untouched `UDF`/`INVALID` alarm, clears
//! `UDF`, and leaves the source at `NO_ALARM`.

use std::collections::HashMap;

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(calc,"CA1"){field(CALC,"VAL")}
record(calc,"CA2"){field(CALC,"VAL")}
record(calc,"CA3"){field(CALC,"VAL")}
record(calc,"CA4"){field(CALC,"VAL")}
record(ao,"AO1"){field(OUT,"CA1.VAL NPP")}
record(longout,"LO1"){field(OUT,"CA2.VAL NPP")}
record(stringout,"SO1"){field(OUT,"CA3.VAL NPP")}
record(calcout,"COV"){field(CALC,"2.5") field(OUT,"CA4.VAL NPP")}
"#;

const UDF_ALARM: u16 = 17;
const INVALID: u16 = 3;

fn subscribe(db: &PvDatabase, rec: &str) -> EventReader {
    let r = db.get_record(rec).unwrap();
    let mut g = r.write();
    let rx = g
        .add_subscriber(
            "VAL",
            1,
            DbFieldType::Double,
            (EventMask::VALUE | EventMask::ALARM).bits(),
        )
        .unwrap();
    drop(g);
    rx
}

/// The events since the last drain, as `(value, stat, sevr)`.
fn drain(rx: &mut EventReader) -> Vec<(f64, u16, u16)> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        let v = ev.value().as_value().and_then(EpicsValue::to_f64).unwrap();
        out.push((v, ev.stat(), ev.sevr()));
    }
    out
}

fn f64_of(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

#[epics_macros_rs::epics_test]
async fn a_converted_link_store_reads_back_as_the_same_type_store() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut subs: Vec<_> = ["CA1", "CA2", "CA3", "CA4"]
        .into_iter()
        .map(|r| {
            let mut rx = subscribe(&db, r);
            drain(&mut rx);
            rx
        })
        .collect();

    for (n, (rec, value, target, want)) in [
        ("AO1", EpicsValue::Double(1.5), 0, 1.5),
        ("LO1", EpicsValue::Long(7), 1, 7.0),
        ("SO1", EpicsValue::String("1.5".into()), 2, 1.5),
        ("SO1", EpicsValue::String("2.25e1".into()), 2, 22.5),
    ]
    .into_iter()
    .enumerate()
    {
        db.put_record_field_from_ca(rec, "VAL", value)
            .await
            .unwrap();
        assert_eq!(
            drain(&mut subs[target]),
            [(want, UDF_ALARM, INVALID)],
            "step {n}: {rec}"
        );
        assert_eq!(f64_of(&db, &format!("CA{}", target + 1)), want);
        assert_eq!(f64_of(&db, &format!("CA{}.UDF", target + 1)), 0.0);
        assert_eq!(f64_of(&db, &format!("{rec}.SEVR")), 0.0, "{rec}");
    }
    db.process_record_with_links("COV", &mut ProcStack::new())
        .await
        .unwrap();
    assert_eq!(drain(&mut subs[3]), [(2.5, UDF_ALARM, INVALID)]);
    assert_eq!(f64_of(&db, "CA4"), 2.5);

    // The target's own cycle commits its alarm.
    db.process_record_with_links("CA1", &mut ProcStack::new())
        .await
        .unwrap();
    assert_eq!(drain(&mut subs[0]), [(1.5, 0, 0)]);
}
