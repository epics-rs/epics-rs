//! aSub seeds INPA..U through `dbLoadLinkArray(plink, FTx, a, &n)` with
//! `n = NOx`, keeping NEx unless `n > 0` (`aSubRecord.c:128-137`), not
//! `recGblInitConstantLink`. oracleStdIoc R7.0.10.1, `dbgf` after `iocInit`.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;

const DB: &str = r#"
record(aSub, "AS") {
  field(FTA, "CHAR") field(NOA, "10") field(INPA, {const:"hello"})
  field(FTB, "LONG") field(NOB, "4") field(INPB, {const:[1,2,3]})
  field(FTC, "STRING") field(NOC, "2") field(INPC, {const:[1.5,2]})
  field(FTD, "DOUBLE") field(INPD, "3.5")
  field(FTE, "STRING") field(INPE, {const:"abc"})
  field(FTF, "CHAR") field(NOF, "4") field(INPF, {const:""})
  field(FTG, "LONG") field(NOG, "3") field(INPG, {const:[]})
  field(FTH, "DOUBLE") field(NOH, "3") field(INPH, "[4,5]")
  field(FTI, "STRING") field(NOI, "3") field(INPI, {const:["x","y"]})
  field(FTJ, "CHAR") field(NOJ, "3") field(INPJ, {const:"hello"})
  field(FTK, "DOUBLE") field(NOK, "2") field(INPK, {const:[1,2,3]})
  field(FTL, "LONG") field(INPL, {const:7.9})
  field(FTM, "LONG") field(NOM, "3") field(INPM, "[]")
}
"#;

#[epics_macros_rs::epics_test]
async fn asub_inputs_load_through_db_load_link_array_asubrecord_c_134() {
    let (db, _h) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    let strings = |v: &[&str]| EpicsValue::StringArray(v.iter().map(|s| (*s).into()).collect());
    let expect: Vec<(&str, EpicsValue, f64)> = vec![
        ("A", EpicsValue::CharArray(b"hello\0".to_vec().into()), 6.0),
        ("B", EpicsValue::LongArray(vec![1, 2, 3].into()), 3.0),
        ("C", strings(&["1.500000", "2.000000"]), 2.0),
        ("D", EpicsValue::Double(3.5), 1.0),
        ("E", EpicsValue::String("abc".into()), 1.0),
        ("F", EpicsValue::CharArray(vec![0].into()), 1.0),
        // `[]` loads nothing: the zeroed NOx buffer, NEx still NOx.
        ("G", EpicsValue::LongArray(vec![0, 0, 0].into()), 3.0),
        ("H", EpicsValue::DoubleArray(vec![4.0, 5.0].into()), 2.0),
        ("I", strings(&["x", "y"]), 2.0),
        ("J", EpicsValue::CharArray(b"he\0".to_vec().into()), 3.0),
        ("K", EpicsValue::DoubleArray(vec![1.0, 2.0].into()), 2.0),
        ("L", EpicsValue::Long(7), 1.0),
        ("M", EpicsValue::LongArray(vec![0, 0, 0].into()), 3.0),
    ];

    let mut wrong = Vec::new();
    for (ch, val, ne) in expect {
        let got = (
            db.get_pv(&format!("AS.{ch}")).ok(),
            db.get_pv(&format!("AS.NE{ch}"))
                .ok()
                .and_then(|v| v.to_f64()),
        );
        if got != (Some(val.clone()), Some(ne)) {
            wrong.push(format!("{ch}: got {got:?}, C {:?}", (val, ne)));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
