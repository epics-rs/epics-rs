//! Emits the RTEMS link arguments for `realtime-pva-ioc`.
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
//!
//! Also emits `epics_embedded_target` (set when `CARGO_CFG_TARGET_OS` is
//! `rtems` or `vxworks`) for this crate's own module gates
//! (`qsrv::run_ca_pva_qsrv_ioc` and its definition in `pva_adapter.rs`) —
//! a `cargo::rustc-cfg` set by a dependency's build script does not
//! propagate to a dependent crate either, so each crate that reads the cfg
//! emits its own copy, same as `epics-libcom-rs`/`epics-base-rs`/
//! `epics-ca-rs`/`epics-pva-rs`.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(epics_embedded_target)");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if matches!(target_os.as_str(), "rtems" | "vxworks") {
        println!("cargo::rustc-cfg=epics_embedded_target");
    }

    epics_rtems_boot::contract::emit_link_args();
}
