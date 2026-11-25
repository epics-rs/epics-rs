//! The `device()` lines a C IOC's `.dbd` would carry for the device supports
//! a test serves through `register_dynamic_device_support`.

use epics_base_rs::server::record::{DbLinkType, DevSup, Dset};

/// One row per choice, all of `link_type`.
pub fn rows(choices: &[&'static str], link_type: DbLinkType) -> &'static [DevSup] {
    choices
        .iter()
        .map(|&choice| DevSup {
            name: "devTest",
            choice,
            link_type,
            dset: Dset::new(6, &[3], false),
        })
        .collect::<Vec<_>>()
        .leak()
}
