//! A bare `[…]` is a CONSTANT link (`dbStaticLib.c:2353`), and
//! `dbConstLoadLS` hands every CONSTANT text to `dbLSConvertJSON`
//! (`dbConstLink.c:178-191`): the first string lands, `[]` and a pending
//! number give LEN 1, and any other value refuses the load. Each expectation
//! is softIoc 7.0.10's `dbgf` VAL / LEN / UDF after `iocInit`.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(lso,"O1"){field(DOL,["hello"])}
record(lso,"O2"){field(DOL,["first", "second"])}
record(lso,"O3"){field(DOL,[1, 2])}
record(lso,"O4"){field(DOL,[])}
record(lso,"O5"){field(DOL,["a\"b"])}
record(lso,"O6"){field(DOL,["x", 3])}
record(lso,"O7"){field(DOL,["a,b"])}
record(lso,"O8"){field(DOL,['q'])}
record(lso,"O9"){field(DOL,[null])}
record(lso,"OA"){field(DOL,[["n"]])}
record(lso,"N1"){field(DOL,"Infinity")}
record(lso,"N2"){field(DOL,"inf")}
record(lso,"N3"){field(DOL,"0x1p4")}
record(lso,"N4"){field(DOL,"0x10")}
record(lso,"N5"){field(DOL,"1.5")}
record(lso,"J1"){field(DOL,{const:["a,b"]})}
record(lso,"J2"){field(DOL,{const:[]})}
record(lsi,"I1"){field(INP,["hello"])}
record(lsi,"I2"){field(INP,["first", "second"])}
record(lsi,"I3"){field(INP,[1, 2])}
record(lsi,"I4"){field(INP,[])}
record(lsi,"I6"){field(INP,["x", 3])}
"#;

fn loaded(db: &epics_base_rs::server::database::PvDatabase, rec: &str) -> (String, u32, u8) {
    let inst = db.get_record(rec).unwrap_or_else(|| panic!("{rec}"));
    let inst = inst.read();
    let val = match inst.record.get_field("VAL") {
        Some(EpicsValue::String(s)) => s.as_str_lossy().into_owned(),
        Some(EpicsValue::CharArray(b)) => {
            String::from_utf8_lossy(b.split(|&c| c == 0).next().unwrap_or_default()).into_owned()
        }
        other => panic!("{rec}.VAL: {other:?}"),
    };
    let len = match inst.record.get_field("LEN") {
        Some(EpicsValue::ULong(n)) => n,
        other => panic!("{rec}.LEN: {other:?}"),
    };
    (val, len, inst.common.udf)
}

#[epics_macros_rs::epics_test]
async fn a_bracketed_constant_loads_through_db_ls_convert_json() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let text = |v: &str, len| (v.to_string(), len, 0u8);
    let refused = (String::new(), 0, 1u8);
    for (rec, want) in [
        ("O1", text("hello", 6)),
        ("O2", text("first", 6)),
        ("O3", refused.clone()),
        ("O4", text("", 1)),
        ("O5", text("a\"b", 4)),
        ("O6", refused.clone()),
        ("O7", text("a,b", 4)),
        ("O8", text("q", 2)),
        ("O9", refused.clone()),
        ("OA", refused.clone()),
        // A CONSTANT number the yajl lexer completes is a `dblsj_number`
        // refusal; one it leaves pending loads LEN 1.
        ("N1", refused.clone()),
        ("N2", refused.clone()),
        ("N3", refused.clone()),
        ("N4", text("", 1)),
        ("N5", text("", 1)),
        ("J1", text("a,b", 4)),
        ("J2", refused.clone()),
        ("I1", text("hello", 6)),
        ("I2", text("first", 6)),
        ("I3", refused.clone()),
        ("I4", text("", 1)),
        ("I6", refused.clone()),
    ] {
        assert_eq!(loaded(&db, rec), want, "{rec}");
    }
}
