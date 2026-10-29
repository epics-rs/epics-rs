//! Compiles the validate programs C's `test/validate/Makefile` builds
//! (`REGRESSION_TESTS` and `REGRESSION_TESTS_REMOTE_ONLY`) with snc-rs to
//! `$OUT_DIR/<stem>.rs`. `pvSync.st` and `raceCommon.st` are included by
//! others, `race.st` is commented out of the Makefile, and `skeleton.st`
//! is a template; none of them is built. Nor is a program snc-rs does not
//! translate yet.
//!
//! It also derives this crate's own `exec_backend` / `tokio_backend` cfg, as
//! `std-rs`'s build script explains: the CA-remote cases need a CA server,
//! which only the reactor has.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(exec_backend)");
    println!("cargo::rustc-check-cfg=cfg(tokio_backend)");

    let embedded_target = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "rtems");

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

    for program in seq_validate_programs() {
        epics_seq_rs::snc::build::compile(format!("validate/{program}.st"));
    }
}

fn seq_validate_programs() -> &'static [&'static str] {
    &[
        "array",
        "assign",
        "bittypes",
        "change",
        "commaOperator",
        "entryOpte",
        "evflag",
        "evflagExt",
        "exitOptx",
        "local",
        "monitorEvflag",
        "norace",
        "opttVar",
        "pvAssignStress",
        "pvAssignSubst",
        "pvGetAsync",
        "pvGetComplete",
        "pvGetSync",
        "pvPutAndMonitor",
        "pvPutAsync",
        "pvSyncDb",
        "pvSyncNoDb",
        "reassign",
        "safeModeNotAssigned",
        "safeMonitor",
        "stop",
        "userfunc",
        "userfuncEf",
    ]
}
