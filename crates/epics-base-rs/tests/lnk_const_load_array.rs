//! C `lnkConst_loadArray` (`lnkConst.c:451-541`) through `devWfSoft`'s
//! `init_record`: every FTVL against each `{const: …}` value type. softIoc
//! R7.0.10.1 with the DB below, `dbgf` of NORD, UDF and VAL per record.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;

const LONG: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ012345678";

fn db() -> String {
    let consts = [
        ("S", format!("\"{LONG}\""), 100),
        ("N", "5".into(), 100),
        ("D", "2.5".into(), 100),
        ("Q", "\"12\"".into(), 100),
        ("A", "[1,2,3]".into(), 100),
        ("T", "[\"7\",\"8\"]".into(), 100),
        ("X", format!("\"{LONG}\""), 3),
    ];
    let mut db = String::new();
    for (p, value, nelm) in &consts {
        for ftvl in ["CHAR", "UCHAR", "STRING", "DOUBLE", "LONG"] {
            db.push_str(&format!(
                "record(waveform,\"{p}_{ftvl}\"){{field(FTVL,\"{ftvl}\") field(NELM,\"{nelm}\") field(INP,{{const:{value}}})}}\n"
            ));
        }
    }
    db
}

#[epics_macros_rs::epics_test]
async fn lnk_const_load_array_matches_softioc_lnkconst_c_451() {
    let (db, _h) = IocBuilder::new()
        .db_string(&db(), &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    let long_chars = |n: usize| {
        let mut v = LONG.as_bytes()[..n].to_vec();
        v.push(0);
        EpicsValue::CharArray(v.into())
    };
    let strings = |v: &[&str]| EpicsValue::StringArray(v.iter().map(|s| (*s).into()).collect());
    let first_39 = &LONG[..39];
    let defined = |nord: f64, val: EpicsValue| (nord, 0.0, val);
    let failed = (0.0, 1.0, EpicsValue::UCharArray(vec![].into()));

    let expect: Vec<(&str, (f64, f64, EpicsValue))> = vec![
        ("S_CHAR", defined(72.0, long_chars(71))),
        ("S_UCHAR", failed.clone()),
        ("S_STRING", defined(1.0, strings(&[first_39]))),
        (
            "S_DOUBLE",
            defined(1.0, EpicsValue::DoubleArray(vec![123456789.0].into())),
        ),
        (
            "S_LONG",
            defined(1.0, EpicsValue::LongArray(vec![342391].into())),
        ),
        (
            "N_CHAR",
            defined(1.0, EpicsValue::CharArray(vec![5].into())),
        ),
        (
            "N_UCHAR",
            defined(1.0, EpicsValue::UCharArray(vec![5].into())),
        ),
        ("N_STRING", defined(1.0, strings(&["5"]))),
        (
            "N_DOUBLE",
            defined(1.0, EpicsValue::DoubleArray(vec![5.0].into())),
        ),
        (
            "N_LONG",
            defined(1.0, EpicsValue::LongArray(vec![5].into())),
        ),
        (
            "D_CHAR",
            defined(1.0, EpicsValue::CharArray(vec![2].into())),
        ),
        (
            "D_UCHAR",
            defined(1.0, EpicsValue::UCharArray(vec![2].into())),
        ),
        ("D_STRING", defined(1.0, strings(&["2.500000"]))),
        (
            "D_DOUBLE",
            defined(1.0, EpicsValue::DoubleArray(vec![2.5].into())),
        ),
        (
            "D_LONG",
            defined(1.0, EpicsValue::LongArray(vec![2].into())),
        ),
        (
            "Q_CHAR",
            defined(3.0, EpicsValue::CharArray(b"12\0".to_vec().into())),
        ),
        (
            "Q_UCHAR",
            defined(1.0, EpicsValue::UCharArray(vec![12].into())),
        ),
        ("Q_STRING", defined(1.0, strings(&["12"]))),
        (
            "Q_DOUBLE",
            defined(1.0, EpicsValue::DoubleArray(vec![12.0].into())),
        ),
        (
            "Q_LONG",
            defined(1.0, EpicsValue::LongArray(vec![12].into())),
        ),
        (
            "A_CHAR",
            defined(3.0, EpicsValue::CharArray(vec![1, 2, 3].into())),
        ),
        (
            "A_UCHAR",
            defined(3.0, EpicsValue::UCharArray(vec![1, 2, 3].into())),
        ),
        ("A_STRING", defined(3.0, strings(&["1", "2", "3"]))),
        (
            "A_DOUBLE",
            defined(3.0, EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0].into())),
        ),
        (
            "A_LONG",
            defined(3.0, EpicsValue::LongArray(vec![1, 2, 3].into())),
        ),
        (
            "T_CHAR",
            defined(2.0, EpicsValue::CharArray(vec![7, 8].into())),
        ),
        (
            "T_UCHAR",
            defined(2.0, EpicsValue::UCharArray(vec![7, 8].into())),
        ),
        ("T_STRING", defined(2.0, strings(&["7", "8"]))),
        (
            "T_DOUBLE",
            defined(2.0, EpicsValue::DoubleArray(vec![7.0, 8.0].into())),
        ),
        (
            "T_LONG",
            defined(2.0, EpicsValue::LongArray(vec![7, 8].into())),
        ),
        (
            "X_CHAR",
            defined(3.0, EpicsValue::CharArray(b"01\0".to_vec().into())),
        ),
        ("X_UCHAR", failed.clone()),
        ("X_STRING", defined(1.0, strings(&[first_39]))),
        (
            "X_DOUBLE",
            defined(1.0, EpicsValue::DoubleArray(vec![123456789.0].into())),
        ),
        (
            "X_LONG",
            defined(1.0, EpicsValue::LongArray(vec![342391].into())),
        ),
    ];

    let mut wrong = Vec::new();
    for (rec, (nord, udf, val)) in expect {
        let f = |field: &str| db.get_pv(&format!("{rec}.{field}")).ok();
        let got = (
            f("NORD").and_then(|v| v.to_f64()),
            f("UDF").and_then(|v| v.to_f64()),
            f("VAL"),
        );
        if got != (Some(nord), Some(udf), Some(val.clone())) {
            wrong.push(format!("{rec}: got {got:?}, C {:?}", (nord, udf, val)));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
