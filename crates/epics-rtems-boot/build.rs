//! Compiles the RTEMS boot shim and emits the propagating half of the link
//! contract.
//!
//! No-ops on every non-RTEMS target, so a host build — and `cargo package` on a
//! machine with no cross toolchain — never needs the cross compiler.
//!
//! The flag definitions live in `src/contract.rs` and are `include!`d rather
//! than duplicated: that same file is compiled into the library so a dependent
//! IOC crate's `build.rs` can emit the *non*-propagating half (see the module
//! docs there for the measurement that forces the split).

// `#[path]` rather than `include!`: this makes `contract.rs` a real module file
// of the build script, so its `//!` module docs stay legal and there is still
// exactly one copy of the flag definitions.
#[path = "src/contract.rs"]
mod contract;

use contract::*;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(rtems_boot_linked)");
    println!("cargo::rerun-if-changed=csrc/boot_args.c");
    println!("cargo::rerun-if-changed=csrc/boot_args.h");
    println!("cargo::rerun-if-changed=csrc/rtems_config.c");
    println!("cargo::rerun-if-changed=csrc/rtems_init.c");
    println!("cargo::rerun-if-changed=csrc/rtems_shell_cmds.c");
    println!("cargo::rerun-if-changed=csrc/rtems_stats.c");
    println!("cargo::rerun-if-changed=src/contract.rs");
    println!("cargo::rerun-if-env-changed={BSP_PREFIX_ENV}");
    println!("cargo::rerun-if-env-changed={BSP_ENV}");
    println!("cargo::rerun-if-env-changed={CMDLINE_ENV}");
    println!("cargo::rerun-if-env-changed={KQUEUE_ENV}");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("rtems") {
        return;
    }

    // Fail here rather than at link time with a message about VFP register
    // arguments: rustc's own code generation has to match the multilib the
    // RTEMS libraries were built for.
    if let Err(why) = check_abi(
        &std::env::var("CARGO_CFG_TARGET_ABI").unwrap_or_default(),
        &std::env::var("CARGO_CFG_TARGET_FEATURE").unwrap_or_default(),
    ) {
        panic!("{why}");
    }

    // Unset prefix ⟹ the portability-check configuration: type-check only, no
    // toolchain required, and `lib.rs` leaves a self-naming undefined symbol so
    // the resulting objects cannot silently become a shimless image.
    let Some(prefix) = resolve_prefix() else {
        return;
    };

    let lib_dir = bsp_lib_dir(&prefix);
    let include_dir = bsp_include_dir(&prefix);

    let mut build = cc::Build::new();
    build
        // Compiled here as well as by `scripts/csrc-check.sh`: the gate proves
        // the tokeniser still behaves, this line is what puts it in the image.
        // One source, so the gate cannot drift from what boots.
        .file("csrc/boot_args.c")
        .file("csrc/rtems_config.c")
        .file("csrc/rtems_init.c")
        .file("csrc/rtems_shell_cmds.c")
        .file("csrc/rtems_stats.c")
        .include(&include_dir)
        // Base passes -DBSP_$(RTEMS_BSP) (modules/libcom/RTEMS/Makefile:41) so
        // configuration can be BSP-conditional; kept for parity even though our
        // shim has no BSP conditional today.
        .define(&format!("BSP_{}", prefix.bsp), None)
        .warnings(true);

    // Not echoed here: `csrc/rtems_init.c` prints the line it parsed at boot
    // ("rtems-boot: boot command line (N argument(s)): [...]"), which is the
    // report that comes from the image that actually holds it.
    if let Some(cmdline) = boot_cmdline() {
        build.define(
            "EPICS_RTEMS_CMDLINE",
            Some(format!("\"{cmdline}\"").as_str()),
        );
    }

    // `cc` cannot guess a cross compiler for a tier-3 triple. An explicit
    // CC_armv7_rtems_eabihf wins if the operator set one; otherwise take the
    // driver from the same prefix everything else is derived from.
    if std::env::var_os("CC_armv7_rtems_eabihf").is_none()
        && std::env::var_os("CC_armv7-rtems-eabihf").is_none()
    {
        build.compiler(prefix.cc_path());
    }

    // The C objects must land in the same multilib as the Rust objects and the
    // RTEMS libraries.
    for flag in ABI_FLAGS {
        build.flag(flag);
    }

    build.compile("epics_rtems_boot_shim");

    // The non-propagating half, for this package's own executables — its test
    // harness is one, so `cargo test --target armv7-rtems-eabihf` links against
    // the same contract every IOC binary does. Same call a dependent IOC
    // package makes from its own build script.
    emit_link_args();

    // These two instructions are the ones that propagate to a dependent
    // binary's link (measured). `cc` has already emitted the search path and
    // `-l` for the shim archive itself.
    println!("cargo::rustc-link-search=native={lib_dir}");
    for lib in PRE_GROUP_LIBS {
        println!("cargo::rustc-link-lib={lib}");
    }

    println!("cargo::rustc-cfg=rtems_boot_linked");
}

