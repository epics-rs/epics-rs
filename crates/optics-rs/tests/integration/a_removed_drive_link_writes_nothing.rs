//! A refused `M0XL` put removes the link before `SPC_NOMOD` refuses it
//! (`dbAccess.c:1149`, `:1167-1174`): the table keeps driving the axis, but
//! `dbPutLink` on a link with no lset is `S_db_noLSET` with no alarm
//! (`dbLink.c:440-441`), so the motor is not written, and the field reads
//! back with the cleared `pvlMask` (`dbDbLink.c:138`).
//!
//! oracleTableIoc (base 8f5015b66, optics `tableRecord.c`) on this database:
//!
//! ```text
//! epics> dbpf T.X 1          -> MX 1
//! epics> dbpf MX 7
//! epics> dbpf T.M0XL "MX"
//! recGblDbaddrError: dbPut Attempt to modify noMod field PV: T.M0XL
//! DBF_STRING:         "MX NPP NMS"
//! epics> dbpf T.X 2          -> MX 7, T.STAT NO_ALARM, T.SEVR NO_ALARM
//! ```

// RTEMS-EXEC-MODEL-ALLOW(1): no CA server is built; the reactor is
// `#[tokio::test]`'s own, which the exec backend does not remove.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;
use optics_rs::records::table::TableRecord;

const DB: &str = r#"
record(ao, "MX") { field(VAL, "0") }
record(table, "T") {
    field(LX, "100")
    field(LZ, "100")
    field(M0XL, "MX PP MS")
    field(R0XI, "MX")
}
"#;

#[tokio::test]
async fn a_removed_m0xl_leaves_the_motor_and_reads_back_npp_nms() {
    let (db, _) = IocBuilder::new()
        .register_record_type("table", || Box::new(TableRecord::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let get = |pv: &str| db.get_pv(pv).unwrap();

    db.put_record_field_from_ca("T", "X", EpicsValue::Double(1.0))
        .await
        .unwrap();
    assert_eq!(get("MX").to_f64(), Some(1.0), "a live M0XL drives MX");

    db.put_pv("MX", EpicsValue::Double(7.0)).await.unwrap();
    assert!(
        db.put_record_field_from_ca_no_notify("T", "M0XL", EpicsValue::String("MX".into()))
            .await
            .is_err(),
        "M0XL is SPC_NOMOD"
    );
    assert_eq!(get("T.M0XL"), EpicsValue::String("MX NPP NMS".into()));

    db.put_record_field_from_ca("T", "X", EpicsValue::Double(2.0))
        .await
        .unwrap();
    assert_eq!(
        get("MX").to_f64(),
        Some(7.0),
        "the removed link writes nothing"
    );
    assert_eq!(get("T.STAT").to_f64(), Some(0.0), "and raises nothing");
    assert_eq!(get("T.SEVR").to_f64(), Some(0.0));
}
