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
    println!("cargo::rerun-if-changed=csrc/rtems_stats.c");
    println!("cargo::rerun-if-changed=src/contract.rs");
    println!("cargo::rerun-if-env-changed={BSP_PREFIX_ENV}");
    println!("cargo::rerun-if-env-changed={BSP_ENV}");

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
        .file("csrc/rtems_stats.c")
        .include(&include_dir)
        // Base passes -DBSP_$(RTEMS_BSP) (modules/libcom/RTEMS/Makefile:41) so
        // configuration can be BSP-conditional; kept for parity even though our
        // shim has no BSP conditional today.
        .define(&format!("BSP_{}", prefix.bsp), None)
        .warnings(true);

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
