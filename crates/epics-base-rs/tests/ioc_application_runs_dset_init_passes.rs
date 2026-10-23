//! `IocApplication`'s `iocBuild` runs every dset's `init(0)` and `init(1)`,
//! used or not, as C `initDevSup` and `finishDevSup` do
//! (`iocInit.c:441-480`).

#[path = "dset_init_pass/probe.rs"]
mod probe;

use epics_base_rs::server::ioc_app::{IocApplication, IocInitDecision};

#[test]
fn unused_dsets_init_in_record_type_order() {
    epics_base_rs::runtime::task::test_block_on(async {
        probe::register();
        IocApplication::new()
            .port(0)
            .before_ioc_init(|| IocInitDecision::run(false))
            .run(|_config| async { Ok(()) })
            .await
            .unwrap();
        probe::assert_logged(true);
    });
}
