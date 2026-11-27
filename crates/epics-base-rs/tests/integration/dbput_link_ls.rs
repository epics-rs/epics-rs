//! C `dbPutLinkLS` (`dbLink.c:527-538`): lso and printf put their long
//! string as `DBR_CHAR` elements only into a `DBF_CHAR`/`DBF_UCHAR` field,
//! and as one `DBR_STRING` into any other, so ao reads "12.5" as 12.5, not
//! '1'. An aao CHAR source is a plain `dbPutLink` and keeps the array, into
//! a LONG waveform too. Pinned to oracleStdIoc, this DB verbatim.

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(lso,"LS"){field(SIZV,"64") field(OUT,"TA.VAL NPP")}
record(ao,"TA"){}
record(lso,"LL"){field(SIZV,"64") field(OUT,"TL.VAL NPP")}
record(longout,"TL"){}
record(lso,"LW"){field(SIZV,"64") field(OUT,"TW.VAL NPP")}
record(waveform,"TW"){field(FTVL,"LONG") field(NELM,"8")}
record(lso,"LC"){field(SIZV,"64") field(OUT,"TC.VAL NPP")}
record(waveform,"TC"){field(FTVL,"CHAR") field(NELM,"16")}
record(printf,"PF"){field(FMT,"12.5") field(OUT,"PTA.VAL NPP")}
record(ao,"PTA"){}
record(printf,"PL"){field(FMT,"42") field(OUT,"PTL.VAL NPP")}
record(longout,"PTL"){}
record(aao,"AA"){field(FTVL,"CHAR") field(NELM,"8") field(OUT,"ATC.VAL NPP")}
record(waveform,"ATC"){field(FTVL,"CHAR") field(NELM,"16")}
record(aao,"AL"){field(FTVL,"CHAR") field(NELM,"8") field(OUT,"ATL.VAL NPP")}
record(waveform,"ATL"){field(FTVL,"LONG") field(NELM,"16")}
"#;

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

fn chars(s: &str) -> EpicsValue {
    let mut b = s.as_bytes().to_vec();
    b.push(0);
    EpicsValue::CharArray(b.into())
}

fn bytes(db: &PvDatabase, pv: &str) -> Vec<u8> {
    match db.get_pv(pv).unwrap() {
        EpicsValue::CharArray(b) => b.to_vec(),
        other => panic!("{pv} = {other:?}"),
    }
}

#[epics_macros_rs::epics_test]
async fn a_long_string_puts_one_string_into_a_field_that_is_not_char() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    for (rec, s) in [("LS", "12.5"), ("LL", "42"), ("LW", "7"), ("LC", "hello")] {
        db.put_record_field_from_ca(rec, "VAL$", chars(s))
            .await
            .unwrap();
    }
    for rec in ["PF", "PL"] {
        db.put_record_field_from_ca(rec, "PROC", EpicsValue::Short(1))
            .await
            .unwrap();
    }
    for rec in ["AA", "AL"] {
        db.put_record_field_from_ca(rec, "VAL", EpicsValue::CharArray(vec![104, 105, 0].into()))
            .await
            .unwrap();
    }

    assert_eq!(num(&db, "TA"), 12.5);
    assert_eq!(num(&db, "TL"), 42.0);
    assert_eq!(num(&db, "TW.NORD"), 1.0);
    match db.get_pv("TW").unwrap() {
        EpicsValue::LongArray(v) => assert_eq!(v[..1], [7]),
        other => panic!("TW = {other:?}"),
    }
    assert_eq!(num(&db, "TC.NORD"), 6.0, "a CHAR target takes LEN chars");
    assert_eq!(bytes(&db, "TC")[..6], *b"hello\0");
    assert_eq!(num(&db, "PTA"), 12.5);
    assert_eq!(num(&db, "PTL"), 42.0);
    assert_eq!(num(&db, "ATC.NORD"), 3.0, "aao keeps the array");
    assert_eq!(bytes(&db, "ATC")[..3], [104, 105, 0]);
    assert_eq!(num(&db, "ATL.NORD"), 3.0);
    match db.get_pv("ATL").unwrap() {
        EpicsValue::LongArray(v) => assert_eq!(v[..3], [104, 105, 0]),
        other => panic!("ATL = {other:?}"),
    }
    for rec in ["LS", "LL", "LW", "LC", "PF", "PL", "AA", "AL"] {
        assert_eq!(num(&db, &format!("{rec}.SEVR")), 0.0, "{rec}");
    }
}
