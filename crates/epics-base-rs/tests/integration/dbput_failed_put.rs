//! Spec §5 test 11: a refused OUT put. The target keeps what `dbPut` stored
//! before `special` refused, is not processed and posts nothing; the source
//! takes LINK/INVALID "field OUT" (`dbLink.c:443-446`), and MS inherit runs
//! on the failure too. The source's CA put result is not asserted (C
//! answers ECA_PUTFAIL; carried, spec §2). Pinned to oracleStdIoc (C ground
//! truth §5.11, its DB verbatim).

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(calc,"CC"){field(CALC,"A+1")}
record(stringout,"SB"){field(OUT,"CC.CALC PP")}
record(waveform,"WN"){field(FTVL,"DOUBLE") field(NELM,"4")}
record(ao,"AW"){field(HIGH,"5") field(HSV,"MINOR") field(OUT,"WN.NORD PP MS")}
record(longout,"LT"){field(HIGH,"5") field(HSV,"MINOR")}
record(stringout,"SL"){field(OUT,"LT.VAL PP")}
record(calc,"CM"){field(CALC,"A+1")}
record(calcout,"SBM"){field(CALC,"A") field(HIGH,"5") field(HSV,"MINOR") field(OOPT,"Every Time") field(OUT,"CM.CALC PP MS")}
"#;

const NO_ALARM: (f64, f64) = (0.0, 0.0);
const UDF_INVALID: (f64, f64) = (17.0, 3.0);
const LINK_INVALID: (f64, f64) = (14.0, 3.0);
const LINK_MINOR: (f64, f64) = (14.0, 1.0);
const HIGH_MINOR: (f64, f64) = (4.0, 1.0);

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

fn text(db: &PvDatabase, pv: &str) -> String {
    match db.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        other => panic!("{pv} = {other:?}"),
    }
}

/// `(STAT, SEVR)`.
fn alarm(db: &PvDatabase, rec: &str) -> (f64, f64) {
    (
        num(db, &format!("{rec}.STAT")),
        num(db, &format!("{rec}.SEVR")),
    )
}

#[epics_macros_rs::epics_test]
async fn a_refused_out_put_alarms_the_source_and_leaves_the_target_unprocessed() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let (mut cc_val, mut cc_calc) = {
        let r = db.get_record("CC").unwrap();
        let mut g = r.write();
        let mask = (EventMask::VALUE | EventMask::ALARM).bits();
        let v = g
            .add_subscriber("VAL", 1, DbFieldType::Double, mask)
            .unwrap();
        let c = g
            .add_subscriber("CALC", 2, DbFieldType::String, mask)
            .unwrap();
        (v, c)
    };
    while cc_val.try_recv().is_ok() {}
    while cc_calc.try_recv().is_ok() {}

    // calc's special refuses "A+" after dbPut stored it.
    let _ = db
        .put_record_field_from_ca("SB", "VAL", EpicsValue::String("A+".into()))
        .await;
    assert_eq!(text(&db, "CC.CALC"), "A+");
    assert_eq!(num(&db, "CC"), 0.0);
    assert_eq!(num(&db, "CC.UDF"), 1.0);
    assert_eq!(alarm(&db, "CC"), UDF_INVALID);
    assert!(cc_val.try_recv().is_err(), "CC not processed");
    assert!(cc_calc.try_recv().is_err(), "CC.CALC not posted");
    assert_eq!(alarm(&db, "SB"), LINK_INVALID);
    assert_eq!(text(&db, "SB.AMSG"), "field OUT");

    db.put_record_field_from_ca("SB", "VAL", EpicsValue::String("A+2".into()))
        .await
        .unwrap();
    assert_eq!(text(&db, "CC.CALC"), "A+2");
    assert_eq!(num(&db, "CC"), 2.0);
    assert_eq!(alarm(&db, "CC"), NO_ALARM);
    assert_eq!(alarm(&db, "SB"), NO_ALARM);

    // NORD is SPC_NOMOD: refused, WN not processed, but MS inherit already
    // left LINK/MINOR pending on WN for its next process.
    let _ = db
        .put_record_field_from_ca("AW", "VAL", EpicsValue::Double(7.0))
        .await;
    assert_eq!(num(&db, "WN.NORD"), 0.0);
    assert_eq!(alarm(&db, "WN"), UDF_INVALID);
    assert_eq!(alarm(&db, "AW"), LINK_INVALID);
    assert_eq!(text(&db, "AW.AMSG"), "field OUT");
    db.put_record_field_from_ca("WN", "PROC", EpicsValue::UChar(1))
        .await
        .unwrap();
    assert_eq!(alarm(&db, "WN"), LINK_MINOR);
    assert_eq!(text(&db, "WN.AMSG"), "");

    let _ = db
        .put_record_field_from_ca("SL", "VAL", EpicsValue::String("abc".into()))
        .await;
    assert_eq!(num(&db, "LT"), 0.0);
    assert_eq!(alarm(&db, "LT"), UDF_INVALID);
    assert_eq!(alarm(&db, "SL"), LINK_INVALID);
    assert_eq!(text(&db, "SL.AMSG"), "field OUT");
    db.put_record_field_from_ca("SL", "VAL", EpicsValue::String("9".into()))
        .await
        .unwrap();
    assert_eq!(num(&db, "LT"), 9.0);
    assert_eq!(alarm(&db, "LT"), HIGH_MINOR);
    assert_eq!(alarm(&db, "SL"), NO_ALARM);

    db.put_record_field_from_ca("SBM", "A", EpicsValue::Double(7.0))
        .await
        .unwrap();
    assert_eq!(text(&db, "CM.CALC"), "7");
    assert_eq!(num(&db, "CM"), 7.0);
    assert_eq!(alarm(&db, "CM"), LINK_MINOR);
    assert_eq!(alarm(&db, "SBM"), HIGH_MINOR);
}
