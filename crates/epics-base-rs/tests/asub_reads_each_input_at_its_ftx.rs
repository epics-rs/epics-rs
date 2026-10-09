//! aSub reads each INPx at its FTx with `nRequest = NOx` (`aSubRecord.c:283`),
//! pinned to softIocPVX (pvxs 1.5.3, R7.0.10) through pvxget, so int64
//! values are exact. I1 = 2^53+1, I2 = 5e9, WI = [2^53+1, 5e9, -1]:
//!
//! ```text
//! INT64 I1 9007199254740993   LONG I1 1           DOUBLE I1 9.0072e+15
//! STRING I1 "9007199254740993"                    ULONG I1 1
//! INT64 I2 5000000000         LONG I2 705032704   SHORT I2 -3584
//! UINT64 I1 9007199254740993  FLOAT I1 9.0072e+15 STRING I2 "5000000000"
//! NOA 3: INT64 I1 {1}[9007199254740993]  LONG I1 {1}[1]
//!        INT64 WI {3}[9007199254740993, 5000000000, -1]
//!        LONG WI {3}[1, 705032704, -1]
//! ```

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(int64in, "I1") { field(VAL, "9007199254740993") }
record(int64in, "I2") { field(VAL, "5000000000") }
record(waveform, "WI") { field(FTVL, "INT64") field(NELM, "3") }
record(aSub, "A_INT64_1") { field(FTA, "INT64") field(NOA, "1") field(INPA, "I1") }
record(aSub, "A_LONG_1") { field(FTA, "LONG") field(NOA, "1") field(INPA, "I1") }
record(aSub, "A_DOUBLE_1") { field(FTA, "DOUBLE") field(NOA, "1") field(INPA, "I1") }
record(aSub, "A_STRING_1") { field(FTA, "STRING") field(NOA, "1") field(INPA, "I1") }
record(aSub, "A_INT64_2") { field(FTA, "INT64") field(NOA, "1") field(INPA, "I2") }
record(aSub, "A_LONG_2") { field(FTA, "LONG") field(NOA, "1") field(INPA, "I2") }
record(aSub, "A_DOUBLE_2") { field(FTA, "DOUBLE") field(NOA, "1") field(INPA, "I2") }
record(aSub, "A_STRING_2") { field(FTA, "STRING") field(NOA, "1") field(INPA, "I2") }
record(aSub, "A_ULONG_1") { field(FTA, "ULONG") field(NOA, "1") field(INPA, "I1") }
record(aSub, "A_SHORT_2") { field(FTA, "SHORT") field(NOA, "1") field(INPA, "I2") }
record(aSub, "A_UINT64_1") { field(FTA, "UINT64") field(NOA, "1") field(INPA, "I1") }
record(aSub, "A_FLOAT_1") { field(FTA, "FLOAT") field(NOA, "1") field(INPA, "I1") }
record(aSub, "A_INT64_N") { field(FTA, "INT64") field(NOA, "3") field(INPA, "I1") }
record(aSub, "A_LONG_N") { field(FTA, "LONG") field(NOA, "3") field(INPA, "I1") }
record(aSub, "A_INT64_W") { field(FTA, "INT64") field(NOA, "3") field(INPA, "WI") }
record(aSub, "A_LONG_W") { field(FTA, "LONG") field(NOA, "3") field(INPA, "WI") }
"#;

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
}

#[epics_macros_rs::epics_test]
async fn each_input_reads_at_its_ftx_like_softiocpvx() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.put_record_field_from_ca(
        "WI",
        "VAL",
        EpicsValue::Int64Array(vec![9007199254740993, 5000000000, -1].into()),
    )
    .await
    .unwrap();
    let rows: [(&str, EpicsValue); 16] = [
        ("A_INT64_1", EpicsValue::Int64(9007199254740993)),
        ("A_LONG_1", EpicsValue::Long(1)),
        (
            "A_DOUBLE_1",
            EpicsValue::Double(9007199254740993_i64 as f64),
        ),
        ("A_STRING_1", EpicsValue::String("9007199254740993".into())),
        ("A_INT64_2", EpicsValue::Int64(5000000000)),
        ("A_LONG_2", EpicsValue::Long(705032704)),
        ("A_DOUBLE_2", EpicsValue::Double(5e9)),
        ("A_STRING_2", EpicsValue::String("5000000000".into())),
        ("A_ULONG_1", EpicsValue::ULong(1)),
        ("A_SHORT_2", EpicsValue::Short(-3584)),
        ("A_UINT64_1", EpicsValue::UInt64(9007199254740993)),
        ("A_FLOAT_1", EpicsValue::Float(9007199254740993_i64 as f32)),
        (
            "A_INT64_N",
            EpicsValue::Int64Array(vec![9007199254740993].into()),
        ),
        ("A_LONG_N", EpicsValue::LongArray(vec![1].into())),
        (
            "A_INT64_W",
            EpicsValue::Int64Array(vec![9007199254740993, 5000000000, -1].into()),
        ),
        (
            "A_LONG_W",
            EpicsValue::LongArray(vec![1, 705032704, -1].into()),
        ),
    ];
    for (rec, want) in rows {
        process(&db, rec).await;
        assert_eq!(db.get_pv(&format!("{rec}.A")).unwrap(), want, "{rec}");
    }
}
