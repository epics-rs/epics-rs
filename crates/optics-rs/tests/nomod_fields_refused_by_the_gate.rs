//! table's `SPC_NOMOD` fields: `put_field` stores them like any field, as
//! C's `dbPutString` does at load, and a runtime put is refused by the
//! database's NOMOD gate alone, as C's `dbPutSpecial` pass 0 refuses it
//! (`dbAccess.c:122-127`).

use std::collections::HashMap;

use epics_base_rs::error::CaError;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::Record;
use optics_rs::records::table::TableRecord;

/// The `special(SPC_NOMOD)` value fields of `tableRecord.dbd`.
const NOMOD: [&str; 70] = [
    "VERS", "AXL", "AYL", "AZL", "XL", "YL", "ZL", "AXRB", "AYRB", "AZRB", "XRB", "YRB", "ZRB",
    "EAX", "EAY", "EAZ", "EX", "EY", "EZ", "HLAX", "HLAY", "HLAZ", "HLX", "HLY", "HLZ", "LLAX",
    "LLAY", "LLAZ", "LLX", "LLY", "LLZ", "M0X", "M0Y", "M1Y", "M2X", "M2Y", "M2Z", "R0X", "R0Y",
    "R1Y", "R2X", "R2Y", "R2Z", "E0X", "E0Y", "E1Y", "E2X", "E2Y", "E2Z", "V0X", "V0Y", "V1Y",
    "V2X", "V2Y", "V2Z", "H0X", "H0Y", "H1Y", "H2X", "H2Y", "H2Z", "L0X", "L0Y", "L1Y", "L2X",
    "L2Y", "L2Z", "LVIO", "MMAP", "TORAD",
];

#[test]
fn put_field_stores_a_nomod_field() {
    let mut rec = TableRecord::default();
    for field in NOMOD {
        let value = rec.get_field(field).unwrap();
        rec.put_field(field, value)
            .unwrap_or_else(|e| panic!("table.{field}: {e}"));
    }
}

#[epics_base_rs::epics_test]
async fn a_runtime_put_to_a_nomod_field_is_refused_by_the_gate() {
    let (db, _) = IocBuilder::new()
        .register_record_type("table", || Box::new(TableRecord::default()))
        .db_string("record(table, \"R\") {\n}\n", &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    for field in NOMOD {
        let before = db.get_pv(format!("R.{field}")).unwrap();
        assert!(
            matches!(
                db.check_external_put_preconditions("R", field).await,
                Err(CaError::ReadOnlyField(_))
            ),
            "table.{field}: check_no_mod must refuse it"
        );
        let put = db
            .put_record_field_from_ca("R", field, before.clone())
            .await;
        assert!(
            matches!(put, Err(CaError::ReadOnlyField(_))),
            "table.{field}: {put:?}"
        );
        // Debug text, so an unset NaN compares equal to itself.
        assert_eq!(
            format!("{:?}", db.get_pv(format!("R.{field}")).unwrap()),
            format!("{before:?}")
        );
    }
}
