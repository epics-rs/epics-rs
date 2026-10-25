//! Rust port of the synApps [ip](https://github.com/epics-modules/ip) module's
//! SNL programs, compiled by epics-snc-rs from `src/snl` and run on
//! epics-seq-rs.
//!
//! # C reference pins
//!
//! Every `file:NNN` citation in this crate resolves in ip at `d00f687`.

pub mod snl;

/// The module root, for an `st.cmd` that names `$(IP)/db/...`.
pub const IP_DIR: &str = env!("CARGO_MANIFEST_DIR");

/// Path to the bundled database template directory.
pub const IP_DB_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/db");

/// The `registrar()` lines of `ipSupport.dbd`: each program becomes
/// startable by the shell's `seq` command, which the first registration
/// adds. Idempotent.
pub fn register_ip_dbd() {
    // C defect fixed: ipSupport.dbd has no registrar(FederalRegistrar), so a
    // C IOC built on it answers `seq &Federal` with "Can't find sequencer".
    snl::federal::FederalRegistrar();
}
