//! `mcaRecord.c:442-444` seeds SIMM only for `siml.type == CONSTANT`, and a
//! `{const:…}` SIML is a `JSON_LINK` (`dbStaticLib.c:2280-2287`).

// RTEMS-EXEC-MODEL-ALLOW(1): no CA server is built; the reactor is
// `#[tokio::test]`'s own, which the exec backend does not remove.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset};

/// `mcaSupport.dbd`'s `device(mca,CONSTANT,devMCA_soft,"Soft Channel")`.
static SOFT_MCA: DevSup = DevSup {
    name: "devMCA_soft",
    choice: "Soft Channel",
    link_type: DbLinkType::Constant,
    dset: Dset::new(6, &[2, 4, 5], false),
};

const DB: &str = r#"
record(mca, "M:json") { field(DTYP, "Soft Channel") field(SIML, "{const:1}") field(NMAX, "4") }
record(mca, "M:const") { field(DTYP, "Soft Channel") field(SIML, "1") field(NMAX, "4") }
"#;

#[tokio::test]
async fn a_json_siml_does_not_seed_simm() {
    let (name, factory) = mca_rs::mca_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_device_support("mca", &SOFT_MCA, || Box::new(mca_rs::SoftMca))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let simm = |rec: &str| db.get_pv(format!("{rec}.SIMM")).unwrap().to_f64();
    assert_eq!(simm("M:json"), Some(0.0));
    assert_eq!(simm("M:const"), Some(1.0));
}
