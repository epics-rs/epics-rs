//! The RTEMS link contract — every flag that turns a pile of Rust objects into
//! a bootable `.exe`, in one place.
//!
//! This module is deliberately **pure and target-neutral**: it computes strings
//! from an install prefix and never touches the filesystem, so it compiles and
//! is unit-tested on the host even though nothing here can be *linked* without
//! an `arm-rtems6` toolchain. It is compiled twice — once into this crate's
//! library (so a dependent's build script can call [`emit_link_args`]), and
//! once as a `#[path]` module of this crate's own `build.rs`. One source, two
//! consumers, no second copy of the flag list.
//!
//! # Why the contract is split across two build scripts
//!
//! Measured on this workspace (cargo 1.94.0), a build script's link output does
//! **not** all reach a dependent binary's link:
//!
//! | Instruction | Reaches a dependent binary? |
//! |---|---|
//! | `cargo::rustc-link-search` | **yes** — cargo puts `-L native=…` on the dependent's own `rustc` line |
//! | `cargo::rustc-link-lib` | **yes**, indirectly — rustc reads it from this crate's rlib metadata and forwards `-l…` to the real linker, *but only if the binary actually references this crate* |
//! | `cargo::rustc-link-arg` | **no** — it applies only to the emitting package's own targets, and an rlib performs no link |
//!
//! So the contract is delivered in two halves:
//!
//! * **Search paths and libraries** (`-L`, `-lbsd -lm -lz`) come from *this*
//!   crate's `build.rs`, because those two instructions propagate.
//! * **Link arguments** (the ABI/multilib selectors, `-B<bsp>/lib`, `-qrtems`,
//!   `--gc-sections`, `-u POSIX_Init`) cannot propagate, so the package that
//!   owns the IOC binary calls [`emit_link_args`] from its own three-line
//!   `build.rs`. The flags are still written down exactly once — here.
//!
//! The ordering that falls out of this is the one the measured C link needs:
//! `-lbsd -lm -lz` arrive as `rustc-link-lib` (early on rustc's link line) and
//! `-qrtems` arrives as a `rustc-link-arg` (late), so the three libraries
//! precede the `-qrtems` group rather than landing inside it.
//!
//! # How the Rust side is made to agree with the multilib
//!
//! The toolchain's RTEMS libraries live in the `thumb/armv7-a+simd/hard`
//! multilib, selected by `-march=armv7-a -mthumb -mfpu=neon -mfloat-abi=hard
//! -mtune=cortex-a9`. Agreement has three parts, and only the third is a
//! choice we make here:
//!
//! 1. **Rust's own code generation already matches on every ABI-significant
//!    axis.** The `armv7-rtems-eabihf` target spec is `abi = "eabihf"`,
//!    `llvm-floatabi = "hard"`, `features = "+thumb2,+neon,+vfp3"`,
//!    `llvm-target = "armv7-unknown-none-eabihf"` — hard-float VFP register
//!    passing, NEON (gcc's `+simd`), Armv7-A.
//! 2. **The one axis that differs is not an ABI axis.** rustc emits A32 (Arm)
//!    code, not Thumb, because the target has no `thumb-mode` feature. Armv7-A
//!    is an interworking architecture and AAPCS is identical for the two
//!    instruction sets, so A32 Rust objects call into Thumb library code
//!    through ordinary `BLX`/veneers. `-mthumb` is therefore a *multilib
//!    selector* for the C side, not a constraint on Rust's output.
//! 3. **The selectors must be on the link line**, because `arm-rtems6-gcc`
//!    chooses which multilib's `libgcc`/`libc`/`libbsd` to link from the flags
//!    it is given at link time. Omit them and gcc silently picks its default
//!    multilib, and the link dies with "uses VFP register arguments, … does
//!    not". [`ABI_FLAGS`] is emitted both as link arguments and as the C
//!    compile flags for the shim, so all three object families land in one
//!    multilib.
//!
//! [`check_abi`] turns part 1 from an assumption into a build-time assertion.

/// Environment variable naming the RTEMS/BSP install prefix.
///
/// Its value differs per machine, so it is never committed: `build.rs` reads it
/// and derives every path below from it. With it unset, this crate compiles in
/// check-only mode (see the crate docs).
pub const BSP_PREFIX_ENV: &str = "RTEMS_BSP_PREFIX";

