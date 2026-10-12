//! Spec §5 test 4: `putStringString` keeps `field_size - 1` bytes
//! (`dbConvert.c:916-926`) on both the lean `Str` arm (stringout VAL) and
//! the general one (ao ASG, size 29), with no alarm on the cut. Only an lso
//! source carries more than 39 bytes to a link, as one `DBR_STRING` into ao
//! ASG (`dbPutLinkLS`, `dbLink.c:527-538`). Pinned to oracleStdIoc (C ground
//! truth §5.4, its DB verbatim).

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(stringout,"SO"){field(OUT,"SOT.VAL NPP")}
record(stringout,"SOT"){}
record(stringout,"SA"){field(OUT,"AT.ASG NPP")}
record(ao,"AT"){}
record(lso,"LS"){field(SIZV,"64") field(OUT,"SOT2.VAL NPP")}
record(stringout,"SOT2"){}
record(lso,"LSA"){field(SIZV,"64") field(OUT,"AT2.ASG NPP")}
record(ao,"AT2"){}
"#;

fn text(db: &PvDatabase, pv: &str) -> String {
    match db.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        other => panic!("{pv} = {other:?}"),
    }
}

fn no_alarm(db: &PvDatabase, rec: &str) {
    for f in ["STAT", "SEVR"] {
        assert_eq!(
            db.get_pv(&format!("{rec}.{f}")).unwrap().to_f64(),
            Some(0.0),
            "{rec}.{f}"
        );
    }
}

fn chars(s: &str) -> EpicsValue {
    let mut b = s.as_bytes().to_vec();
    b.push(0);
    EpicsValue::CharArray(b.into())
}

#[epics_macros_rs::epics_test]
async fn a_string_link_put_keeps_field_size_less_one() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let d39 = "123456789012345678901234567890123456789";
    db.put_record_field_from_ca("SO", "VAL", EpicsValue::String(d39.into()))
        .await
        .unwrap();
    assert_eq!(text(&db, "SOT"), d39);
    no_alarm(&db, "SO");

    // The 40th byte never reaches the record, let alone the link.
    db.put_record_field_from_ca("SO", "VAL", EpicsValue::String(format!("{d39}0").into()))
        .await
        .unwrap();
    assert_eq!(text(&db, "SO"), d39);
    assert_eq!(text(&db, "SOT"), d39);

    let d28 = "1234567890123456789012345678";
    for put in [d28, "12345678901234567890123456789"] {
        db.put_record_field_from_ca("SA", "VAL", EpicsValue::String(put.into()))
            .await
            .unwrap();
        assert_eq!(text(&db, "AT.ASG"), d28, "{put}");
        no_alarm(&db, "SA");
    }

    let a50 = "abcdefghij".repeat(5);
    db.put_record_field_from_ca("LS", "VAL$", chars(&a50))
        .await
        .unwrap();
    assert_eq!(text(&db, "SOT2"), &a50[..39]);
    no_alarm(&db, "LS");

    db.put_record_field_from_ca("LSA", "VAL$", chars("abcdefghijabcdefghijabcdefghijXYZ"))
        .await
        .unwrap();
    assert_eq!(text(&db, "AT2.ASG"), "abcdefghijabcdefghijabcdefgh");
    no_alarm(&db, "LSA");
}
