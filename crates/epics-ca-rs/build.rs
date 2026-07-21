//! Emits the RTEMS link arguments for `rtems-ca-ioc`.
//!
//! This crate produces an RTEMS IOC binary, and link *arguments* — unlike
//! `-L`/`-l` — do not propagate from a dependency's build script to a
//! dependent's link (measured; see `epics_rtems_boot::contract`). So the
//! package that owns the binary has to emit them, and it does so by calling
//! into the one crate that defines them rather than by repeating a flag list.
//!
//! No-ops on every non-RTEMS target.

fn main() {
    epics_rtems_boot::contract::emit_link_args();
}
