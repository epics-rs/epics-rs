//! No C `monitor()` calls `db_post_events` on a change-detection tracker:
//! `oraw`/`orbv` (bi, bo, mbbi, mbbo, mbbiDirect, mbboDirect, ao) are the
//! previous RVAL/RBV the RVAL/RBV post compares against, and `mlst`/`alst`/
//! `lalm` are the previous VAL the deadband compares against. The generic
//! change-detection loop must therefore leave them out on every record type,
//! while a `caput` to one still posts through the put path.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::db_loader::create_record;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

fn subscribe(db: &PvDatabase, field: &str, sid: u32) -> Option<EventReader> {
    let inst = db.get_record("R").unwrap();
    let mut g = inst.write();
    g.add_subscriber(
        field,
        sid,
        DbFieldType::Double,
        (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits(),
    )
    .ok()
}

fn read(db: &PvDatabase, field: &str) -> Option<EpicsValue> {
    db.get_record("R").unwrap().read().record.get_field(field)
}

/// Processes `rtype` once with VAL/RVAL moved off their defaults and returns
/// every event a subscriber to one of `trackers` received. `fields` are put
/// first. Every tracker must move, or the case proves nothing.
async fn tracker_posts(rtype: &str, trackers: &[&str], fields: &[(&str, &str)]) -> Vec<String> {
    let db = PvDatabase::new();
    db.add_record("R", create_record(rtype).unwrap())
        .await
        .unwrap();
    let before: Vec<_> = trackers.iter().map(|f| read(&db, f)).collect();
    let mut rxs: Vec<(String, EventReader)> = trackers
        .iter()
        .zip(1u32..)
        .filter_map(|(f, sid)| subscribe(&db, f, sid).map(|rx| (f.to_string(), rx)))
        .collect();
    assert!(!rxs.is_empty(), "{rtype} has none of {trackers:?}");
    for (field, value) in fields {
        db.put_record_field_from_ca("R", field, EpicsValue::String((*value).into()))
            .await
            .unwrap();
    }
    {
        let inst = db.get_record("R").unwrap();
        let mut g = inst.write();
        let _ = g.record.put_field("RVAL", EpicsValue::Long(5));
        let _ = g.record.put_field("RBV", EpicsValue::Long(7));
        let _ = g.record.put_field("MDEL", EpicsValue::Double(-1.0));
        let _ = g.record.put_field("ADEL", EpicsValue::Double(-1.0));
    }
    let _ = db.put_pv("R", EpicsValue::Double(3.0)).await;
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    let _ = db.process_record_with_links("R", &mut visited).await;
    for (field, was) in trackers.iter().zip(before) {
        assert_ne!(read(&db, field), was, "{rtype}.{field} never moved");
    }
    let mut got = Vec::new();
    for (field, rx) in &mut rxs {
        while rx.try_recv().is_ok() {
            got.push(format!("{rtype}.{field}"));
        }
    }
    got
}

macro_rules! tracker_test {
    ($name:ident, $rtype:expr, $trackers:expr) => {
        tracker_test!($name, $rtype, $trackers, &[]);
    };
    ($name:ident, $rtype:expr, $trackers:expr, $fields:expr) => {
        #[epics_macros_rs::epics_test]
        async fn $name() {
            let posted = tracker_posts($rtype, $trackers, $fields).await;
            assert!(posted.is_empty(), "tracker posts: {posted:?}");
        }
    };
}

tracker_test!(bi_oraw_is_not_posted, "bi", &["ORAW"]);
// ao/bo RBV is written by device support alone, so ORBV cannot move here;
// mbbo and mbboDirect carry the ORBV case.
tracker_test!(bo_oraw_is_not_posted, "bo", &["ORAW"]);
tracker_test!(mbbi_oraw_is_not_posted, "mbbi", &["ORAW"]);
tracker_test!(mbbo_oraw_and_orbv_are_not_posted, "mbbo", &["ORAW", "ORBV"]);
tracker_test!(mbbi_direct_oraw_is_not_posted, "mbbiDirect", &["ORAW"]);
tracker_test!(
    mbbo_direct_oraw_orbv_obit_are_not_posted,
    "mbboDirect",
    &["ORAW", "ORBV", "OBIT"]
);
tracker_test!(
    ai_mlst_alst_lalm_are_not_posted,
    "ai",
    &["ORAW", "MLST", "ALST", "LALM"]
);
tracker_test!(
    ao_mlst_alst_lalm_are_not_posted,
    "ao",
    &["ORAW", "MLST", "ALST", "LALM"]
);
tracker_test!(
    calc_mlst_alst_lalm_are_not_posted,
    "calc",
    &["MLST", "ALST", "LALM"],
    &[("CALC", "7")]
);
tracker_test!(
    dfanout_mlst_alst_lalm_are_not_posted,
    "dfanout",
    &["MLST", "ALST", "LALM"]
);
