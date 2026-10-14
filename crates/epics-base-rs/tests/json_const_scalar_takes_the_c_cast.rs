//! A `{const: …}` scalar load is `lnkConst_loadScalar` (`lnkConst.c:358-417`):
//! the JSON value through `dbFastPutConvertRoutine[its own type][dbrType]`,
//! never through the text. softIoc 8f5015b66, `dbgf` after `iocInit`:
//!
//! ```text
//! longin  INP {const:5e9}         -2147483648   (cvt_d_l, x86 cast)
//! int64in INP {const:1e19}        -9223372036854775808
//! longin  INP {const:3000000000}  -1294967296   (cvt_q_l, low word)
//! longin  INP {const:"12abc"}     12            (cvt_st_l)
//! ```

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;

const DB: &str = r#"
record(longin, "LI") { field(INP, {const:5e9}) }
record(int64in, "QI") { field(INP, {const:1e19}) }
record(longin, "LB") { field(INP, {const:3000000000}) }
record(longin, "LS") { field(INP, {const:"12abc"}) }
"#;

#[epics_macros_rs::epics_test]
async fn a_json_const_scalar_converts_by_its_own_type() {
    let (db, _h) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let got: Vec<_> = ["LI", "QI", "LB", "LS"]
        .iter()
        .map(|r| (db.get_pv(r).ok(), db.get_pv(format!("{r}.UDF")).ok()))
        .collect();
    let udf0 = Some(EpicsValue::UChar(0));
    assert_eq!(
        got,
        [
            (Some(EpicsValue::Long(i32::MIN)), udf0.clone()),
            (Some(EpicsValue::Int64(i64::MIN)), udf0.clone()),
            (Some(EpicsValue::Long(-1294967296)), udf0.clone()),
            (Some(EpicsValue::Long(12)), udf0),
        ]
    );
}
