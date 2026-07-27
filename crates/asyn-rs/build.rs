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
    println!("cargo::rustc-check-cfg=cfg(asyn_serial_backend)");
    println!("cargo::rustc-check-cfg=cfg(asyn_af_unix)");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_family = std::env::var("CARGO_CFG_TARGET_FAMILY").unwrap_or_default();
    let embedded = matches!(target_os.as_str(), "rtems" | "vxworks");
    if embedded {
        println!("cargo::rustc-cfg=epics_embedded_target");
    }

    // Whether `drivers::serial_port` exists at all, as opposed to which of the
    // two backends it is. Emitted as one name rather than left as
    // `any(all(unix, not(target_os = "rtems")), windows)` repeated at each
    // caller: `iocsh` needs the answer in four places, and a predicate spelled
    // out four times is four places for the next backend to be added to three
    // of them. `drivers::mod` still spells out the arms, because there the
    // question genuinely is *which* file to mount.
    //
    // CARGO_CFG_TARGET_FAMILY is comma-separated for targets in more than one
    // family, so this is a membership test, not an equality test.
    //
    // NOT `epics_embedded_target`: VxWorks has the backend (its `libc` binds
    // VxWorks 7's POSIX termios ABI-correctly), RTEMS does not (`libc`'s
    // newlib `struct termios` is the Linux shape, so it would compile and
    // write to the wrong offsets). The predicate must therefore name RTEMS,
    // and `drivers::mod` states the evidence.
    let unix = target_family.split(',').any(|f| f == "unix");
    let windows = target_family.split(',').any(|f| f == "windows");
    if (unix && target_os != "rtems") || windows {
        println!("cargo::rustc-cfg=asyn_serial_backend");
    }

    // Whether `unix://` is a usable protocol, mirroring C's own `HAS_AF_UNIX`:
    //
    //     #if !defined(_WIN32) && !defined(vxWorks) && defined(AF_UNIX)
    //     # define HAS_AF_UNIX 1
    //
    // (`drvAsynIPPort.c:62-64`). Named here for the same reason C names it:
    // `drivers::ip_port` tests the condition at eight sites, and C tests it at
    // as many, in both cases because the enum variant, each transport arm, the
    // connect and its refusal must all agree.
    //
    // NOT `epics_embedded_target`, and the difference is the point: that macro
    // excludes `_WIN32` and `vxWorks` but says nothing about `__rtems__`, so
    // RTEMS keeps AF_UNIX. VxWorks is the one embedded target that loses it,
    // and `std` compiles `UnixStream` for the triple regardless — so without
    // this the VxWorks build takes the unix arm, offers `unix://`, and fails
    // at connect on a socket family C never offered there.
    if unix && target_os != "vxworks" {
        println!("cargo::rustc-cfg=asyn_af_unix");
    }
}
