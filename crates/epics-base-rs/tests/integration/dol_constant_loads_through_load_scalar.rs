//! `recGblInitConstantLink` is `!dbLoadLink(...)` (`recGbl.c:175`), the
//! link's own `loadScalar`: `cvt_st_*` over a plain constant's text, which
//! parses the integer prefix (`dbConstLink.c:54-66`), and `lnkConst`'s cast
//! for a `{const: …}`. softIoc 8f5015b66, `dbgf` after `iocInit`:
//!
//! ```text
//! longout  DOL "5e9"               5, UDF 0
//! int64out DOL "1e19"              1, UDF 0
//! int64out DOL "-1e19"             -1, UDF 0
//! longout  DOL "0x10"              16, UDF 0
//! longout  DOL {const:5e9}         -2147483648, UDF 0
//! longout  DOL {const:3000000000}  -1294967296, UDF 0
//! bo       DOL "65537"             0, UDF 1   (epicsParseUInt16 refuses)
//! ```

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;

const DB: &str = r#"
record(longout, "LO") { field(DOL, "5e9") }
record(int64out, "QO") { field(DOL, "1e19") }
record(int64out, "QJ") { field(DOL, "-1e19") }
record(longout, "LH") { field(DOL, "0x10") }
record(longout, "LJ") { field(DOL, {const:5e9}) }
record(longout, "LK") { field(DOL, {const:3000000000}) }
record(bo, "BO") { field(DOL, "65537") }
"#;

#[epics_macros_rs::epics_test]
async fn a_dol_constant_loads_as_its_link_s_load_scalar() {
    let (db, _h) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let got: Vec<_> = ["LO", "QO", "QJ", "LH", "LJ", "LK", "BO"]
        .iter()
        .map(|r| (db.get_pv(r).ok(), db.get_pv(format!("{r}.UDF")).ok()))
        .collect();
    let udf = |u| Some(EpicsValue::UChar(u));
    assert_eq!(
        got,
        [
            (Some(EpicsValue::Long(5)), udf(0)),
            (Some(EpicsValue::Int64(1)), udf(0)),
            (Some(EpicsValue::Int64(-1)), udf(0)),
            (Some(EpicsValue::Long(16)), udf(0)),
            (Some(EpicsValue::Long(i32::MIN)), udf(0)),
            (Some(EpicsValue::Long(-1294967296)), udf(0)),
            (Some(EpicsValue::Enum(0)), udf(1)),
        ]
    );
}
