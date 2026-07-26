//! Emits `epics_embedded_target` for the reactor-free triples (RTEMS,
//! VxWorks).
//!
//! Same three lines as `epics-libcom-rs`, `epics-base-rs`, `epics-ca-rs`,
//! `epics-pva-rs` and `epics-bridge-rs` already carry, and for the reason
//! `epics-libcom-rs`'s own build script states: a seam above it gates on this
//! one capability cfg instead of repeating
//! `any(target_os = "rtems", target_os = "vxworks")` at each site. Here it
//! selects which serial backend `drivers::mod` mounts — a `Cargo.toml` target
//! table cannot express the predicate, and a module gate must.
//!
//! Note this is deliberately *not* the predicate for the AF_UNIX gate in
//! `drivers::ip_port`. C turns AF_UNIX off on vxWorks alone
//! (`drvAsynIPPort.c:62`, which excludes `_WIN32` and `vxWorks` but not
//! `__rtems__`), so that site names `target_os = "vxworks"` directly. The two
//! are different questions and share no predicate.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(epics_embedded_target)");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if matches!(target_os.as_str(), "rtems" | "vxworks") {
        println!("cargo::rustc-cfg=epics_embedded_target");
    }
}
