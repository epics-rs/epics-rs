//! motor's `SPC_NOMOD` fields: `put_field` stores them like any field, as
//! C's `dbPutString` does at load, and a runtime put is refused by the
//! database's NOMOD gate alone with `S_db_noMod`, as C's `dbPutSpecial`
//! pass 0 refuses it (`dbAccess.c:122-127`).

use std::collections::HashMap;

use epics_base_rs::error::CaError;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::Record;
use motor_rs::MotorRecord;

/// The `special(SPC_NOMOD)` value fields of `motorRecord.dbd`.
const NOMOD: [&str; 36] = [
    "VERS", "CARD", "RHLM", "RLLM", "HLS", "LLS", "RHLS", "RLLS", "RCNT", "MISS", "LSPG", "LVAL",
    "LDVL", "LRVL", "LRLV", "RBV", "DRBV", "DIFF", "RDIF", "CDIR", "RRBV", "RMP", "REP", "RVEL",
    "DMOV", "MOVN", "MSTA", "LVIO", "TDIR", "ATHM", "PP", "MIP", "MMAP", "NMAP", "ALST", "MLST",
];

#[test]
fn put_field_stores_a_nomod_field() {
    let mut rec = MotorRecord::new();
    for field in NOMOD {
        let value = rec.get_field(field).unwrap();
        rec.put_field(field, value)
            .unwrap_or_else(|e| panic!("motor.{field}: {e}"));
    }
}

#[epics_base_rs::epics_test]
async fn a_runtime_put_to_a_nomod_field_is_refused_by_the_gate() {
    let (db, _) = IocBuilder::new()
        .register_record_type("motor", || Box::new(MotorRecord::new()))
        .db_string("record(motor, \"R\") {\n}\n", &HashMap::new())
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
            "motor.{field}: check_no_mod must refuse it"
        );
        let put = db
            .put_record_field_from_ca("R", field, before.clone())
            .await;
        assert!(
            matches!(put, Err(CaError::ReadOnlyField(_))),
            "motor.{field}: {put:?}"
        );
        // Debug text, so an unset NaN compares equal to itself.
        assert_eq!(
            format!("{:?}", db.get_pv(format!("R.{field}")).unwrap()),
            format!("{before:?}")
        );
    }
}
