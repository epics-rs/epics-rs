//! scaler's `SPC_NOMOD` fields: `put_field` stores them like any field, as
//! C's `dbPutString` does at load, and a runtime put is refused by the
//! database's NOMOD gate alone, as C's `dbPutSpecial` pass 0 refuses it
//! (`dbAccess.c:122-127`).

use std::collections::HashMap;

use epics_base_rs::error::CaError;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::Record;
use scaler_rs::ScalerRecord;

const NOMOD: [&str; 9] = ["PCNT", "SS", "US", "NCH", "T", "VERS", "S1", "S32", "S64"];

#[test]
fn put_field_stores_a_nomod_field() {
    let mut rec = ScalerRecord::default();
    for field in NOMOD {
        let value = rec.get_field(field).unwrap();
        rec.put_field(field, value)
            .unwrap_or_else(|e| panic!("scaler.{field}: {e}"));
    }
}

#[epics_base_rs::epics_test]
async fn a_runtime_put_to_a_nomod_field_is_refused_by_the_gate() {
    let (db, _) = IocBuilder::new()
        .register_record_type("scaler", || Box::new(ScalerRecord::default()))
        .db_string("record(scaler, \"R\") {\n}\n", &HashMap::new())
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
            "scaler.{field}: check_no_mod must refuse it"
        );
        let put = db
            .put_record_field_from_ca("R", field, before.clone())
            .await;
        assert!(
            matches!(put, Err(CaError::ReadOnlyField(_))),
            "scaler.{field}: {put:?}"
        );
        assert_eq!(db.get_pv(format!("R.{field}")).unwrap(), before);
    }
}