/// Environment variable selecting the BSP, defaulting to [`DEFAULT_BSP`].
pub const BSP_ENV: &str = "RTEMS_BSP";

/// The BSP the acceptance ladder runs on (`doc/rtems-qemu-bringup-artefacts.md`).
pub const DEFAULT_BSP: &str = "xilinx_zynq_a9_qemu";

/// The toolchain target triple, as it appears in the install tree layout.
pub const TOOL_TARGET: &str = "arm-rtems6";

/// The cross compiler driver, which is also the linker (`.cargo/config.toml`).
pub const CC_NAME: &str = "arm-rtems6-gcc";

/// The RTEMS entry task. `POSIX_Init` because RTEMS >= 5 uses the POSIX API arm
/// (EPICS base `configure/toolchain.c:31-35`), which is also what Rust's
/// pthread-based `std::thread` requires.
pub const ENTRY_SYMBOL: &str = "POSIX_Init";

/// The multilib the RTEMS libraries live in, as a path fragment.
///
/// Recorded for the error message in [`check_abi`]; the path itself is never
/// constructed, because `-B` lets the gcc driver do the selecting.
pub const MULTILIB: &str = "thumb/armv7-a+simd/hard";

/// The flags that select [`MULTILIB`], measured from a real
/// `arm-rtems6-gcc -qrtems` expansion (`doc/rtems-qemu-bringup-artefacts.md`).
///
/// Applied to both the C shim's compilation and the final link so the C
/// objects, the Rust objects and the toolchain libraries agree.
pub const ABI_FLAGS: &[&str] = &[
    "-march=armv7-a",
    "-mthumb",
    "-mfpu=neon",
    "-mfloat-abi=hard",
    "-mtune=cortex-a9",
];

/// Libraries that must precede the `-qrtems` group, in this order.
///
/// Measured: the real C link is `… <objs> -lbsd -lm -lz --start-group -lgcc
/// --start-group -lrtemsbsp -lrtemscpu -latomic -lc -lgcc --end-group
/// --end-group`. These three sit *outside* the group, ahead of it.
pub const PRE_GROUP_LIBS: &[&str] = &["bsd", "m", "z"];

/// `<prefix>/arm-rtems6/<bsp>/lib` — the BSP library directory.
///
/// This is the `-B` prefix, and passing it as `-B` is what supplies both the
/// BSP's `-L` and its `-T linkcmds` (measured). The linker script is therefore
/// never named explicitly; naming it ourselves would be a second, divergent
/// source of truth for something the driver already knows.
pub fn bsp_lib_dir(prefix: &str, bsp: &str) -> String {
    format!("{prefix}/{TOOL_TARGET}/{bsp}/lib")
}

/// `<prefix>/arm-rtems6/<bsp>/lib/include` — where the BSP and libbsd headers
/// the shim includes (`<rtems.h>`, `<machine/rtems-bsd-config.h>`,
/// `<rtems/netcmds-config.h>`, `<rtems/shellconfig.h>`) are installed.
///
/// Verified on the bring-up box: a translation unit including all four of
/// `<rtems.h>`, `<machine/rtems-bsd-config.h>`, `<rtems/netcmds-config.h>` and
/// `<rtems/shellconfig.h>` compiles with this directory as the *only*
/// `-isystem` (`arm-rtems6-gcc -fsyntax-only`, exit 0).
///
/// One caveat for anyone re-deriving this: the RTEMS kernel's own waf build
/// compiles out of its source tree, so a BSP sample's compile line shows
/// in-tree `-I`s and is *not* evidence about the installed layout. libbsd is
/// the right analogue, because it builds against the installed BSP as we do.
pub fn bsp_include_dir(prefix: &str, bsp: &str) -> String {
    format!("{}/include", bsp_lib_dir(prefix, bsp))
}

