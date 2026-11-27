//! asynRecord's ERRS is `SPC_NOMOD` through `cvt_dbaddr`
//! (asynRecord.c:962): the database's NOMOD gate alone refuses a client
//! put, as C's `dbPutSpecial` pass 0 does (`dbAccess.c:122-127`).

use std::collections::HashMap;

use asyn_rs::asyn_record::AsynRecord;
use epics_base_rs::error::CaError;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

#[epics_base_rs::epics_test]
async fn a_runtime_errs_put_is_refused_by_the_gate() {
    let (db, _) = IocBuilder::new()
        .register_record_type("asyn", || Box::new(AsynRecord::default()))
        .db_string("record(asyn, \"A\") {\n}\n", &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let before = db.get_pv("A.ERRS").unwrap();
    assert!(matches!(
        db.check_external_put_preconditions("A", "ERRS").await,
        Err(CaError::ReadOnlyField(_))
    ));
    let put = db
        .put_record_field_from_ca(
            "A",
            "ERRS",
            EpicsValue::CharArray(b"operator".to_vec().into()),
        )
        .await;
    assert!(matches!(put, Err(CaError::ReadOnlyField(_))), "{put:?}");
    assert_eq!(db.get_pv("A.ERRS").unwrap(), before);
}
