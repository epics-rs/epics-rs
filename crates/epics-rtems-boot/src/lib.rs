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

/// Whether this build's `libc` describes the target's time types correctly.
///
/// **False on a stock `libc`.** The crate types `time_t` as `i32` for every
/// newlib target except `horizon`/`espidf` (`src/unix/newlib/mod.rs`), while
/// the `arm-rtems6` toolchain has `sizeof(time_t) == 8`, signed. Measured on
/// this target with the stock crate: `size_of::<libc::time_t>() == 4` and
/// `size_of::<libc::timespec>() == 8`, where the target's `timespec` is 16
/// with `tv_nsec` at offset 8.
///
/// # Why this is a boot-blocking defect and not a 4-byte overrun
///
/// `std` compiles the same definition. RTEMS `clock_gettime` writes 12 bytes
/// into the 8-byte struct `std` allocated, so `tv_nsec` is read from the *high
/// word* of the kernel's 64-bit `tv_sec` and the real nanoseconds land out of
/// bounds. That is why the reading is a clean zero rather than garbage.
/// Confirmed on target:
///
/// * `SystemTime::now().subsec_nanos()` reads exactly 0, every time.
/// * `Instant::elapsed()` over a spin loop reads 0 ns.
///
/// So every timeout, rate limit, backoff and scan period built on `Instant`
/// silently collapses to zero elapsed time. Nothing errors and nothing logs;
/// the IOC simply stops honouring time.
///
/// # The workaround
///
/// Patch `libc` so `time_t` is `c_longlong` on this target — the other
/// measured-wrong types (`dev_t`, `ino_t`, `rlim_t`, all 4 where the target
/// has 8) fall out of the same change — and build `std` against the patched
/// source. A `[patch.crates-io]` in this workspace fixes *our* dependency but
/// **not** the copy `-Zbuild-std` compiles for `std`, and `std` is where the
/// timer damage happens. Both have to be patched.
///
/// Derived layouts the target actually has, for whoever does that work:
/// `timespec` 16/align 8 with `tv_nsec` at 8, `timeval` 16/align 8,
/// `itimerspec` 32, `struct stat` 104 with `st_size` at 40 and `st_atim` at 48.
///
/// # What this can and cannot see
///
/// It reads *this workspace's* `libc`, which is a **proxy** for `std`'s: under
/// `-Zbuild-std` those are two separate compilations and can in principle
/// resolve different versions. It is the closest observable stand-in — `std`
/// exposes no layout of its own to assert on — and it is a live one, because
/// this workspace pins no patched `libc` (no `[patch]`/`[replace]`), so a
/// stock resolution makes this `false` today.
///
/// Only `time_t` and `timespec` are checked. `dev_t`, `ino_t` and `rlim_t` are
/// equally wrong but appear nowhere in `epics-base-rs`, `epics-ca-rs` or
/// `epics-pva-rs`; asserting types we never touch would make this a `libc`
/// conformance suite that fails for reasons unrelated to us. Add one when the
/// first use appears.
#[cfg(target_os = "rtems")]
pub const RTEMS_LIBC_TIME_LAYOUT_IS_CORRECT: bool = size_of::<libc::time_t>() == 8
    && size_of::<libc::timespec>() == 16
    && align_of::<libc::timespec>() == 8
    // At offset 4 this field reads the high word of the kernel's 64-bit
    // `tv_sec`, which is what turns the bug from garbage into a clean zero.
    && core::mem::offset_of!(libc::timespec, tv_nsec) == 8;