/// The full ordered list of link arguments for an RTEMS IOC binary.
///
/// Each entry becomes one `cargo::rustc-link-arg`. `-u` and [`ENTRY_SYMBOL`]
/// are two entries because the gcc driver takes them as two arguments.
pub fn link_args(prefix: &str, bsp: &str) -> Vec<String> {
    let mut args: Vec<String> = ABI_FLAGS.iter().map(|s| (*s).to_string()).collect();
    // Supplies the BSP `-L` *and* the `-T linkcmds`, so neither is named here.
    args.push(format!("-B{}", bsp_lib_dir(prefix, bsp)));
    // Expands to the crt objects, the BSP search paths and the
    // `-lrtemsbsp -lrtemscpu -latomic -lc -lgcc` group.
    args.push("-qrtems".to_string());
    args.push("-Wl,--gc-sections".to_string());
    // Redundant by measurement — the `-qrtems` expansion already begins with
    // `-u POSIX_Init` — and kept anyway: `--gc-sections` is on, the shim lives
    // in an archive, and an entry task garbage-collected out of the image
    // produces a board that boots to silence. The cost of the redundancy is two
    // arguments; the cost of being wrong is a serial-line bisect.
    args.push("-u".to_string());
    args.push(ENTRY_SYMBOL.to_string());
    args
}

/// Asserts that rustc is generating code for the multilib the RTEMS libraries
/// were built for.
///
/// `abi` is `CARGO_CFG_TARGET_ABI` and `features` is the comma-separated
/// `CARGO_CFG_TARGET_FEATURE`. Both come from the compiler, so this catches a
/// wrong triple or a `-C target-feature=-neon` at build time rather than at
/// link time, where the message would be about VFP register arguments.
pub fn check_abi(abi: &str, features: &str) -> Result<(), String> {
    if abi != "eabihf" {
        return Err(format!(
            "target ABI is `{abi}`, but the RTEMS libraries are in the `{MULTILIB}` \
             multilib and pass floats in VFP registers; build for \
             `armv7-rtems-eabihf` (ABI `eabihf`)"
        ));
    }
    if !features.split(',').any(|f| f == "neon") {
        return Err(format!(
            "target feature `neon` is not enabled, but the RTEMS libraries are in \
             the `{MULTILIB}` multilib (`+simd` is NEON); remove whatever disables \
             it from RUSTFLAGS"
        ));
    }
    Ok(())
}

/// Emits the half of the contract that a build script must own itself —
/// the link arguments — and nothing else.
///
/// Call this from the `build.rs` of any package that produces an RTEMS IOC
/// binary. It no-ops on every non-RTEMS target, so a host build of the same
/// package is untouched.
///
/// # Panics
///
/// On an RTEMS target with [`BSP_PREFIX_ENV`] set to a path that is not a
/// directory. An unset variable is *not* an error: that is the portability
/// check configuration, which type-checks without a toolchain.
pub fn emit_link_args() {
    println!("cargo::rerun-if-env-changed={BSP_PREFIX_ENV}");
    println!("cargo::rerun-if-env-changed={BSP_ENV}");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("rtems") {
        return;
    }
    let Some((prefix, bsp)) = resolve_prefix() else {
        return;
    };
    for arg in link_args(&prefix, &bsp) {
        println!("cargo::rustc-link-arg={arg}");
    }
}

