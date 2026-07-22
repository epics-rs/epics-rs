//! Emits the RTEMS link arguments for `rtems-pva-ioc`.
//!
//! This crate produces an RTEMS IOC binary, and link *arguments* — unlike
//! `-L`/`-l` — do not propagate from a dependency's build script to a
//! dependent's link (measured; see `epics_rtems_boot::contract`). So the
//! package that owns the binary has to emit them, and it does so by calling
//! into the one crate that defines them rather than by repeating a flag list.
//!
//! The binary moved here from `epics-pva-rs` when it grew a QSRV group source
//! (doc/qsrv-rtems-design.md §9.7): mounting QSRV needs `epics-bridge-rs`, and
//! `epics-pva-rs` cannot depend on the bridge without a cyclic package
//! dependency. This file moved with it, for the reason stated above — leaving
//! it behind would have left the binary linking without its RTEMS arguments.
//!
//! No-ops on every non-RTEMS target.

fn main() {
    epics_rtems_boot::contract::emit_link_args();
}
