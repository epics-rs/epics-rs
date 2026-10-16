//! A `DBR_STRING` link read of a float source renders at the SOURCE's PREC
//! (`cvt_d_st`, `dbFastLinkConv.c:1333-1346`): scalcout INAA..INLL ask for
//! `DBR_STRING` unless the source is a char array (`sCalcoutRecord.c:912-935`),
//! as stringin's INP does. Pinned to oracleStdIoc on this DB verbatim.

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao,"A1"){field(PREC,"1") field(VAL,"0.5")}
record(ao,"A3"){field(PREC,"3") field(VAL,"0.5")}
record(ao,"A0"){field(VAL,"0.5")}
record(stringin,"S1"){field(INP,"A1 NPP")}
record(stringin,"S3"){field(INP,"A3 NPP")}
record(scalcout,"SA1"){field(INAA,"A1 NPP") field(CALC,"AA")}
record(scalcout,"SA3"){field(INAA,"A3 NPP") field(CALC,"AA")}
record(scalcout,"SA0"){field(INAA,"A0 NPP") field(CALC,"AA")}
record(bo,"B"){field(ZNAM,"Off") field(ONAM,"On") field(VAL,"1")}
record(waveform,"W"){field(FTVL,"CHAR") field(NELM,"16")}
record(scalcout,"XB"){field(INAA,"B NPP") field(CALC,"AA")}
record(scalcout,"XW"){field(INAA,"W NPP") field(CALC,"AA")}
record(scalcout,"XN"){field(INAA,"NOPE NPP") field(CALC,"AA")}
"#;

fn text(db: &PvDatabase, pv: &str) -> String {
    match db.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.to_string(),
        other => panic!("{pv} = {other:?}"),
    }
}

#[epics_macros_rs::epics_test]
async fn a_string_link_read_renders_a_float_source_at_its_prec() {
    let (db, _) = IocBuilder::new()
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    for rec in ["A1", "A3", "A0", "B"] {
        db.put_record_field_from_ca(rec, "PROC", EpicsValue::Short(1))
            .await
            .unwrap();
    }
    db.put_record_field_from_ca(
        "W",
        "VAL",
        EpicsValue::CharArray(b"he\"l\0".to_vec().into()),
    )
    .await
    .unwrap();
    for rec in ["S1", "S3", "SA1", "SA3", "SA0", "XB", "XW", "XN"] {
        db.put_record_field_from_ca(rec, "PROC", EpicsValue::Short(1))
            .await
            .unwrap();
    }

    assert_eq!(text(&db, "S1"), "0.5");
    assert_eq!(text(&db, "S3"), "0.500");
    assert_eq!(text(&db, "SA1.AA"), "0.5");
    assert_eq!(text(&db, "SA3.AA"), "0.500");
    assert_eq!(text(&db, "SA0.AA"), "1");
    assert_eq!(text(&db, "SA1.SVAL"), "0.5");
    assert_eq!(text(&db, "SA3.SVAL"), "0.500");
    assert_eq!(text(&db, "XB.AA"), "On");
    assert_eq!(
        text(&db, "XW.AA"),
        "he\\\"l",
        "a char array keeps its escaped bytes"
    );
    assert_eq!(text(&db, "XN.AA"), "XN:fetch(AA) failed");
}
