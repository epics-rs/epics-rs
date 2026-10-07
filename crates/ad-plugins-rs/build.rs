//! Derives this crate's own copy of the `exec_backend` / `tokio_backend` cfg,
//! the way `epics-libcom-rs` (the original), `epics-base-rs`, `epics-ca-rs`
//! and `epics-pva-rs` derive theirs.
//!
//! `ioc` mounts the plugin set on `epics_ca_rs::server::run_ca_ioc_app` and the QSRV runner, both of which are `tokio_backend`-only, so the module is gated on the same predicate they are.
//!
//! A cfg set by a dependency's build script is not visible here, so every
//! crate that gates on the pair derives it again from the same two inputs:
//! `EPICS_RS_BUILD_EXEC_BACKEND` and the target OS. Reading the variable alone
//! would be wrong on exactly the case the workspace predicate exists for: on
//! RTEMS and VxWorks it is unset while `exec_backend` is ON.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(exec_backend)");
    println!("cargo::rustc-check-cfg=cfg(tokio_backend)");
    println!("cargo::rustc-check-cfg=cfg(epics_embedded_target)");

    emit_rust_hdf5_pin();

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
}

/// The `rust-hdf5` pin, read out of this crate's own manifest and handed to
/// `file_netcdf`'s `_NCProperties` attribute as `RUST_HDF5_PIN`.
///
/// That attribute is how a reader learns which library wrote the file, so the
/// version in it has to be the one actually compiled. It was a literal beside
/// the manifest pin, and the two drifted the moment the pin moved: files
/// written against 0.7.2 claimed `rust-hdf5=0.6`. Deriving it leaves one
/// source of truth, and a bump cannot carry a stale attribute with it.
fn emit_rust_hdf5_pin() {
    println!("cargo::rerun-if-changed=Cargo.toml");
    let manifest = std::fs::read_to_string("Cargo.toml").expect("cannot read Cargo.toml");
    let pin = manifest
        .lines()
        .find(|l| l.trim_start().starts_with("rust-hdf5"))
        .and_then(|l| l.split_once("version")?.1.split('"').nth(1))
        .expect("Cargo.toml declares no `rust-hdf5 = { version = \"..\" }` to derive from");
    println!("cargo::rustc-env=RUST_HDF5_PIN={pin}");
}
