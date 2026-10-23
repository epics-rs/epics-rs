//! `IocBuilder::build`, which announces nothing, still runs every driver's
//! `init` once, as C `initDrvSup` does (`iocInit.c:398-413`).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use epics_base_rs::server::driver_support::{DriverSupport, register_driver_support};
use epics_base_rs::server::ioc_builder::IocBuilder;

static INITS: AtomicUsize = AtomicUsize::new(0);

struct Drv;
impl DriverSupport for Drv {
    fn report(&self, _: i32) -> Option<Vec<u8>> {
        None
    }
    fn init(&self) {
        INITS.fetch_add(1, Ordering::Relaxed);
    }
}

#[epics_macros_rs::epics_test]
async fn driver_init_runs_once() {
    register_driver_support("drvBuildProbe", Arc::new(Drv));
    let _db = IocBuilder::new().build().await.unwrap();
    assert_eq!(INITS.load(Ordering::Relaxed), 1);
}
