//! mbbiDirect MLST and mbboDirect MLST/OBIT are `SPC_NOMOD` fields whose
//! `put_field` refuses them, yet C's `dbPutString` stores a `.db` value.
//! The load reaches them through `Record::load_field`, on a new record and
//! on a merged one; routed through `put_field`, it fails.

use std::collections::HashMap;

use epics_base_rs::server::db_loader::create_record;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const CASES: [(&str, &str); 3] = [
    ("mbbiDirect", "MLST"),
    ("mbboDirect", "MLST"),
    ("mbboDirect", "OBIT"),
];

#[epics_macros_rs::epics_test]
async fn a_loaded_nomod_field_reads_back() {
    for (rtype, field) in CASES {
        let refused = create_record(rtype)
            .unwrap()
            .put_field(field, EpicsValue::Long(3));
        assert!(
            refused.is_err(),
            "{rtype}.{field}: put_field must refuse it"
        );

        let new = format!("record({rtype}, \"R\") {{\n  field({field}, \"3\")\n}}\n");
        let merged = format!("record({rtype}, \"R\") {{\n}}\n{new}");
        for db in [new, merged] {
            let got = IocBuilder::new()
                .db_string(&db, &HashMap::new())
                .unwrap()
                .load()
                .await
                .unwrap_or_else(|e| panic!("{rtype}.{field}: {e}\n{db}"))
                .get_pv(format!("R.{field}"))
                .unwrap();
            assert_eq!(
                got.to_f64(),
                Some(3.0),
                "{rtype}.{field} read {got:?}\n{db}"
            );
        }
    }
}
