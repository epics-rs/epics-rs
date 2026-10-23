//! `IocBuilder::build`, which announces nothing, runs every dset's
//! `init(0)` and `init(1)`, used or not, as C `initDevSup` and
//! `finishDevSup` do (`iocInit.c:441-480`).

#[path = "dset_init_pass/probe.rs"]
mod probe;

use epics_base_rs::server::ioc_builder::IocBuilder;

#[epics_macros_rs::epics_test]
async fn unused_dsets_init_in_record_type_order() {
    probe::register();
    let _db = IocBuilder::new().build().await.unwrap();
    probe::assert_logged(false);
}
