//! A scalar `dbPut` into a waveform's VAL is one element: the convert
//! routine writes `bptr[0]` and `put_array_info` sets NORD to 1
//! (`dbAccess.c:1350-1367`), from an ao OUT link, an lso's `DBR_STRING`
//! and a CA put alike. Pinned to softIoc on this DB verbatim.

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{EpicsValue, PvString};

const DB: &str = r#"
record(waveform,"W"){field(FTVL,"LONG") field(NELM,"8")}
record(ao,"A"){field(OUT,"W.VAL NPP")}
record(lso,"L"){field(SIZV,"64") field(OUT,"WL.VAL NPP")}
record(waveform,"WL"){field(FTVL,"LONG") field(NELM,"8")}
record(waveform,"WU"){field(FTVL,"ULONG") field(NELM,"8")}
record(ao,"AU"){field(OUT,"WU.VAL NPP")}
record(waveform,"WC"){field(FTVL,"CHAR") field(NELM,"8")}
record(ao,"AC"){field(OUT,"WC.VAL NPP")}
record(waveform,"WD"){field(FTVL,"DOUBLE") field(NELM,"8")}
record(waveform,"WS"){field(FTVL,"SHORT") field(NELM,"8")}
record(waveform,"W1"){field(FTVL,"LONG") field(NELM,"1")}
record(ao,"A1"){field(OUT,"W1.VAL NPP")}
record(waveform,"WN"){field(FTVL,"LONG") field(NELM,"8")}
"#;

fn s(v: &str) -> EpicsValue {
    EpicsValue::String(PvString::from(v))
}

fn nord(db: &PvDatabase, rec: &str) -> f64 {
    db.get_pv(&format!("{rec}.NORD")).unwrap().to_f64().unwrap()
}

#[epics_macros_rs::epics_test]
async fn a_scalar_put_into_a_waveform_writes_element_0_and_nord_1() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let put = |rec: &'static str, field: &'static str, v: EpicsValue| {
        let db = db.clone();
        async move { db.put_record_field_from_ca(rec, field, v).await }
    };
    put("W", "VAL", EpicsValue::LongArray(vec![1, 2, 3].into()))
        .await
        .unwrap();
    put("A", "VAL", s("3.9")).await.unwrap();
    let mut seven = b"7".to_vec();
    seven.push(0);
    put("L", "VAL$", EpicsValue::CharArray(seven.into()))
        .await
        .unwrap();
    put("AU", "VAL", s("-1")).await.unwrap();
    put("AC", "VAL", s("200")).await.unwrap();
    put("WD", "VAL", s("2.5")).await.unwrap();
    put("WS", "VAL", s("0x10")).await.unwrap();
    assert!(put("WS", "VAL", s("hi")).await.is_err());
    put("A1", "VAL", s("-7.5")).await.unwrap();
    put("WN", "VAL", EpicsValue::Double(3.9)).await.unwrap();

    let rows = [
        ("W", EpicsValue::LongArray(vec![3].into())),
        ("WL", EpicsValue::LongArray(vec![7].into())),
        ("WU", EpicsValue::ULongArray(vec![u32::MAX].into())),
        ("WC", EpicsValue::CharArray(vec![200].into())),
        ("WD", EpicsValue::DoubleArray(vec![2.5].into())),
        ("WS", EpicsValue::ShortArray(vec![16].into())),
        ("W1", EpicsValue::LongArray(vec![-7].into())),
        ("WN", EpicsValue::LongArray(vec![3].into())),
    ];
    for (rec, want) in rows {
        assert_eq!(db.get_pv(rec).unwrap(), want, "{rec}");
        assert_eq!(nord(&db, rec), 1.0, "{rec}.NORD");
    }
}
