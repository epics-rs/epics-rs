//! Two dsets no record uses, registered `longin` first, and the hooks
//! either side of their passes.

use std::sync::{Arc, Mutex};

use epics_base_rs::server::ioc_app::init_hooks::{InitHookState, init_hook_register};
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, register_device_menu};

static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn push(line: String) {
    LOG.lock().unwrap().push(line);
}

fn init_ai(after: bool) {
    push(format!("ai init({})", after as i32));
}

fn init_longin(after: bool) {
    push(format!("longin init({})", after as i32));
}

static AI: &[DevSup] = &[DevSup {
    name: "devAiDsetInitProbe",
    choice: "Dset Init Probe",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(6, &[4], false).with_init(init_ai),
}];

static LONGIN: &[DevSup] = &[DevSup {
    name: "devLiDsetInitProbe",
    choice: "Dset Init Probe",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[4], false).with_init(init_longin),
}];

pub fn register() {
    register_device_menu("longin", LONGIN);
    register_device_menu("ai", AI);
    init_hook_register(Arc::new(|state| {
        if matches!(
            state,
            InitHookState::AfterInitRecSup
                | InitHookState::AfterInitDevSup
                | InitHookState::AfterInitDatabase
                | InitHookState::AfterFinishDevSup
        ) {
            push(state.name().to_string());
        }
    }));
}

/// The log is C's: `initDevSup` in `recordTypeList` order, then its
/// announce, and `finishDevSup` likewise; without the announces for a build
/// that makes none.
pub fn assert_logged(announced: bool) {
    let all = [
        "initHookAfterInitRecSup",
        "ai init(0)",
        "longin init(0)",
        "initHookAfterInitDevSup",
        "initHookAfterInitDatabase",
        "ai init(1)",
        "longin init(1)",
        "initHookAfterFinishDevSup",
    ];
    let expected: Vec<&str> = all
        .into_iter()
        .filter(|l| announced || !l.starts_with("initHook"))
        .collect();
    assert_eq!(*LOG.lock().unwrap(), expected);
}
