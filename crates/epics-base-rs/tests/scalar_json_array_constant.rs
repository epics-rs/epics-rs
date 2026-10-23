//! A soft dset's `recGblInitConstantLink` on a `[…]` CONSTANT runs
//! `dbPutConvertJSON(text, dbrType, buf, &1)` (`dbConstLink.c:163-168`):
//! element 0 converted into the dset's type, a string element refused unless
//! that type is `DBF_STRING`, and `[]` a success that writes nothing. Each
//! expectation is softIoc 7.0.10's `dbgf` VAL / UDF after `iocInit`.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ai,"C1"){field(INP,["5"])}
record(ai,"C2"){field(INP,[7])}
record(ai,"C3"){field(INP,[7,8])}
record(ai,"C4"){field(INP,[])}
record(longin,"C5"){field(INP,[1.9])}
record(mbbi,"C6"){field(INP,[2])}
record(stringin,"C7"){field(INP,["s1","s2"])}
record(stringin,"C8"){field(INP,[3])}
record(longin,"C9"){field(INP,[1e400])}
"#;

#[epics_macros_rs::epics_test]
async fn a_bracketed_constant_loads_element_zero_through_db_put_convert_json() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let loaded = |rec: &str| {
        let inst = db.get_record(rec).unwrap_or_else(|| panic!("{rec}"));
        let inst = inst.read();
        (inst.record.get_field("VAL"), inst.common.udf)
    };
    for (rec, val, udf) in [
        ("C1", EpicsValue::Double(0.0), 1),
        ("C2", EpicsValue::Double(7.0), 0),
        ("C3", EpicsValue::Double(7.0), 0),
        ("C4", EpicsValue::Double(0.0), 0),
        ("C5", EpicsValue::Long(1), 0),
        ("C6", EpicsValue::Enum(2), 0),
        ("C7", EpicsValue::String("s1".into()), 0),
        ("C8", EpicsValue::String("3".into()), 0),
        ("C9", EpicsValue::Long(0), 1),
    ] {
        assert_eq!(loaded(rec), (Some(val), udf), "{rec}");
    }
}
