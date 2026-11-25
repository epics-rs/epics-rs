//! An `iocBuild` runs every registered driver's `init` once, between
//! `initHookAfterCaLinkInit` and `initHookAfterInitDrvSup`, as C
//! `iocBuild_2`'s `initDrvSup` does (`iocInit.c:165-168`).

use std::sync::{Arc, Mutex};

use epics_base_rs::server::driver_support::{DriverSupport, register_driver_support};
use epics_base_rs::server::ioc_app::init_hooks::{InitHookState, init_hook_register};
use epics_base_rs::server::ioc_builder::IocBuilder;

static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct Drv(&'static str);
impl DriverSupport for Drv {
    fn report(&self, _: i32) -> Option<Vec<u8>> {
        None
    }
    fn init(&self) {
        LOG.lock().unwrap().push(format!("init {}", self.0));
    }
}

#[epics_macros_rs::epics_test]
async fn driver_init_runs_in_registration_order_before_its_announce() {
    register_driver_support("drvA", Arc::new(Drv("drvA")));
    register_driver_support("drvB", Arc::new(Drv("drvB")));
    init_hook_register(Arc::new(|state| {
        if matches!(
            state,
            InitHookState::AfterCaLinkInit
                | InitHookState::AfterInitDrvSup
                | InitHookState::AfterInitRecSup
        ) {
            LOG.lock().unwrap().push(state.name().to_string());
        }
    }));
    let _ioc = IocBuilder::new().build_ioc().await.unwrap();
    assert_eq!(
        *LOG.lock().unwrap(),
        [
            "initHookAfterCaLinkInit",
            "init drvA",
            "init drvB",
            "initHookAfterInitDrvSup",
            "initHookAfterInitRecSup",
        ]
    );
}