/// The refusal. Sited on the same `rtems_boot_linked` arm as the boot shim
/// itself, so **bootable implies checked, by construction**: an image built
/// without [`contract::BSP_PREFIX_ENV`] has no `POSIX_Init` and cannot link at
/// all (see [`UNLINKABLE_MARKER`]), so there is no path to a running IOC that
/// skips this.
///
/// The predicate above is deliberately *outside* this gate. It is type-checked
/// by the ordinary `cargo check --target armv7-rtems-eabihf` portability gate
/// on a machine with no toolchain, so it cannot rot unnoticed; only the
/// build-stopping `assert!` is scoped to builds that can produce an image.
/// Putting the assertion itself at that scope would delete that gate — the
/// same trade this crate already makes for the missing toolchain, and for the
/// same reason (`doc/rtems-boot-shim-design.md` §4.2: the portability check
/// "is the only gate that works without the box").
#[cfg(all(target_os = "rtems", rtems_boot_linked))]
const _RTEMS_LIBC_TIME_LAYOUT: () = assert!(
    RTEMS_LIBC_TIME_LAYOUT_IS_CORRECT,
    "RTEMS libc layout bug: this build's `libc` types `time_t` as i32, but \
     arm-rtems6 has sizeof(time_t) == 8, making `timespec` 8 bytes where the \
     target's is 16 with tv_nsec at offset 8. `std` compiles the same \
     definition, so RTEMS clock_gettime overruns std's timespec and \
     `Instant`/`SystemTime` become SILENTLY ZERO-RESOLUTION on target: \
     subsec_nanos() reads exactly 0 and elapsed() reads 0 ns, so every \
     timeout, rate limit, backoff and scan period in the IOC collapses to \
     zero elapsed time with no error and no log line. This refuses to build \
     because that failure is invisible at runtime. WORKAROUND: patch libc's \
     time_t to c_longlong for this target, in BOTH this workspace and the \
     copy -Zbuild-std compiles for std -- patching only one leaves std broken. \
     See RTEMS_LIBC_TIME_LAYOUT_IS_CORRECT for the measured layouts."
);

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

    /// This file, with the test module cut off.
    ///
    /// The layout guard is `#[cfg(target_os = "rtems")]`, so no host test can
    /// evaluate it and the only thing host CI can defend is its source. That
    /// makes self-matching the hazard: a test that searched the whole file
    /// would find its own needles and pass vacuously forever.
    fn production_source() -> &'static str {
        let src = include_str!("lib.rs");
        let marker = "\n#[cfg(test)]\n";
        assert_eq!(
            src.matches(marker).count(),
            1,
            "a second top-level #[cfg(test)] would make this slice the wrong prefix"
        );
        src.split(marker).next().unwrap()
    }

    /// One item's source, from its declaration to the terminating `;`.
    ///
    /// Every content guard below searches an *item*, never the file. The doc
    /// comments above these two items quote the same tokens the guards look
    /// for — `c_longlong`, and `size_of::<libc::timespec>() == 8` as the
    /// *wrong* value — so a whole-file search passes on the prose while the
    /// code says the opposite. Measured, not theorised: deleting the
    /// workaround from the refusal message left the message guard green until
    /// it was scoped this way.
    fn item_body(declaration: &str) -> &'static str {
        let (_, body) = production_source()
            .split_once(declaration)
            .unwrap_or_else(|| panic!("`{declaration}` is gone from lib.rs"));
        body.split_once(';')
            .unwrap_or_else(|| panic!("`{declaration}` has no terminating `;`"))
            .0
    }

    /// The refusal must stay on the *linkable-image* arm — not narrower, not
    /// wider.
    ///
    /// Narrower (an added `feature = …`, or a `not(…)`) lets a bootable image
    /// through with zero-resolution `Instant`. Wider (dropping
    /// `rtems_boot_linked`) deletes the toolchain-free portability gate, which
    /// is the one gate that runs without the bring-up box. Both directions are
    /// regressions, so the cfg is pinned exactly.
    #[test]
    fn the_libc_layout_refusal_fires_for_every_image_that_can_boot() {
        let src = production_source();
        let cfg = concat!("#[cfg(all(target_os = \"rtems\", ", "rtems_boot_linked))]");
        assert_eq!(
            src.matches(cfg).count(),
            1,
            "the libc layout refusal must carry exactly `{cfg}`: an image that \
             links has a boot shim, and an image without one cannot link at all"
        );
        let (_, after) = src.split_once(cfg).unwrap();
        assert!(
            after
                .trim_start()
                .starts_with("const _RTEMS_LIBC_TIME_LAYOUT: () = assert!("),
            "that cfg must guard the layout assertion and nothing else"
        );
    }

    /// The measured target layout, pinned as numbers.
    ///
    /// A host cannot name `libc::timespec` for `arm-rtems6`, so these are the
    /// values from the bring-up box written down. Weakening the predicate to
    /// match the broken `libc` (`== 8`, `== 4`) is the mutation that silently
    /// restores the defect, and it is exactly what this catches.
    #[test]
    fn the_predicate_pins_the_layout_the_target_actually_has() {
        let src = item_body("pub const RTEMS_LIBC_TIME_LAYOUT_IS_CORRECT: bool =");
        for required in [
            "size_of::<libc::time_t>() == 8",
            "size_of::<libc::timespec>() == 16",
            "align_of::<libc::timespec>() == 8",
            "offset_of!(libc::timespec, tv_nsec) == 8",
        ] {
            assert!(
                src.contains(required),
                "the RTEMS libc layout predicate lost `{required}`; the target's \
                 timespec is 16/align 8 with tv_nsec at 8 and time_t is 8 signed"
            );
        }
    }

    /// The message is the whole deliverable: whoever hits this is holding a
    /// hard build error and must not have to rediscover a week of bring-up.
    #[test]
    fn the_refusal_message_names_the_defect_and_the_way_out() {
        let src = item_body("const _RTEMS_LIBC_TIME_LAYOUT: () = assert!(");
        for required in [
            // the consequence, in the words that make it searchable
            "ZERO-RESOLUTION",
            "subsec_nanos",
            "elapsed",
            // the workaround, and the half of it that is easy to miss
            "c_longlong",
            "build-std",
        ] {
            assert!(
                src.contains(required),
                "the RTEMS libc layout refusal message must still name `{required}`"
            );
        }
    }

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

    /// Regression guard for the measured *link* failure, the sibling of the one
    /// above: without `CONFIGURE_APPLICATION_NEEDS_LIBBLOCK` the link dies with
    /// `undefined reference to rtems_bdbuf_configuration` from
    /// `librtemscpu.a(bdbuf.c.70.o)`. Omitting the directive does not drop
    /// libblock from the image, only its configuration, and
    /// `RTEMS_BSD_CONFIG_BSP_CONFIG` pulls in this BSP's nexus devices —
    /// including its two SDHCI controllers, whose SD/MMC stack references bdbuf
    /// unconditionally. `confdefs/bdbuf.h:54,133` defines the symbol only under
    /// this macro.
    #[test]
    fn the_shim_configures_libblock_for_the_bsps_block_devices() {
        let cfg = include_str!("../csrc/rtems_config.c");
        assert!(cfg.contains("#define CONFIGURE_APPLICATION_NEEDS_LIBBLOCK"));
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
    ///
    /// `CONFIGURE_APPLICATION_NEEDS_LIBBLOCK` was on this list and is
    /// deliberately no longer: this guard fired on the first cross-toolchain
    /// link and demanded the decision it exists to demand. The decision, with
    /// the measurement, is at the directive in `rtems_config.c` — omitting it
    /// does not drop libblock from the image, only its *configuration*, and the
    /// BSP's nexus devices reference `rtems_bdbuf_configuration` unconditionally.
    /// `the_shim_configures_libblock_for_the_bsps_block_devices` now guards it
    /// from the other direction, so removing it is caught too.
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
            "CONFIGURE_APPLICATION_NEEDS_RTC_DRIVER",
        ] {
            assert!(
                !production.contains(dropped),
                "{dropped} was dropped by design; re-adding it needs a decision"
            );
        }
    }
}
