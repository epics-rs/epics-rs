//! OLDSIMM, PUTF and PACT are `SPC_NOMOD`. C refuses a runtime put to them
//! only in `dbPutSpecial` pass 0 (`dbAccess.c:122-127`); the database's
//! NOMOD gate is that refusal, and `put_common_field` keeps no second one.

use std::collections::HashMap;

use epics_base_rs::error::CaError;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

#[epics_macros_rs::epics_test]
async fn oldsimm_putf_and_pact_puts_are_refused_by_the_gate() {
    let (db, _) = IocBuilder::new()
        .db_string("record(ai, \"R\") {\n}\n", &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    for field in ["OLDSIMM", "PUTF", "PACT"] {
        let before = db.get_pv(format!("R.{field}")).unwrap();
        let put = db
            .put_record_field_from_ca("R", field, EpicsValue::Long(1))
            .await;
        assert!(
            matches!(put, Err(CaError::ReadOnlyField(_))),
            "{field}: {put:?}"
        );
        assert_eq!(db.get_pv(format!("R.{field}")).unwrap(), before, "{field}");
    }
}

/// A declared NOMOD field the record keeps no member for is stored in the
/// declared-override map; a `.db` value loads there and the gate alone
/// refuses a client.
#[epics_macros_rs::epics_test]
async fn declared_override_nomod_puts_are_refused_by_the_gate() {
    for (rtype, field) in [("sub", "LA"), ("sub", "LU"), ("sel", "LA"), ("sel", "NLST")] {
        let (db, _) = IocBuilder::new()
            .db_string(&format!("record({rtype}, \"R\") {{\n}}\n"), &HashMap::new())
            .unwrap()
            .build()
            .await
            .unwrap();
        let before = db.get_pv(format!("R.{field}")).unwrap();
        let put = db
            .put_record_field_from_ca("R", field, EpicsValue::Double(7.0))
            .await;
        assert!(
            matches!(put, Err(CaError::ReadOnlyField(_))),
            "{rtype}.{field}: {put:?}"
        );
        assert_eq!(
            db.get_pv(format!("R.{field}")).unwrap(),
            before,
            "{rtype}.{field}"
        );
    }
}
