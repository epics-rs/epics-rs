//! The `device()` lines a C IOC's `.dbd` would carry for the probe device
//! supports these tests register: a `.db` may name only a device-menu choice.

use epics_base_rs::server::record::{DbLinkType, DevSup, Dset};

static ROWS: [DevSup; 4] = [
    row("probeLive"),
    row("probeDead"),
    row("probeErr"),
    row("probeRaw"),
];

const fn row(choice: &'static str) -> DevSup {
    DevSup {
        name: "devProbe",
        choice,
        link_type: DbLinkType::Constant,
        dset: Dset::new(6, &[3], false),
    }
}

/// Every probe choice, for each of `record_types`.
pub fn devices<'a>(record_types: &[&'a str]) -> Vec<(&'a str, &'static [DevSup])> {
    record_types
        .iter()
        .map(|&rtype| (rtype, &ROWS[..]))
        .collect()
}
