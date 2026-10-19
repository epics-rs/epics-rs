//! `IocBuilder::build` arms the callback bands, as `iocBuild`'s
//! `callbackInit` does.

#[path = "build_arms_callback_bands/probe.rs"]
mod probe;

use epics_base_rs::runtime::background::CallbackError;
use epics_base_rs::server::ioc_builder::IocBuilder;

#[epics_macros_rs::epics_test]
async fn build_arms_the_callback_bands() {
    assert_eq!(probe::request(), Err(CallbackError::NotInit));
    let _ = IocBuilder::new().build().await.unwrap();
    assert_eq!(probe::request(), Ok(()));
}