/// The environment variable carrying the image's boot command line.
const CMDLINE_ENV: &str = "EPICS_RTEMS_CMDLINE";

/// The readiness-backend override, forwarded from the build environment into
/// that line. See [`boot_cmdline`].
const KQUEUE_ENV: &str = "EPICS_RTEMS_KQUEUE";

/// `csrc/rtems_init.c`'s `boot_cmdline` buffer. A longer line is refused here
/// rather than truncated, for the reason the C side refuses an oversized DHCP
/// value: half an address list is a wrong address list.
const BOOT_CMDLINE_CAPACITY: usize = 1024;

/// The boot command line to compile in, or `None` for the empty default.
///
/// `csrc/rtems_init.c` takes the line from the compile-time
/// `EPICS_RTEMS_CMDLINE` define, overridden at run time by the DHCP
/// `rtems_cmdline` option. There is no third source: QEMU's `-append` does not
/// reach it, and a variable exported in the build shell is the *host's*
/// environment, not the target's. So a define is what an image built outside a
/// DHCP site has, and this is where it is made.
///
/// `EPICS_RTEMS_KQUEUE` is forwarded because the prefix's generated
/// `epics-rs-env.sh` sets it (`scripts/rtems-bsp.sh`: a series-6 tree reports
/// 6.0.0, below the 6.3 the readiness gate needs, though the script asserted
/// the fixes are in the tree it built) — and the gate reads it in the target
/// process. Without this forward that export selected nothing; it only ever
/// configured the build host. It is prepended, so an assignment of the same
/// name inside `EPICS_RTEMS_CMDLINE` comes later and wins: `boot_args` applies
/// assignments in the order they appear.
fn boot_cmdline() -> Option<String> {
    let mut line = String::new();
    if let Ok(kqueue) = std::env::var(KQUEUE_ENV) {
        let kqueue = kqueue.trim();
        if !kqueue.is_empty() {
            line.push_str(&format!("{KQUEUE_ENV}={kqueue}"));
        }
    }
    if let Ok(extra) = std::env::var(CMDLINE_ENV) {
        let extra = extra.trim();
        if !extra.is_empty() {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(extra);
        }
    }
    if line.is_empty() {
        return None;
    }
    // The line becomes a C string literal. A quote or a backslash would end or
    // re-interpret it, so neither is escaped away into something the operator
    // did not write - it is refused.
    if let Some(bad) = line.chars().find(|c| matches!(c, '"' | '\\')) {
        panic!(
            "{CMDLINE_ENV}/{KQUEUE_ENV} produced a boot command line containing {bad:?}, \
             which cannot go through a C string literal: {line}"
        );
    }
    if line.len() >= BOOT_CMDLINE_CAPACITY {
        panic!(
            "boot command line is {} bytes; csrc/rtems_init.c's buffer is \
             {BOOT_CMDLINE_CAPACITY}: {line}",
            line.len()
        );
    }
    Some(line)
}
