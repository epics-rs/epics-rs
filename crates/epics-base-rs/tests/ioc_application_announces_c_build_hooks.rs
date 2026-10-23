//! `IocApplication`'s iocInit announces C `iocBuild`'s init hooks, in C's
//! order, as `IocBuilder::build_ioc` does.

#[path = "build_hook_order/probe.rs"]
mod probe;

use epics_base_rs::server::ioc_app::IocApplication;

#[epics_macros_rs::epics_test]
async fn ioc_application_walks_c_build_hooks() {
    probe::register();
    IocApplication::new()
        .port(0)
        .record("p", probe::Probe)
        .run(|_config| async move { Ok(()) })
        .await
        .unwrap();
    assert_eq!(probe::built(), probe::C_ORDER);
}
