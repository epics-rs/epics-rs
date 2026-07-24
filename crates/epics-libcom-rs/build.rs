//! Selects the `runtime::task` spawn/sleep/interval backend at compile time.
//!
//! The task seam (`src/runtime/task.rs`) has two mutually exclusive backends,
//! chosen here so the ~two dozen seam sites gate on one uniform condition
//! rather than repeating the target/feature predicate at each:
//!
//! * `exec_backend` — the std-thread background executor (callback pool +
//!   delayed timer + scanOnce worker, `runtime::background`). This is the
//!   RTEMS target's only option (no tokio reactor there), and — via the
//!   `rtems-exec-model` cargo feature — the **host-selectable RTEMS execution
//!   model**: a real product mechanism for a Linux (e.g. PREEMPT_RT)
//!   blocking-front-end deployment that wants the same runtime-free
//!   spawn/timer backend the RTEMS build uses, driving async record-processing
//!   completion on dedicated OS threads instead of a tokio runtime.
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
