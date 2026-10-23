//! Spec §5 test 3: a scalar OUT put into a `SPC_DBADDR` waveform VAL is
//! C `dbPut`'s array arm: one element, NORD set to 1 and posted by
//! `put_array_info` when it changed, VAL (`pp(TRUE)`) not posted by the put.
//! Pinned to oracleStdIoc (C ground truth §5.3, its DB verbatim).

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(waveform,"WF"){field(FTVL,"DOUBLE") field(NELM,"5")}
record(waveform,"WFP"){field(FTVL,"DOUBLE") field(NELM,"5")}
record(waveform,"WFL"){field(FTVL,"LONG") field(NELM,"5")}
record(ao,"AO"){field(OUT,"WF.VAL NPP")}
record(ao,"AOP"){field(OUT,"WFP.VAL PP")}
record(longout,"LO"){field(OUT,"WFL.VAL NPP")}
"#;

const UDF_ALARM: u16 = 17;
const INVALID: u16 = 3;

struct Subs {
    nord: EventReader,
    val: EventReader,
}

fn subscribe(db: &PvDatabase, rec: &str) -> Subs {
    let r = db.get_record(rec).unwrap();
    let mut g = r.write();
    let mask = (EventMask::VALUE | EventMask::ALARM).bits();
    let mut nord = g
        .add_subscriber("NORD", 1, DbFieldType::Long, mask)
        .unwrap();
    let mut val = g
        .add_subscriber("VAL", 2, DbFieldType::Double, mask)
        .unwrap();
    drop(g);
    while nord.try_recv().is_ok() {}
    while val.try_recv().is_ok() {}
    Subs { nord, val }
}

impl Subs {
    /// NORD's posts as `(nord, stat, sevr)`, and how many VAL posts.
    fn drain(&mut self) -> (Vec<(f64, u16, u16)>, usize) {
        let mut nord = Vec::new();
        while let Ok(ev) = self.nord.try_recv() {
            let v = ev.value().as_value().and_then(EpicsValue::to_f64).unwrap();
            nord.push((v, ev.stat(), ev.sevr()));
        }
        let mut val = 0;
        while self.val.try_recv().is_ok() {
            val += 1;
        }
        (nord, val)
    }
}

fn first(db: &PvDatabase, pv: &str) -> (f64, f64) {
    let v = match db.get_pv(pv).unwrap() {
        EpicsValue::DoubleArray(a) => a[0],
        EpicsValue::LongArray(a) => a[0].into(),
        other => panic!("{pv} = {other:?}"),
    };
    let nord = db.get_pv(format!("{pv}.NORD")).unwrap().to_f64().unwrap();
    (v, nord)
}

#[epics_macros_rs::epics_test]
async fn a_scalar_into_a_waveform_sets_and_posts_nord() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut wf = subscribe(&db, "WF");
    let mut wfp = subscribe(&db, "WFP");
    let mut wfl = subscribe(&db, "WFL");

    db.put_record_field_from_ca(
        "WF",
        "VAL",
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0].into()),
    )
    .await
    .unwrap();
    assert_eq!(wf.drain(), (vec![(3.0, UDF_ALARM, INVALID)], 1), "step 1");

    db.put_record_field_from_ca("AO", "VAL", EpicsValue::Double(2.5))
        .await
        .unwrap();
    assert_eq!(wf.drain(), (vec![(1.0, 0, 0)], 0), "step 2");
    assert_eq!(first(&db, "WF"), (2.5, 1.0));
    assert_eq!(db.get_pv("WF.UDF").unwrap().to_f64(), Some(0.0));

    // NORD unchanged: `put_array_info` posts nothing.
    db.put_record_field_from_ca("AO", "VAL", EpicsValue::Double(2.5))
        .await
        .unwrap();
    assert_eq!(wf.drain(), (vec![], 0), "step 4");

    // PP: NORD from the put, then VAL from the target's process.
    db.put_record_field_from_ca("AOP", "VAL", EpicsValue::Double(7.5))
        .await
        .unwrap();
    assert_eq!(wfp.drain(), (vec![(1.0, UDF_ALARM, INVALID)], 1), "step 5");
    assert_eq!(first(&db, "WFP"), (7.5, 1.0));

    db.put_record_field_from_ca("LO", "VAL", EpicsValue::Long(9))
        .await
        .unwrap();
    assert_eq!(wfl.drain(), (vec![(1.0, UDF_ALARM, INVALID)], 0), "step 7");
    assert_eq!(first(&db, "WFL"), (9.0, 1.0));
}
