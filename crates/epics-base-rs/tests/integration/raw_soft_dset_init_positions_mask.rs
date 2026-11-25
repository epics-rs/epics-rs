//! The multibit `devXxxSoftRaw::init_record` stores its mask in MASK
//! (`devMbbiSoftRaw.c:45-48`, `devMbbiDirectSoftRaw.c:45-48`,
//! `devMbboSoftRaw.c:28-31`, `devMbboDirectSoftRaw.c:28-31`): NOBT 0 overrides
//! a configured MASK, then `mask <<= shft`. softIocPVX (R7.0.10) with the DB
//! below reads MASK 14 / 4294967295 / 14 / 14 / 4294967295 / 7, and after the
//! processes RVAL 14 / 255 / 14 and SINK 14.

use epics_base_rs::server::database::ProcStack;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;

const DB: &str = r#"
record(longin, "SRC") { field(VAL, "255") }
record(mbbi, "I1") { field(DTYP, "Raw Soft Channel") field(NOBT, "3") field(SHFT, "1") field(INP, "SRC") }
record(mbbi, "I0") { field(DTYP, "Raw Soft Channel") field(MASK, "5") field(NOBT, "0") field(INP, "SRC") }
record(mbbiDirect, "J1") { field(DTYP, "Raw Soft Channel") field(NOBT, "3") field(SHFT, "1") field(INP, "SRC") }
record(mbbo, "O1") { field(DTYP, "Raw Soft Channel") field(NOBT, "3") field(SHFT, "1") field(OUT, "SINK") }
record(mbboDirect, "P0") { field(DTYP, "Raw Soft Channel") field(MASK, "5") field(NOBT, "0") field(OUT, "SINK2") }
record(mbbi, "S1") { field(DTYP, "Soft Channel") field(NOBT, "3") field(SHFT, "1") field(INP, "SRC") }
record(longin, "SINK") { }
record(longin, "SINK2") { }
"#;

#[epics_macros_rs::epics_test]
async fn raw_soft_dset_init_positions_mask_devmbbisoftraw_45() {
    let (db, _h) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let field = |rec: &str, f: &str| db.get_record(rec).unwrap().read().record.get_field(f);

    for (rec, mask) in [
        ("I1", 14),
        ("I0", u32::MAX),
        ("J1", 14),
        ("O1", 14),
        ("P0", u32::MAX),
        ("S1", 7),
    ] {
        assert_eq!(field(rec, "MASK"), Some(EpicsValue::ULong(mask)), "{rec}");
    }

    for rec in ["I1", "I0", "J1"] {
        db.process_record_with_links(rec, &mut ProcStack::new())
            .await
            .unwrap();
    }
    assert_eq!(field("I1", "RVAL"), Some(EpicsValue::ULong(14)));
    assert_eq!(field("I0", "RVAL"), Some(EpicsValue::ULong(255)));
    assert_eq!(field("J1", "RVAL"), Some(EpicsValue::ULong(14)));

    db.put_record_field_from_ca_no_notify("O1", "VAL", EpicsValue::Enum(7))
        .await
        .unwrap();
    assert_eq!(field("SINK", "VAL"), Some(EpicsValue::Long(14)));
}
