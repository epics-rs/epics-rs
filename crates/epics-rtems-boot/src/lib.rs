//! The RTEMS boot shim and link contract for epics-rs IOCs.
//!
//! An RTEMS image is not a Rust binary with a different target: it is a Rust
//! binary plus a C entry task (`POSIX_Init`) that configures the kernel, brings
//! up libbsd and only then calls `main`. This crate owns that C code and the
//! link flags that go with it, once, for every IOC binary in the workspace —
//! `rtems-ca-ioc` today and `rtems-pva-ioc` when it exists. Duplicating a
//! `build.rs` and a copy of the C into each IOC crate would be two owners for
//! one boot contract, which is the shape that produces divergence.
//!
//! On every non-RTEMS target this crate is empty and its build script does
//! nothing, so depending on it costs a host build nothing.
//!
//! # Layout
//!
//! * [`contract`] — the link flags, pure and host-tested. Also the module a
//!   dependent's `build.rs` calls into.
//! * `csrc/rtems_config.c` — the `CONFIGURE_*` directives, derived from EPICS
//!   base's POSIX arm.
//! * `csrc/rtems_init.c` — `POSIX_Init`: console, clock, libbsd, DHCP, `main`.
//! * `build.rs` — compiles the two C files with `cc` and emits the propagating
//!   half of the contract.
//!
//! # Two build configurations, and why an unset prefix is not an error
//!
//! Building *for* RTEMS and *linking* an RTEMS image are different things, and
//! cargo cannot tell them apart: it resolves dependencies identically for
//! `cargo check` and `cargo build`. The workspace's portability gate is a
//! `cargo check --target armv7-rtems-eabihf` that runs on machines with no
//! cross toolchain at all, and it is the only RTEMS gate that works without the
//! bring-up box — so a build script that hard-failed on a missing toolchain
//! would delete that gate.
//!
//! So the presence of [`contract::BSP_PREFIX_ENV`] selects the configuration:
//!
//! | `RTEMS_BSP_PREFIX` | C shim | Link flags | `POSIX_Init` anchor | Result |
//! |---|---|---|---|---|
//! | set | compiled | emitted | emitted | a linkable image |
//! | unset | skipped | none | replaced by [`UNLINKABLE_MARKER`] | type-checks; **fails to link, by name** |
//!
//! The second row is the interesting one. It must not be silent: an image built
//! with no boot shim links cleanly and then boots to nothing, which costs a
//! serial-line bisect to diagnose. Instead the crate deliberately leaves one
//! undefined symbol in the object graph, whose name *is* the diagnosis:
//!
//! ```text
//! undefined reference to `epics_rtems_boot__RTEMS_BSP_PREFIX_was_not_set_at_build_time'
//! ```
//!
//! This is a deliberate deviation from `doc/rtems-boot-shim-design.md` §3.2,
//! which asks the build script to fail outright when the variable is unset.
//! That would satisfy §3.2 at the cost of §4.2's `rtems-check` job, and the
//! design says of that job that it "must keep running independently — it is the
//! only gate that works without the box". Moving the failure from build time to
//! link time keeps both: nothing that can only be caught by linking is
//! suppressed, and nothing that never links is broken.
//!
//! # Why the IOC binary must call [`link_anchor`]
//!
//! Measured: rustc forwards a dependency's `cargo::rustc-link-lib` entries to
//! the real linker only when the binary actually references that dependency —
//! an unreferenced rlib is not linked at all, and its `-lbsd -lm -lz` and the
//! shim's own archive vanish with it. `epics_rtems_boot::link_anchor()` in the
//! IOC's `main` is what pulls the rlib in. It compiles to nothing on the host.
//!
//! # What has not been proven
//!
//! No `arm-rtems6` toolchain exists on the development machine, so **nothing
//! here has ever been linked**. What is proven locally is the Rust side: both
//! configurations type-check for `armv7-rtems-eabihf`, and the emitted rlib
//! carries `U POSIX_Init` when linked and `U epics_rtems_boot__…` when not.
//!
//! Open, in the order the bring-up box will hit them:
//!
//! 1. **The C does not compile here.** Neither `csrc/` file has been through a
//!    compiler; the host tests guard their structure, not their syntax.
//! 2. **The include path is a guess.** [`contract::bsp_include_dir`] assumes the
//!    standard RTEMS 6 layout. Take the real `-I` set from a BSP sample's
//!    *compile* line.
//! 3. **The fd ceiling is a recommendation.** 150 is base's own score-arm value
//!    and our three crates make no `select`/`poll` call, but libbsd's internals
//!    were not audited and the confdefs macro may be spelled
//!    `CONFIGURE_LIBIO_MAXIMUM_FILE_DESCRIPTORS` in RTEMS 6.
//! 4. **`CONFIGURE_MAXIMUM_USER_EXTENSIONS 1` may be one too few** once
//!    `CONFIGURE_STACK_CHECKER_ENABLED` also claims capacity; base reserves 5.
//! 5. **Library resolution is untested.** `-lbsd -lm -lz` before the `-qrtems`
//!    group is the measured C order, but rustc also emits `-Bdynamic` and RTEMS
//!    has no shared libraries.
//! 6. **Interworking is reasoned, not observed.** A32 Rust objects calling into
//!    the Thumb multilib is standard Armv7-A behaviour; only a link proves the
//!    veneers resolve.

