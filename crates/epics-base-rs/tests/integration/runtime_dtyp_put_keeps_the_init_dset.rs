//! C binds `prec->dset` from DTYP once, at iocInit (`iocInit.c:529`); a later
//! DTYP put stores the menu index and the running record keeps its dset.
//! softIocPVX (R7.0.10) with the DB below, every DTYP put to index 1 ("Raw
//! Soft Channel"), then `M/A/B.PROC` and `MO.VAL 22`: M VAL 22 RVAL 0 MASK 7,
//! MO SINK 22, A VAL 22 RVAL 0, B VAL 22 RVAL 0, every DTYP reads back 1.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;

const DB: &str = r#"
record(longin, "SRC") { field(VAL, "22") }
record(mbbi, "M") { field(DTYP, "Soft Channel") field(NOBT, "3") field(SHFT, "1") field(INP, "SRC") }
record(mbbo, "MO") { field(DTYP, "Soft Channel") field(NOBT, "3") field(SHFT, "1") field(OUT, "SINK") }
record(ai, "A") { field(DTYP, "Soft Channel") field(LINR, "NO CONVERSION") field(ASLO, "2") field(INP, "SRC") }
record(bi, "B") { field(DTYP, "Soft Channel") field(INP, "SRC") }
record(longin, "SINK") { }
"#;

#[epics_macros_rs::epics_test]
async fn a_runtime_dtyp_put_keeps_the_init_dset_iocinit_529() {
    let (db, _h) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let field = |rec: &str, f: &str| db.get_pv(format!("{rec}.{f}")).ok();

    for rec in ["M", "MO", "A", "B"] {
        db.put_record_field_from_ca_no_notify(rec, "DTYP", EpicsValue::Enum(1))
            .await
            .unwrap();
    }
    for rec in ["M", "A", "B"] {
        db.put_record_field_from_ca_no_notify(rec, "PROC", EpicsValue::Short(1))
            .await
            .unwrap();
    }
    db.put_record_field_from_ca_no_notify("MO", "VAL", EpicsValue::Enum(22))
        .await
        .unwrap();

    for rec in ["M", "MO", "A", "B"] {
        assert_eq!(field(rec, "DTYP"), Some(EpicsValue::Enum(1)), "{rec}");
    }
    assert_eq!(field("M", "VAL"), Some(EpicsValue::Enum(22)));
    assert_eq!(field("M", "RVAL"), Some(EpicsValue::ULong(0)));
    assert_eq!(field("M", "MASK"), Some(EpicsValue::ULong(7)));
    assert_eq!(field("SINK", "VAL"), Some(EpicsValue::Long(22)));
    assert_eq!(field("A", "VAL"), Some(EpicsValue::Double(22.0)));
    assert_eq!(field("A", "RVAL"), Some(EpicsValue::Long(0)));
    assert_eq!(field("B", "VAL"), Some(EpicsValue::Enum(22)));
    assert_eq!(field("B", "RVAL"), Some(EpicsValue::ULong(0)));
}
