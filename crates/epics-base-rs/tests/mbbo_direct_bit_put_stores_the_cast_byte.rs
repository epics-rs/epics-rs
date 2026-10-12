//! A Bn put stores the DBF_UCHAR cast byte, which `special()` tests only for
//! nonzero (`mbboDirectRecord.c:281-287`); only `bitsFromVAL` in `process()`
//! normalises it. Measured on a disabled record, so no cycle runs:
//! oracleStdIoc reads `MBD.B0` back as 44, 254, 2, 255 after puts of 300,
//! -2.7, 2, -1, with VAL 1 after each.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(mbboDirect, "MBD") {
  field(DISV, "1")
  field(DISA, "1")
}
"#;

#[epics_macros_rs::epics_test]
async fn a_bit_put_reads_back_the_cast_byte_as_the_oracle_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    for (put, byte) in [(300.0, 44), (-2.7, 254), (2.0, 2), (-1.0, 255)] {
        db.put_record_field_from_ca("MBD", "B0", EpicsValue::Double(put))
            .await
            .unwrap();
        assert_eq!(
            db.get_pv("MBD.B0").unwrap(),
            EpicsValue::UChar(byte),
            "B0 <- {put}"
        );
        assert_eq!(
            db.get_pv("MBD.VAL").unwrap(),
            EpicsValue::Long(1),
            "VAL after B0 <- {put}"
        );
    }
}