pub mod contract;

/// Undefined symbol deliberately left in this crate's object graph when it is
/// built for RTEMS without [`contract::BSP_PREFIX_ENV`].
///
/// Such a build type-checks — that is the whole point, the portability gate has
/// no toolchain — but must never silently produce an image with no boot shim in
/// it. Referencing a symbol that nobody defines makes the *link* fail with an
/// error that names its own cause, instead of producing an `.exe` that boots to
/// nothing over a serial line.
///
/// This is a property of the crate's own objects rather than of the link
/// contract, so it lives here and not in [`contract`].
pub const UNLINKABLE_MARKER: &str = "epics_rtems_boot__RTEMS_BSP_PREFIX_was_not_set_at_build_time";

/// Pulls this crate — and with it the boot shim and the RTEMS libraries — into
/// the binary's link.
///
/// Call it once, first thing in an RTEMS IOC's `main`. On a host target it is
/// an empty function and the whole crate is inert.
///
/// See the crate docs for why a plain `Cargo.toml` dependency is not enough.
#[inline(never)]
pub fn link_anchor() {
    #[cfg(target_os = "rtems")]
    rtems::anchor();
}

#[cfg(target_os = "rtems")]
mod rtems {
    /// The linked configuration: `POSIX_Init` exists, in the archive `cc` built
    /// from `csrc/`.
    ///
    /// `--gc-sections` is on for this target, and the entry task is referenced
    /// by nothing in the Rust object graph — it is called by the RTEMS kernel
    /// through a table `<rtems/confdefs.h>` generated. `-u POSIX_Init` on the
    /// link line is the primary defence; this `#[used]` static is the second,
    /// so the symbol survives even if that link argument is ever dropped.
    #[cfg(rtems_boot_linked)]
    pub(super) fn anchor() {
        use core::ffi::c_void;

        unsafe extern "C" {
            fn POSIX_Init(argument: *mut c_void) -> *mut c_void;
        }

        #[used]
        static POSIX_INIT: unsafe extern "C" fn(*mut c_void) -> *mut c_void = POSIX_Init;
    }

    /// The check-only configuration: no C was compiled, so there is no entry
    /// task. Leave a self-naming undefined symbol rather than let a shimless
    /// image link and boot to silence — see the crate docs.
    #[cfg(not(rtems_boot_linked))]
    pub(super) fn anchor() {
        unsafe extern "C" {
            #[link_name = "epics_rtems_boot__RTEMS_BSP_PREFIX_was_not_set_at_build_time"]
            fn missing_bsp_prefix();
        }

        #[used]
        static MISSING_BSP_PREFIX: unsafe extern "C" fn() = missing_bsp_prefix;
    }
}

#[cfg(test)]
mod tests {
    use super::UNLINKABLE_MARKER;
    use super::contract::*;

    /// The poison symbol's spelling is load-bearing: it is the entire error
    /// message a developer gets from a shimless link, and it appears in two
    /// places — the constant, and the `#[link_name]` in the `rtems` module.
    #[test]
    fn the_poison_symbol_names_the_variable_that_was_not_set() {
        assert!(UNLINKABLE_MARKER.contains(BSP_PREFIX_ENV));
        let src = include_str!("lib.rs");
        assert!(
            src.contains(&format!("#[link_name = \"{UNLINKABLE_MARKER}\"]")),
            "the link_name and UNLINKABLE_MARKER have drifted apart"
        );
    }

