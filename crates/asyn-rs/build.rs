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
//! (`drvAsynIPPort.c:63-65`, which excludes `_WIN32` and `vxWorks` but not
//! `__rtems__`), so that site names `target_os = "vxworks"` directly. The two
//! are different questions and share no predicate.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(epics_embedded_target)");
    println!("cargo::rustc-check-cfg=cfg(exec_backend)");
    println!("cargo::rustc-check-cfg=cfg(tokio_backend)");
    println!("cargo::rustc-check-cfg=cfg(asyn_serial_backend)");
    println!("cargo::rustc-check-cfg=cfg(asyn_baud_code_is_rate)");
    println!("cargo::rustc-check-cfg=cfg(asyn_af_unix)");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_family = std::env::var("CARGO_CFG_TARGET_FAMILY").unwrap_or_default();
    let embedded_target = matches!(target_os.as_str(), "rtems" | "vxworks");
    if embedded_target {
        println!("cargo::rustc-cfg=epics_embedded_target");
    }

    // The workspace backend predicate, derived here the way `epics-libcom-rs`
    // (the original), `epics-base-rs`, `epics-ca-rs` and `epics-pva-rs` derive
    // theirs. `tests/process_gate_tests.rs` drives the asyn record through a
    // real CA server's put gate, and that server is reactor-backed.
    //
    // A cfg set by a dependency's build script is not visible here, so the
    // fact arrives twice: from `EPICS_RS_BUILD_EXEC_BACKEND` and from the
    // target OS. The variable alone is the wrong predicate on exactly the case
    // this cfg exists for: on RTEMS and VxWorks it is unset while
    // `exec_backend` is ON.
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

    // Whether `drivers::serial_port` exists at all, as opposed to which of the
    // two backends it is. Emitted as one name rather than left as
    // `any(all(unix, not(target_os = "rtems")), windows)` repeated at each
    // caller: `iocsh` asks it once per serial command, and a predicate spelled
    // out at each of them is one place per command for the next backend to be
    // added to all but one. `drivers::mod` still spells out the arms, because
    // there the question genuinely is *which* file to mount.
    //
    // CARGO_CFG_TARGET_FAMILY is comma-separated for targets in more than one
    // family, so this is a membership test, not an equality test.
    //
    // Every unix plus Windows: the POSIX backend reaches the termios ABI
    // through its own `platform` seam rather than through `libc` directly, so
    // a target whose `libc` binding is wrong or absent (RTEMS) is a matter of
    // which arm of that seam compiles, not of whether the driver exists.
    let unix = target_family.split(',').any(|f| f == "unix");
    let windows = target_family.split(',').any(|f| f == "windows");
    if unix || windows {
        println!("cargo::rustc-cfg=asyn_serial_backend");
    }

    // Whether this platform's termios `Bxxx` speed codes ARE the literal baud
    // rate. C asks it as a preprocessor test on the constants themselves
    // (`drvAsynSerialPort.c:273`), which decides whether *any* rate can be
    // programmed or only the ladder's; `drivers::serial_port` needs the answer
    // as a `cfg` because the two arms name different constants, and it needs
    // it in `baud_to_speed`, in `speed_to_baud`, and in the assertion that pins
    // this list against the constants. Named once here so adding a target is
    // one edit rather than one per site, which can disagree.
    //
    // macOS/iOS and the BSDs are the group C names; both embedded targets join
    // it — VxWorks (`termios.h:22-52`) and RTEMS (`sys/_termios.h:186-221`)
    // both define `B300 == 300`. Linux is the counter-example: there the codes
    // are small encoded integers and `B9600 == 13`.
    if matches!(
        target_os.as_str(),
        "macos" | "ios" | "freebsd" | "netbsd" | "openbsd" | "dragonfly" | "vxworks" | "rtems"
    ) {
        println!("cargo::rustc-cfg=asyn_baud_code_is_rate");
    }

    // Whether `unix://` is a usable protocol, mirroring C's own `HAS_AF_UNIX`:
    //
    //     #if !defined(_WIN32) && !defined(vxWorks) && defined(AF_UNIX)
    //     # define HAS_AF_UNIX 1
    //
    // (`drvAsynIPPort.c:63-65`). Named here for the same reason C names it:
    // `drivers::ip_port` tests the condition wherever the answer shows — the
    // enum variant, each transport arm, the connect and its refusal — and so
    // does C, in both cases because all of them must agree.
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
