//! A refused `SIML` put removes the link before `SPC_NOMOD` refuses it
//! (`dbAccess.c:1149`, `:1167-1174`). `mcaRecord.c:1106` reads SIML with
//! `dbGetLink`, which answers a link with no lset with a silent -1
//! (`dbLink.c:335-337`), so the record raises nothing and SIMM is never read
//! again; the field reads back with the cleared `pvlMask` (`dbDbLink.c:138`).
//!
//! A scratch C IOC (base 8f5015b66 plus mca's `mcaRecord.c` and
//! `device(mca,CONSTANT,devMCA_soft,"Soft Channel")`) on this database:
//!
//! ```text
//! epics> dbgf MA.SIML        -> "SM PP MS"
//! epics> dbpf MA.SIML "SM"
//! recGblDbaddrError: dbPut Attempt to modify noMod field PV: MA.SIML
//! DBF_STRING:         "SM NPP NMS"
//! epics> dbpf MA.PROC 1      -> MA.STAT NO_ALARM, MA.SEVR NO_ALARM
//! epics> dbpf SM 1
//! epics> dbpf MA.PROC 1      -> MA.SIMM NO, MA.STAT NO_ALARM, MA.SEVR NO_ALARM
//! ```

// RTEMS-EXEC-MODEL-ALLOW(1): no CA server is built; the reactor is
// `#[tokio::test]`'s own, which the exec backend does not remove.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, register_device_row};
use epics_base_rs::types::EpicsValue;

/// `mcaSupport.dbd`'s `device(mca,CONSTANT,devMCA_soft,"Soft Channel")`.
static SOFT_MCA: DevSup = DevSup {
    name: "devMCA_soft",
    choice: "Soft Channel",
    link_type: DbLinkType::Constant,
    dset: Dset::new(6, &[2, 4, 5], false),
};

const DB: &str = r#"
record(waveform, "WF") { field(NELM, "4") field(FTVL, "LONG") }
record(bo, "SM") { field(VAL, "0") }
record(mca, "MA") {
    field(DTYP, "Soft Channel")
    field(INP, "WF")
    field(NMAX, "4")
    field(SIML, "SM PP MS")
}
"#;

#[tokio::test]
async fn a_removed_siml_is_a_silent_failed_read() {
    let (name, factory) = mca_rs::mca_record_factory();
    register_device_row("mca", &SOFT_MCA);
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let get = |pv: &str| db.get_pv(pv).unwrap();
    let process = || async {
        let mut visited = epics_base_rs::server::database::ProcStack::new();
        db.process_record_with_links("MA", &mut visited)
            .await
            .unwrap();
    };

    assert_eq!(get("MA.SIML"), EpicsValue::String("SM PP MS".into()));
    assert!(
        db.put_record_field_from_ca_no_notify("MA", "SIML", EpicsValue::String("SM".into()))
            .await
            .is_err(),
        "SIML is SPC_NOMOD"
    );
    assert_eq!(get("MA.SIML"), EpicsValue::String("SM NPP NMS".into()));

    process().await;
    assert_eq!(
        get("MA.STAT").to_f64(),
        Some(0.0),
        "dbGetLink raises nothing"
    );
    assert_eq!(get("MA.SEVR").to_f64(), Some(0.0));

    db.put_pv("SM", EpicsValue::Short(1)).await.unwrap();
    process().await;
    assert_eq!(get("MA.SIMM").to_f64(), Some(0.0), "SIML is not read");
    assert_eq!(get("MA.STAT").to_f64(), Some(0.0));
    assert_eq!(get("MA.SEVR").to_f64(), Some(0.0));
}