    /// `link_anchor` must stay callable with no `#[cfg]` at the call site: the
    /// IOC's `main` is shared between the host and RTEMS builds, and a target
    /// gate there would be a second place to keep in step.
    #[test]
    fn the_anchor_is_callable_on_the_host() {
        super::link_anchor();
    }

    /// Regression guard for the measured boot failure: a shim without
    /// `CONFIGURE_MAXIMUM_USER_EXTENSIONS` dies in libbsd's early init with
    /// `rtems_bsd_threads_init_early: cannot create extension`, because
    /// `CONFIGURE_UNLIMITED_OBJECTS` does not cover user extensions.
    #[test]
    fn the_shim_reserves_a_user_extension_for_libbsd() {
        let cfg = include_str!("../csrc/rtems_config.c");
        assert!(cfg.contains("#define CONFIGURE_MAXIMUM_USER_EXTENSIONS 1"));
        assert!(cfg.contains("#define CONFIGURE_UNLIMITED_OBJECTS"));
    }

    /// `<rtems/confdefs.h>` generates the object tables from the macros above
    /// it, so anything after it is ignored — silently.
    #[test]
    fn confdefs_is_the_last_thing_in_the_configuration() {
        let cfg = include_str!("../csrc/rtems_config.c");
        let tail = cfg
            .rsplit_once("#include <rtems/confdefs.h>")
            .expect("confdefs is included")
            .1;
        assert!(
            tail.trim().is_empty(),
            "these directives are generated away: {tail}"
        );
        let init_at = cfg.find("#define CONFIGURE_INIT").expect("CONFIGURE_INIT");
        let confdefs_at = cfg.find("#include <rtems/confdefs.h>").unwrap();
        assert!(init_at < confdefs_at);
    }

    /// The entry point is a three-way agreement — the configuration names it,
    /// the C defines it, and the link forces it. A rename that misses one of
    /// the three produces an image that boots to nothing.
    #[test]
    fn the_entry_point_agrees_across_the_configuration_the_c_and_the_link() {
        let cfg = include_str!("../csrc/rtems_config.c");
        let init = include_str!("../csrc/rtems_init.c");
        assert!(cfg.contains(&format!(
            "#define CONFIGURE_POSIX_INIT_THREAD_ENTRY_POINT {ENTRY_SYMBOL}"
        )));
        assert!(init.contains(&format!("void *{ENTRY_SYMBOL}(void *argument)")));
        assert!(link_args("/p", DEFAULT_BSP).contains(&ENTRY_SYMBOL.to_string()));
    }

    /// The shim's whole purpose is to reach Rust; base's own contract is the
    /// `main(argc, argv)` call at `rtems_init.c:1183`.
    #[test]
    fn the_entry_task_calls_main() {
        let init = include_str!("../csrc/rtems_init.c");
        assert!(init.contains("extern int main(int argc, char **argv);"));
        assert!(init.contains("result = main("));
        assert!(init.contains("rtems_bsd_initialize()"));
    }

    /// Every facility dropped in the design costs tasks, descriptors and image
    /// size; each is also a service with its own failure modes on a board we
    /// cannot debug interactively. Re-adding one should be a decision, not a
    /// paste.
    #[test]
    fn no_dropped_facility_crept_back_into_the_configuration() {
        let cfg = include_str!("../csrc/rtems_config.c");
        let production: String = cfg
            .lines()
            .filter(|l| l.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        for dropped in [
            "CONFIGURE_FILESYSTEM_NFS",
            "CONFIGURE_FILESYSTEM_TFTPFS",
            "RTEMS_BSD_CONFIG_SERVICE_TELNETD",
            "RTEMS_BSD_CONFIG_SERVICE_FTPD",
            "CONFIGURE_APPLICATION_NEEDS_LIBBLOCK",
            "CONFIGURE_APPLICATION_NEEDS_RTC_DRIVER",
        ] {
            assert!(
                !production.contains(dropped),
                "{dropped} was dropped by design; re-adding it needs a decision"
            );
        }
    }
}
