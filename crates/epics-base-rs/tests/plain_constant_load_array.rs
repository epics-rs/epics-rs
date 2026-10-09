//! C `dbConstLoadArray` (`dbConstLink.c:194-212`): a plain constant INP on a
//! waveform goes through `dbPutConvertJSON`, whose yajl takes JSON5 numbers
//! (hex) and surrounding blanks; the ai beside it is the scalar
//! `dbConstLoadScalar` path. Text that is not a number is a PV link in both.
//! softIoc R7.0.10.1, `dbgf` of NORD/UDF/VAL; iocInit printed no error line.
//! The one difference is E_CHAR, below.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;

const INPS: [(&str, &str); 6] = [
    ("H", "0x1f"),
    ("E", "1e3"),
    ("B", "abc"),
    ("W", " 5 "),
    ("C", "5,6"),
    ("R", "[1,2]"),
];

fn db() -> String {
    let mut db = String::new();
    for (p, inp) in INPS {
        for ftvl in ["DOUBLE", "LONG", "CHAR"] {
            db.push_str(&format!(
                "record(waveform,\"{p}_{ftvl}\"){{field(FTVL,\"{ftvl}\") field(NELM,\"10\") field(INP,\"{inp}\")}}\n"
            ));
        }
        db.push_str(&format!("record(ai,\"{p}_AI\"){{field(INP,\"{inp}\")}}\n"));
    }
    db
}

#[epics_macros_rs::epics_test]
async fn plain_constant_waveform_matches_softioc_dbconstloadarray_dbconstlink_c_194() {
    let (db, _h) = IocBuilder::new()
        .db_string(&db(), &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    let wf = |d: &[f64]| {
        [
            EpicsValue::DoubleArray(d.to_vec().into()),
            EpicsValue::LongArray(d.iter().map(|v| *v as i32).collect()),
            // `(epicsInt8)1000.0` is UB; the x86 softIoc stores 0xe8 and
            // `types::c_cast` takes the same 32-bit step, so E_CHAR holds 0xe8.
            EpicsValue::CharArray(d.iter().map(|v| *v as i32 as u8).collect()),
        ]
    };
    // (prefix, waveform elements or None for a failed load, ai VAL, ai UDF)
    let expect: [(&str, Option<Vec<f64>>, f64, f64); 6] = [
        ("H", Some(vec![31.0]), 31.0, 0.0),
        ("E", Some(vec![1000.0]), 1000.0, 0.0),
        ("B", None, 0.0, 1.0),
        ("W", Some(vec![5.0]), 5.0, 0.0),
        ("C", None, 0.0, 1.0),
        ("R", Some(vec![1.0, 2.0]), 1.0, 0.0),
    ];

    let mut wrong = Vec::new();
    let f = |rec: &str, field: &str| db.get_pv(&format!("{rec}.{field}")).ok();
    for (p, elems, ai_val, ai_udf) in expect {
        let vals = wf(elems.as_deref().unwrap_or(&[]));
        let (nord, udf) = match &elems {
            Some(e) => (e.len() as f64, 0.0),
            None => (0.0, 1.0),
        };
        for (ftvl, val) in ["DOUBLE", "LONG", "CHAR"].into_iter().zip(vals) {
            let rec = format!("{p}_{ftvl}");
            let got = (
                f(&rec, "NORD").and_then(|v| v.to_f64()),
                f(&rec, "UDF").and_then(|v| v.to_f64()),
                f(&rec, "VAL"),
            );
            if got != (Some(nord), Some(udf), Some(val.clone())) {
                wrong.push(format!("{rec}: got {got:?}, C {:?}", (nord, udf, val)));
            }
        }
        let rec = format!("{p}_AI");
        let got = (
            f(&rec, "UDF").and_then(|v| v.to_f64()),
            f(&rec, "VAL").and_then(|v| v.to_f64()),
        );
        if got != (Some(ai_udf), Some(ai_val)) {
            wrong.push(format!("{rec}: got {got:?}, C {:?}", (ai_udf, ai_val)));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
