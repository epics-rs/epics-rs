//! `IocBuilder::build_ioc` announces softIoc's init hooks in softIoc's order.

mod init_hook_oracle;

use init_hook_oracle as oracle;

use epics_base_rs::server::ioc_app::{ioc_pause, ioc_run};
use epics_base_rs::server::ioc_builder::IocBuilder;

#[epics_macros_rs::epics_test]
async fn build_ioc_announces_softioc_init_hooks() {
    oracle::register();
    let ioc = IocBuilder::new()
        .register_link_type("ca", oracle::installer)
        .build_ioc()
        .await
        .unwrap();
    // C `iocBuild`: `initHookAfterCaLinkInit` follows `dbCaLinkInit`.
    assert_eq!(
        oracle::rs("initHookAtIocBuild", "initHookAfterIocBuilt"),
        oracle::c("initHookAtIocBuild", "initHookAfterIocBuilt")
    );
    assert!(ioc.run().await);
    // C `iocRun` from `iocBuilt` adds the two deprecated states.
    assert_eq!(
        oracle::rs("initHookAtIocRun", "initHookAfterIocRunning"),
        oracle::c("initHookAtIocRun", "initHookAfterIocRunning")
    );
    assert_eq!(ioc_pause(), 0);
    assert_eq!(ioc_run(), 0);
    // From `iocPaused` it does not.
    assert_eq!(
        oracle::rs("initHookAtIocPause", "initHookAfterIocRunning"),
        oracle::c("initHookAtIocPause", "initHookAfterIocRunning")
    );
    // C `initDatabase` put `iocShutdown` on the exit callbacks.
    epics_base_rs::runtime::exit::call_at_exits();
    assert_eq!(oracle::rs_shutdown(), oracle::c_shutdown());
}
