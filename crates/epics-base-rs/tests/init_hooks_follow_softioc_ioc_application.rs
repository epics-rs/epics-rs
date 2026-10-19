//! `IocApplication`'s iocInit announces softIoc's init hooks in softIoc's
//! order.

mod init_hook_oracle;

use init_hook_oracle as oracle;

use epics_base_rs::server::ioc_app::{IocApplication, ioc_pause, ioc_run};

#[epics_macros_rs::epics_test]
async fn ioc_application_announces_softioc_init_hooks() {
    oracle::register();
    IocApplication::new()
        .port(0)
        .register_link_type("ca", oracle::installer)
        .run(|_config| async move {
            assert_eq!(ioc_pause(), 0);
            assert_eq!(ioc_run(), 0);
            Ok(())
        })
        .await
        .unwrap();
    // C `iocBuild`: `initHookAfterCaLinkInit` follows `dbCaLinkInit`.
    assert_eq!(
        oracle::rs("initHookAtIocBuild", "initHookAfterIocBuilt"),
        oracle::c("initHookAtIocBuild", "initHookAfterIocBuilt")
    );
    // C `iocRun` from `iocBuilt` adds the two deprecated states.
    assert_eq!(
        oracle::rs("initHookAtIocRun", "initHookAfterIocRunning"),
        oracle::c("initHookAtIocRun", "initHookAfterIocRunning")
    );
    // From `iocPaused` it does not.
    assert_eq!(
        oracle::rs("initHookAtIocPause", "initHookAfterIocRunning"),
        oracle::c("initHookAtIocPause", "initHookAfterIocRunning")
    );
    // `run` returns through the exit callbacks C `initDatabase` armed.
    assert_eq!(oracle::rs_shutdown(), oracle::c_shutdown());
}
