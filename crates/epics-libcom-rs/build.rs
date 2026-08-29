//! Selects the `runtime::task` spawn/sleep/interval backend at compile time.
//!
//! The task seam (`src/runtime/task.rs`) has two mutually exclusive backends,
//! chosen here so the ~two dozen seam sites gate on one uniform condition
//! rather than repeating the target/environment predicate at each:
//!
//! * `exec_backend` — the std-thread background executor (callback pool +
//!   delayed timer + scanOnce worker, `runtime::background`). This is the
//!   only option on `epics_embedded_target` (RTEMS or VxWorks — no tokio
//!   reactor on either), and — via `EPICS_RS_BUILD_EXEC_BACKEND=thread` — the
//!   **host-selectable RTEMS execution model**: a real product mechanism for a
//!   Linux (e.g. PREEMPT_RT) blocking-front-end deployment that wants the same
//!   runtime-free spawn/timer backend the embedded build uses, driving async
//!   record-processing completion on dedicated OS threads instead of a tokio
//!   runtime.
//! * `tokio_backend` — the tokio runtime. The hosted default (variable unset
//!   or `tokio`, non-embedded target).
//!
//! Exactly one of the two is set for any build.
//!
//! # Why an environment variable and not a cargo feature
//!
//! It was a feature, `rtems-exec-model`, and a feature that flips a backend is
//! not additive: `--all-features` resolved it ON, so the single invocation that is
//! supposed to mean "everything compiled" turned the reactor OFF and
//! *subtracted* every reactor-dependent test, doc example and lint from the
//! run. There was no cargo invocation that meant "host reactor plus everything
//! on", which is how a `-D warnings` failure and two broken doc examples
//! survived a full CI battery.
//!
//! An environment variable is orthogonal to feature resolution, so
//! `--all-features` means what it says and the backend is one more axis to
//! vary. The cost is that cargo does not track it for you: every build script
//! that reads it must also emit `cargo::rerun-if-env-changed`, or a changed
//! value reuses artefacts built under the old one. That line, not the read, is
//! the correctness of the arrangement — `tools/rtems-exec-gate` holds all 23
//! copies of the derivation against `CANONICAL_DERIVATION` byte for byte so no
//! copy can lose it.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(exec_backend)");
    println!("cargo::rustc-check-cfg=cfg(tokio_backend)");
    println!("cargo::rustc-check-cfg=cfg(epics_embedded_target)");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    // The reactor-free targets: no tokio reactor exists on either, so both the
    // exec-model selection below and every dependency/socket-portability seam
    // above `epics-libcom-rs` gate on this one capability cfg instead of
    // repeating `any(target_os = "rtems", target_os = "vxworks")`.
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
}
