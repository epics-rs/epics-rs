//! subArray's soft dset asks `dbLoadLinkArray` for `INDX + NELM` elements
//! clamped to MALM (`devSASoft.c:61-67`), then subsets from INDX. The clamp
//! shows on a long-string CHAR load, whose NUL lands at `nRequest - 1`
//! (`lnkConst.c:494-500`). oracleStdIoc R7.0.10.1, `dbgf` after `iocInit`.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;

const DB: &str = r#"
record(subArray, "S1") { field(FTVL, "CHAR") field(MALM, "4") field(NELM, "4") field(INDX, "2") field(INP, {const:"hello"}) }
record(subArray, "S2") { field(FTVL, "CHAR") field(MALM, "4") field(NELM, "3") field(INDX, "0") field(INP, {const:"hello"}) }
record(subArray, "S4") { field(FTVL, "CHAR") field(MALM, "5") field(NELM, "9") field(INDX, "1") field(INP, {const:"hello"}) }
record(subArray, "S5") { field(FTVL, "LONG") field(MALM, "4") field(NELM, "4") field(INDX, "2") field(INP, [1,2,3,4,5,6]) }
"#;

#[epics_macros_rs::epics_test]
async fn subarray_init_load_requests_at_most_malm_devsasoft_c_64() {
    let (db, _h) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    let expect: Vec<(&str, EpicsValue, f64)> = vec![
        ("S1", EpicsValue::CharArray(b"l\0".to_vec().into()), 2.0),
        ("S2", EpicsValue::CharArray(b"he\0".to_vec().into()), 3.0),
        ("S4", EpicsValue::CharArray(b"ell\0".to_vec().into()), 4.0),
        ("S5", EpicsValue::LongArray(vec![3, 4].into()), 2.0),
    ];
    let mut wrong = Vec::new();
    for (rec, val, nord) in expect {
        let got = (
            db.get_pv(&format!("{rec}.VAL")).ok(),
            db.get_pv(&format!("{rec}.NORD"))
                .ok()
                .and_then(|v| v.to_f64()),
            db.get_pv(&format!("{rec}.UDF"))
                .ok()
                .and_then(|v| v.to_f64()),
        );
        if got != (Some(val.clone()), Some(nord), Some(0.0)) {
            wrong.push(format!("{rec}: got {got:?}, C {:?}", (val, nord, 0.0)));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
