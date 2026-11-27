//! aSub's `dbGetLink(plink, FTx, buf, 0, &nRequest = NOx)` settles from the
//! channel's own FTx/NOx: an array source delivers up to NOx elements, a
//! scalar source one, each converted at FTx.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const FTX: [&str; 10] = [
    "CHAR", "UCHAR", "SHORT", "USHORT", "LONG", "ULONG", "INT64", "UINT64", "FLOAT", "DOUBLE",
];

fn db_text() -> String {
    let mut db = String::from(
        r#"
record(waveform, "W16") { field(FTVL, "LONG") field(NELM, "16") }
record(waveform, "W1024") { field(FTVL, "LONG") field(NELM, "1024") }
record(waveform, "WI") { field(FTVL, "INT64") field(NELM, "16") }
record(longin, "S1") { field(VAL, "7") }
"#,
    );
    for ft in FTX {
        for (name, n, src) in [
            ("16", 16, "W16"),
            ("1024", 1024, "W1024"),
            ("3S", 3, "S1"),
            ("3W", 3, "W16"),
        ] {
            db += &format!(
                "record(aSub, \"A_{ft}_{name}\") {{ field(FTA, \"{ft}\") field(NOA, \"{n}\") field(INPA, \"{src}\") }}\n"
            );
        }
    }
    db += "record(aSub, \"A_STR_16\") { field(FTA, \"STRING\") field(NOA, \"16\") field(INPA, \"W16\") }\n";
    db += "record(aSub, \"A_STR_3S\") { field(FTA, \"STRING\") field(NOA, \"3\") field(INPA, \"S1\") }\n";
    db += "record(aSub, \"A_I64_16\") { field(FTA, \"INT64\") field(NOA, \"16\") field(INPA, \"WI\") }\n";
    db
}

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
}

fn elements(v: &EpicsValue) -> Vec<f64> {
    match v.convert_to(DbFieldType::Double) {
        EpicsValue::DoubleArray(a) => a.to_vec(),
        EpicsValue::Double(x) => vec![x],
        other => panic!("{other:?}"),
    }
}

#[epics_macros_rs::epics_test]
async fn each_ftx_converts_the_requested_elements() {
    let db = IocBuilder::new()
        .db_string(&db_text(), &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    // Values stay inside CHAR so every FTx holds them.
    let wave = |n: i32| EpicsValue::LongArray((0..n).map(|i| i % 100).collect::<Vec<_>>().into());
    db.put_record_field_from_ca("W16", "VAL", wave(16))
        .await
        .unwrap();
    db.put_record_field_from_ca("W1024", "VAL", wave(1024))
        .await
        .unwrap();
    for ft in FTX {
        for (name, want) in [
            ("16", (0..16).map(f64::from).collect::<Vec<_>>()),
            ("1024", (0..1024).map(|i| f64::from(i % 100)).collect()),
            ("3S", vec![7.0]),
            ("3W", vec![0.0, 1.0, 2.0]),
        ] {
            let rec = format!("A_{ft}_{name}");
            process(&db, &rec).await;
            let got = db.get_pv(format!("{rec}.A")).unwrap();
            assert_eq!(elements(&got), want, "{rec}: {got:?}");
        }
    }
}

#[epics_macros_rs::epics_test]
async fn an_int64_array_source_keeps_every_element_exact() {
    let db = IocBuilder::new()
        .db_string(&db_text(), &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let src: Vec<i64> = (0..16).map(|i| 9007199254740993 + i).collect();
    db.put_record_field_from_ca("WI", "VAL", EpicsValue::Int64Array(src.clone().into()))
        .await
        .unwrap();
    process(&db, "A_I64_16").await;
    assert_eq!(
        db.get_pv("A_I64_16.A").unwrap(),
        EpicsValue::Int64Array(src.into())
    );
}

#[epics_macros_rs::epics_test]
async fn a_string_channel_renders_each_element_as_text() {
    let db = IocBuilder::new()
        .db_string(&db_text(), &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.put_record_field_from_ca(
        "W16",
        "VAL",
        EpicsValue::LongArray((0..16).collect::<Vec<_>>().into()),
    )
    .await
    .unwrap();
    process(&db, "A_STR_16").await;
    let EpicsValue::StringArray(got) = db.get_pv("A_STR_16.A").unwrap() else {
        panic!("not a string array");
    };
    let want: Vec<String> = (0..16).map(|i| i.to_string()).collect();
    assert_eq!(got.iter().map(|s| s.to_string()).collect::<Vec<_>>(), want);
    process(&db, "A_STR_3S").await;
    assert_eq!(db.get_pv("A_STR_3S.A").unwrap().text_g(), "[\"7\"]");
}
