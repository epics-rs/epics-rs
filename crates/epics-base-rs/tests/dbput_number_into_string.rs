//! Spec §5 test 5: a numeric OUT put into a string field formats with the
//! TARGET's precision (`dbFastLinkConv.c:1333` `cvt_d_st`, `paddr` = the
//! target); stringout and stringin have no `get_precision`, so 6. Pinned to
//! oracleStdIoc (C ground truth §5.5, its DB verbatim, and the 05b
//! `cvtDoubleToString` table).

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(stringout,"S1"){}
record(stringout,"S2"){}
record(stringin,"S3"){}
record(stringout,"S4"){}
record(stringout,"S5"){}
record(stringout,"S6"){}
record(stringout,"S7"){}
record(ao,"AO"){field(PREC,"3") field(OUT,"S1.VAL NPP")}
record(longout,"LO"){field(OUT,"S2.VAL NPP")}
record(ao,"AOI"){field(PREC,"1") field(OUT,"S3.VAL PP")}
record(int64out,"I6"){field(OUT,"S4.VAL NPP")}
record(bo,"BO"){field(ZNAM,"zero") field(ONAM,"one") field(OUT,"S5.VAL NPP")}
record(mbbo,"MBO"){field(ZRST,"z") field(ONST,"o") field(TWST,"t") field(OUT,"S6.VAL NPP")}
record(ao,"AOB"){field(OUT,"S7.VAL NPP")}
"#;

fn text(db: &PvDatabase, pv: &str) -> String {
    match db.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        other => panic!("{pv} = {other:?}"),
    }
}

#[epics_macros_rs::epics_test]
async fn a_number_into_a_string_takes_the_targets_precision() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut s3 = {
        let r = db.get_record("S3").unwrap();
        let mut g = r.write();
        let mask = (EventMask::VALUE | EventMask::ALARM).bits();
        g.add_subscriber("VAL", 1, DbFieldType::String, mask)
            .unwrap()
    };
    while s3.try_recv().is_ok() {}

    for (src, value, target, want) in [
        ("AO", EpicsValue::Double(2.5), "S1", "2.500000"),
        ("LO", EpicsValue::Long(42), "S2", "42"),
        ("AOI", EpicsValue::Double(2.5), "S3", "2.500000"),
        ("I6", EpicsValue::Int64(-9_000_000_000), "S4", "-9000000000"),
        // Soft device support writes VAL as a number, not the state string.
        ("BO", EpicsValue::Enum(1), "S5", "1"),
        ("MBO", EpicsValue::Enum(2), "S6", "2"),
    ] {
        db.put_record_field_from_ca(src, "VAL", value)
            .await
            .unwrap();
        assert_eq!(text(&db, target), want, "{src}");
        assert_eq!(
            db.get_pv(&format!("{src}.SEVR")).unwrap().to_f64(),
            Some(0.0)
        );
    }
    // PP: the stringin's process posts VAL once. A string post is a
    // reference read at delivery (`dbEvent.c:493-500`), so the readback above
    // is its text.
    assert!(s3.try_recv().is_ok());
    assert!(s3.try_recv().is_err());

    for (v, want) in [
        (1e300, "1.000000e+300"),
        (-1e300, "-1.000000e+300"),
        (1e17, " 1.000000e+17"),
        (1e16, "10000000000000000.000"),
        (12345678.5, "12345678.500"),
        (10000000.0, "10000000.000000"),
        (10000000.5, "10000000.500"),
        (-12345678.25, "-12345678.250"),
        (1e-300, "0.000000"),
        (f64::NAN, "nan"),
        (f64::NEG_INFINITY, "         -inf"),
        (f64::INFINITY, "          inf"),
        (0.1, "0.100000"),
        (-0.0000005, "-0.000001"),
        (0.0000005, "0.000001"),
        (99999.9999996, "100000.000000"),
        (2147483648.5, "2147483648.500"),
        (1.5e9, "1500000000.000"),
    ] {
        db.put_record_field_from_ca("AOB", "VAL", EpicsValue::Double(v))
            .await
            .unwrap();
        assert_eq!(text(&db, "S7"), want, "{v:e}");
    }
}
