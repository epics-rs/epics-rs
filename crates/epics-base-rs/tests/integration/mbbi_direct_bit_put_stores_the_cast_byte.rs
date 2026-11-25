//! A Bn put stores the DBF_UCHAR cast byte; only `monitor()`'s re-derive
//! (`mbbiDirectRecord.c:221`) normalises it, and VAL never follows a bit.
//! Measured on a disabled record through an ao OUT link (a DBR_DOUBLE put):
//! softIoc reads `MBI.B0` back as 44, 254, 2, 255 after 300, -2.7, 2, -1,
//! with VAL 0 after each.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(mbbiDirect, "MBI") {
  field(DISV, "1")
  field(DISA, "1")
}
"#;

#[epics_macros_rs::epics_test]
async fn a_bit_put_reads_back_the_cast_byte_as_softioc_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    for (put, byte) in [(300.0, 44), (-2.7, 254), (2.0, 2), (-1.0, 255)] {
        db.put_record_field_from_ca("MBI", "B0", EpicsValue::Double(put))
            .await
            .unwrap();
        assert_eq!(
            db.get_pv("MBI.B0").unwrap(),
            EpicsValue::UChar(byte),
            "B0 <- {put}"
        );
        assert_eq!(
            db.get_pv("MBI.VAL").unwrap(),
            EpicsValue::Long(0),
            "VAL after B0 <- {put}"
        );
    }
}
