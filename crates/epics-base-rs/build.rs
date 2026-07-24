//! Derives this crate's own copy of the `exec_backend` / `tokio_backend` cfg.
//!
//! The seam itself lives in `epics-libcom-rs` (whose `build.rs` is the
//! original of this one). This crate still needs the predicate because code
//! *above* the seam gates on it too — `server::scan` sizes its periodic-scan
//! threads differently on the reactor-free backend — and a cfg set by a
//! dependency's build script is not visible here. Each crate that declares
//! `rtems-exec-model` deriving its own cfg from the same two inputs is the
//! existing pattern in this workspace (`epics-ca-rs`, `epics-pva-rs`).
//!
//! Two copies of a predicate is two chances to disagree, so they are pinned:
//! `lib.rs` carries
//! `const _: () = assert!(epics_libcom_rs::EXEC_BACKEND == cfg!(exec_backend))`,
//! which fails to compile if this crate's `rtems-exec-model` ever stops
//! forwarding to the runtime crate's.
//!
//! * `exec_backend` — the std-thread background executor. The RTEMS target's
//!   only option (no tokio reactor there), and — via the `rtems-exec-model`
//!   cargo feature — the host-selectable RTEMS execution model.
//! * `tokio_backend` — the tokio runtime. The hosted default (feature off,
//!   non-RTEMS target).
//!
//! Exactly one of the two is set for any build.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(exec_backend)");
    println!("cargo::rustc-check-cfg=cfg(tokio_backend)");

    let rtems = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("rtems");
    // Cargo exports `CARGO_FEATURE_<NAME>` (uppercased, `-` → `_`) for every
    // enabled feature of this crate.
    let host_exec_model = std::env::var_os("CARGO_FEATURE_RTEMS_EXEC_MODEL").is_some();

    if rtems || host_exec_model {
        println!("cargo::rustc-cfg=exec_backend");
    } else {
        println!("cargo::rustc-cfg=tokio_backend");
    }
}
