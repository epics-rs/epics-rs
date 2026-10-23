//! mbbiDirect MLST and mbboDirect MLST/OBIT are `SPC_NOMOD`. C's
//! `dbPutString` stores a `.db` value and only `dbPutSpecial`'s pass 0
//! refuses a runtime put (`dbAccess.c:122-127`). `put_field` stores them like
//! any field; the refusal belongs to the database's NOMOD gate alone.

use std::collections::HashMap;

use epics_base_rs::error::CaError;
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
        let mut rec = create_record(rtype).unwrap();
        rec.put_field(field, EpicsValue::Long(5))
            .unwrap_or_else(|e| panic!("{rtype}.{field}: put_field must store it: {e}"));
        assert_eq!(rec.get_field(field).and_then(|v| v.to_f64()), Some(5.0));

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

#[epics_macros_rs::epics_test]
async fn a_runtime_put_to_a_nomod_field_is_refused_by_the_gate() {
    for (rtype, field) in CASES {
        let (db, _) = IocBuilder::new()
            .db_string(&format!("record({rtype}, \"R\") {{\n}}\n"), &HashMap::new())
            .unwrap()
            .build()
            .await
            .unwrap();
        let before = db.get_pv(format!("R.{field}")).unwrap();
        assert!(
            matches!(
                db.check_external_put_preconditions("R", field).await,
                Err(CaError::ReadOnlyField(_))
            ),
            "{rtype}.{field}: check_no_mod must refuse it"
        );
        let put = db
            .put_record_field_from_ca("R", field, EpicsValue::Long(7))
            .await;
        assert!(
            matches!(put, Err(CaError::ReadOnlyField(_))),
            "{rtype}.{field}: {put:?}"
        );
        assert_eq!(db.get_pv(format!("R.{field}")).unwrap(), before);
    }
}
