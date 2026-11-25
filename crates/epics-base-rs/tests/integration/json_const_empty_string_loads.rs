//! `{const:""}` is a `lnkConst` `sc40` link, not an empty link: its loads go
//! through `cvt_st_*` (`dbFastLinkConv.c:91-244`), which store 0 for "", and a
//! CHAR array gets the lone NUL (`lnkConst.c:494-500`). Only a plain CONSTANT
//! link refuses empty text (`dbConstLink.c:200`). oracleStdIoc R7.0.10.1,
//! `dbgf` after `iocInit`.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;

const DB: &str = r#"
record(waveform, "E_CHAR") { field(FTVL, "CHAR") field(NELM, "100") field(INP, {const:""}) }
record(waveform, "E_UCHAR") { field(FTVL, "UCHAR") field(NELM, "100") field(INP, {const:""}) }
record(waveform, "E_STRING") { field(FTVL, "STRING") field(NELM, "100") field(INP, {const:""}) }
record(waveform, "E_DOUBLE") { field(FTVL, "DOUBLE") field(NELM, "100") field(INP, {const:""}) }
record(waveform, "E_LONG") { field(FTVL, "LONG") field(NELM, "100") field(INP, {const:""}) }
record(aai, "AC") { field(FTVL, "CHAR") field(NELM, "8") field(INP, {const:""}) }
record(subArray, "SA") { field(FTVL, "CHAR") field(MALM, "8") field(NELM, "4") field(INP, {const:""}) }
record(ai, "AI") { field(INP, {const:""}) }
record(longin, "LI") { field(INP, {const:""}) }
record(stringin, "SI") { field(INP, {const:""}) field(VAL, "x") }
record(int64in, "QI") { field(INP, {const:""}) }
record(ao, "AO") { field(DOL, {const:""}) field(OMSL, "closed_loop") }
record(calc, "CA") { field(CALC, "A") field(INPA, {const:""}) field(A, "5") }
record(sel, "SE") { field(NVL, {const:""}) field(SELN, "3") }
"#;

#[epics_macros_rs::epics_test]
async fn an_empty_json_const_loads_like_any_string_lnkconst_c_494() {
    let (db, _h) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    let get = |pv: &str| db.get_pv(pv).ok();
    let f = |pv: &str| get(pv).and_then(|v| v.to_f64());
    let mut wrong = Vec::new();
    let mut check = |pv: &str, got: Option<EpicsValue>, c: EpicsValue| {
        if got.as_ref() != Some(&c) {
            wrong.push(format!("{pv}: got {got:?}, C {c:?}"));
        }
    };

    for (rec, val) in [
        ("E_CHAR", EpicsValue::CharArray(vec![0].into())),
        ("E_UCHAR", EpicsValue::UCharArray(vec![0].into())),
        ("E_STRING", EpicsValue::StringArray(vec!["".into()].into())),
        ("E_DOUBLE", EpicsValue::DoubleArray(vec![0.0].into())),
        ("E_LONG", EpicsValue::LongArray(vec![0].into())),
        ("AC", EpicsValue::CharArray(vec![0].into())),
        ("SA", EpicsValue::CharArray(vec![0].into())),
    ] {
        check(&format!("{rec}.VAL"), get(&format!("{rec}.VAL")), val);
        let nord = f(&format!("{rec}.NORD")).map(EpicsValue::Double);
        check(&format!("{rec}.NORD"), nord, EpicsValue::Double(1.0));
    }
    // UDF 0 wherever the dset or the record clears it on a successful load;
    // calc and sel seed an input, not VAL.
    for rec in [
        "E_CHAR", "E_STRING", "AC", "SA", "AI", "LI", "SI", "QI", "AO",
    ] {
        let udf = f(&format!("{rec}.UDF")).map(EpicsValue::Double);
        check(&format!("{rec}.UDF"), udf, EpicsValue::Double(0.0));
    }
    for (pv, c) in [
        ("AI.VAL", 0.0),
        ("LI.VAL", 0.0),
        ("QI.VAL", 0.0),
        ("AO.VAL", 0.0),
        ("CA.A", 0.0),
        ("SE.SELN", 0.0),
    ] {
        check(pv, f(pv).map(EpicsValue::Double), EpicsValue::Double(c));
    }
    check("SI.VAL", get("SI.VAL"), EpicsValue::String("".into()));
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
