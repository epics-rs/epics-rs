//! Named time-stamp sources — C's `registryFunctionFind` seen from asyn.
//!
//! C `asynRegisterTimeStampSource(portName, functionName)`
//! (asynShellCommands.c:1181-1204) resolves `functionName` through the EPICS
//! function registry and hands the resulting `timeStampCallback` to
//! `pasynManager->registerTimeStampSource`. An IOC application publishes the
//! function into that registry (a `function()` line in its dbd, or
//! `registryFunctionAdd`), and only then can an st.cmd name it.
//!
//! Rust has no C symbol table to search, so the registry is explicit: an
//! application calls [`register_time_stamp_source`] with the same name its
//! st.cmd will use. That is the only way a name can become resolvable, which
//! makes the iocsh command's failure mode identical to C's — an unregistered
//! name is refused, it does not silently install nothing.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::SystemTime;

/// C `timeStampCallback` (asynDriver.h): the driver calls it to stamp a value.
pub type TimeStampSource = Arc<dyn Fn() -> SystemTime + Send + Sync>;

fn registry() -> &'static RwLock<HashMap<String, TimeStampSource>> {
    static REGISTRY: OnceLock<RwLock<HashMap<String, TimeStampSource>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Publish a time-stamp source under the name an st.cmd will use with
/// `asynRegisterTimeStampSource`. C's `registryFunctionAdd`.
pub fn register_time_stamp_source<F>(name: &str, source: F)
where
    F: Fn() -> SystemTime + Send + Sync + 'static,
{
    if let Ok(mut reg) = registry().write() {
        reg.insert(name.to_string(), Arc::new(source));
    }
}

/// Resolve a name published by [`register_time_stamp_source`] — C's
/// `registryFunctionFind` (asynShellCommands.c:1197). `None` is C's "cannot
/// find function".
pub fn find_time_stamp_source(name: &str) -> Option<TimeStampSource> {
    registry().read().ok()?.get(name).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn an_unregistered_name_does_not_resolve() {
        assert!(find_time_stamp_source("nothing_registered_under_this").is_none());
    }

    #[test]
    fn a_registered_name_resolves_to_its_source() {
        let fixed = UNIX_EPOCH + Duration::from_secs(1_000_000);
        register_time_stamp_source("fixed_ts_for_test", move || fixed);
        let f = find_time_stamp_source("fixed_ts_for_test").expect("registered name must resolve");
        assert_eq!(f(), fixed);
    }
}
