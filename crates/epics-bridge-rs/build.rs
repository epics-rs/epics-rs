//! Emits the RTEMS link arguments for `realtime-pva-ioc`.
//!
//! This crate produces an RTEMS IOC binary, and link *arguments* — unlike
//! `-L`/`-l` — do not propagate from a dependency's build script to a
//! dependent's link (measured; see `epics_rtems_boot::contract`). So the
//! package that owns the binary has to emit them, and it does so by calling
//! into the one crate that defines them rather than by repeating a flag list.
//!
//! The binary moved here from `epics-pva-rs` when it grew a QSRV group
//! source: mounting QSRV needs `epics-bridge-rs`, and `epics-pva-rs` cannot
//! depend on the bridge without a cyclic package dependency. This file moved
//! with it, for the reason stated above — leaving it behind would have left
//! the binary linking without its RTEMS arguments.
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
//!
//! The same applies to `exec_backend`/`tokio_backend`, the pair that says
//! whether a future started through `runtime::task::spawn` gets a reactor:
//!
//!     exec_backend  <=> epics_embedded_target || EPICS_RS_BUILD_EXEC_BACKEND=thread
//!     tokio_backend <=> otherwise
//!
//! This crate needs it because both bridges front onto reactor-bound servers:
//! `ca_gateway` dials and serves CA through `epics_ca_rs::server`'s async
//! front-end and `qsrv`'s adapter mounts `epics_pva_rs::server_native`'s. Both
//! are `tokio_backend`-only, and without this emission a `tokio_backend` gate
//! written in this crate reads as an unknown cfg — always false — and would
//! compile the async half out of *every* build rather than only the
//! reactor-free one. The `const` assertion in `src/lib.rs` pins this copy of
//! the rule against `epics-base-rs`'s.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(epics_embedded_target)");
    println!("cargo::rustc-check-cfg=cfg(exec_backend)");
    println!("cargo::rustc-check-cfg=cfg(tokio_backend)");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let embedded_target = matches!(target_os.as_str(), "rtems" | "vxworks");
    if embedded_target {
        println!("cargo::rustc-cfg=epics_embedded_target");
    }

    // Build-time backend selection, from the environment rather than from a
    // cargo feature: a feature that flips a backend is not additive, so
    // `--all-features` turned the reactor off and no single invocation meant
    // "everything on". `epics-libcom-rs`'s module docs carry the reasoning;
    // `tools/rtems-exec-gate` holds every copy of this block against that
    // crate's, so 23 derivations of one rule cannot drift apart.
    println!("cargo::rerun-if-env-changed=EPICS_RS_BUILD_EXEC_BACKEND");
    let requested = std::env::var_os("EPICS_RS_BUILD_EXEC_BACKEND").unwrap_or_default();
    let host_exec_backend = match requested.to_string_lossy().as_ref() {
        "thread" => true,
        "" | "tokio" => false,
        bad => panic!(
            "EPICS_RS_BUILD_EXEC_BACKEND={bad}: the exec backend is `thread` \
             (reactor-free std threads) or `tokio` (the host default, which an \
             unset or empty variable also selects)"
        ),
    };
    if embedded_target || host_exec_backend {
        println!("cargo::rustc-cfg=exec_backend");
    } else {
        println!("cargo::rustc-cfg=tokio_backend");
    }

    epics_rtems_boot::contract::emit_link_args();
}
