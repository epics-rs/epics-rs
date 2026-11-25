//! What a build path's `iocBuild` announces, against C's: each init hook
//! with the IOC state its observer reads, and the probe record's
//! `init_record` passes in between.

use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::ioc_app::get_ioc_state;
use epics_base_rs::server::ioc_app::init_hooks::init_hook_register;
use epics_base_rs::server::record::{FieldDesc, ProcessOutcome, Record};
use epics_base_rs::types::EpicsValue;

static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub struct Probe;

impl Record for Probe {
    fn record_type(&self) -> &'static str {
        "probe"
    }
    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        LOG.lock().unwrap().push(format!("init_record({pass})"));
        Ok(())
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }
    fn get_field(&self, _name: &str) -> Option<EpicsValue> {
        None
    }
    fn put_field(&mut self, _name: &str, _value: EpicsValue) -> CaResult<()> {
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        &[]
    }
}

pub fn register() {
    init_hook_register(Arc::new(|state| {
        LOG.lock()
            .unwrap()
            .push(format!("{} {:?}", state.name(), get_ioc_state()));
    }));
}

/// The log through `initHookAfterIocBuilt`.
pub fn built() -> Vec<String> {
    let log = LOG.lock().unwrap();
    let end = log
        .iter()
        .position(|l| l.starts_with("initHookAfterIocBuilt "))
        .map_or(log.len(), |i| i + 1);
    log[..end].to_vec()
}

/// C `iocBuild` (`iocInit.c:116-206`): `iocBuilding` is assigned after
/// `initHookAtBeginning`, `iocBuilt` before `initHookAfterIocBuilt`, and
/// `initDatabase` runs between `initHookAfterInitDevSup` and
/// `initHookAfterInitDatabase`.
pub const C_ORDER: [&str; 15] = [
    "initHookAtIocBuild Void",
    "initHookAtBeginning Void",
    "initHookAfterCallbackInit Building",
    "initHookAfterCaLinkInit Building",
    "initHookAfterInitDrvSup Building",
    "initHookAfterInitRecSup Building",
    "initHookAfterInitDevSup Building",
    "init_record(0)",
    "init_record(1)",
    "initHookAfterInitDatabase Building",
    "initHookAfterFinishDevSup Building",
    "initHookAfterScanInit Building",
    "initHookAfterInitialProcess Building",
    "initHookAfterCaServerInit Building",
    "initHookAfterIocBuilt Built",
];