/// Reads [`BSP_PREFIX_ENV`]/[`BSP_ENV`], returning `None` in check-only mode.
///
/// # Panics
///
/// If the prefix is set but is not a directory — that is a misconfiguration
/// with a fixable cause, and reporting it here is far cheaper than letting
/// `cc` fail with a missing-compiler message or the linker with a missing
/// `linkcmds`.
pub fn resolve_prefix() -> Option<(String, String)> {
    let prefix = std::env::var(BSP_PREFIX_ENV)
        .ok()
        .filter(|p| !p.is_empty())?;
    assert!(
        std::path::Path::new(&prefix).is_dir(),
        "{BSP_PREFIX_ENV}=\"{prefix}\" is not a directory. It must point at the \
         RTEMS install prefix that contains {TOOL_TARGET}/<bsp>/lib and bin/{CC_NAME}."
    );
    let bsp = std::env::var(BSP_ENV)
        .ok()
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| DEFAULT_BSP.to_string());
    Some((prefix, bsp))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREFIX: &str = "/opt/rtems-probe";

    /// The measured values of the real target spec
    /// (`rustc --print cfg --target armv7-rtems-eabihf`).
    #[test]
    fn the_real_rtems_target_agrees_with_the_multilib() {
        assert_eq!(
            check_abi("eabihf", "aclass,d32,dsp,fpregs,neon,thumb2,v7,vfp2,vfp3"),
            Ok(())
        );
    }

    #[test]
    fn a_soft_float_abi_is_rejected_by_name() {
        let err = check_abi("eabi", "neon").unwrap_err();
        assert!(err.contains("eabi"), "{err}");
        assert!(err.contains(MULTILIB), "{err}");
    }

    #[test]
    fn a_neon_less_build_is_rejected_by_name() {
        let err = check_abi("eabihf", "aclass,thumb2,vfp2").unwrap_err();
        assert!(err.contains("neon"), "{err}");
        assert!(err.contains(MULTILIB), "{err}");
    }

    /// `neon` must match as a whole feature, not as a substring of another.
    #[test]
    fn feature_matching_is_exact() {
        assert!(check_abi("eabihf", "neonfp,xneon").is_err());
    }

    /// The gcc driver selects the multilib from flags it sees; a selector that
    /// arrives after the `-qrtems` expansion has already been processed is a
    /// selector that did not select anything.
    #[test]
    fn the_multilib_selectors_precede_the_rtems_group() {
        let args = link_args(PREFIX, DEFAULT_BSP);
        let qrtems = args.iter().position(|a| a == "-qrtems").expect("-qrtems");
        for flag in ABI_FLAGS {
            let at = args.iter().position(|a| a == flag).expect(flag);
            assert!(at < qrtems, "{flag} lands after -qrtems");
        }
    }

    /// Measured consequence 2: `-B<bsp>/lib` supplies the linker script, so
    /// naming it ourselves would create a second source of truth that a BSP
    /// change would silently desynchronise.
    #[test]
    fn the_linker_script_is_never_named() {
        let args = link_args(PREFIX, DEFAULT_BSP);
        assert!(
            args.iter()
                .any(|a| a == &format!("-B{PREFIX}/arm-rtems6/{DEFAULT_BSP}/lib"))
        );
        for a in &args {
            assert!(!a.contains("linkcmds"), "names the linker script: {a}");
            assert!(a != "-T" && !a.starts_with("-T"), "passes -T: {a}");
        }
    }

    /// `--gc-sections` plus an entry task nothing references is an image that
    /// boots to silence; this is the belt half of the belt-and-braces.
    #[test]
    fn the_entry_symbol_is_forced_as_two_arguments() {
        let args = link_args(PREFIX, DEFAULT_BSP);
        let u = args.iter().position(|a| a == "-u").expect("-u");
        assert_eq!(args[u + 1], ENTRY_SYMBOL);
        assert!(args.iter().any(|a| a == "-Wl,--gc-sections"));
    }

    #[test]
    fn the_pre_group_libraries_are_the_measured_three() {
        assert_eq!(PRE_GROUP_LIBS, ["bsd", "m", "z"]);
    }

    #[test]
    fn every_path_is_derived_from_the_prefix() {
        assert_eq!(
            bsp_lib_dir(PREFIX, DEFAULT_BSP),
            "/opt/rtems-probe/arm-rtems6/xilinx_zynq_a9_qemu/lib"
        );
        assert_eq!(
            bsp_include_dir(PREFIX, DEFAULT_BSP),
            "/opt/rtems-probe/arm-rtems6/xilinx_zynq_a9_qemu/lib/include"
        );
        for a in link_args(PREFIX, DEFAULT_BSP) {
            assert!(
                !a.contains("/home/") && !a.contains("$HOME"),
                "a machine-specific path leaked into the contract: {a}"
            );
        }
    }

    /// The contract source itself must stay machine-independent — the whole
    /// reason the prefix is an environment variable.
    #[test]
    fn the_contract_source_hard_codes_no_install_path() {
        let src = include_str!("contract.rs");
        let production: &str = src.split("\n#[cfg(test)]").next().expect("test module");
        for needle in ["/home/", "$HOME", "/opt/rtems", "rtems-bringup"] {
            assert!(
                !production.contains(needle),
                "production source contains a machine-specific path: {needle}"
            );
        }
    }
}
