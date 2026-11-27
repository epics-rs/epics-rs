//! `IocBuilder::build_ioc` announces C `iocBuild`'s init hooks, in C's
//! order, as `IocApplication`'s iocInit does.

use crate::build_hook_order_probe as probe;

use epics_base_rs::server::ioc_builder::IocBuilder;

#[epics_macros_rs::epics_test]
async fn build_ioc_walks_c_build_hooks() {
    probe::register();
    let ioc = IocBuilder::new()
        .record("p", probe::Probe)
        .build_ioc()
        .await
        .unwrap();
    assert_eq!(probe::built(), probe::C_ORDER);
    assert!(ioc.run().await);
}
