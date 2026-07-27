//! The RTEMS boot shim and link contract for epics-rs IOCs.
//!
//! An RTEMS image is not a Rust binary with a different target: it is a Rust
//! binary plus a C entry task (`POSIX_Init`) that configures the kernel, brings
//! up libbsd and only then calls `main`. This crate owns that C code and the
//! link flags that go with it, once, for every IOC binary in the workspace —
//! `realtime-ca-ioc` today and `realtime-pva-ioc` when it exists. Duplicating a
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
//!   base's POSIX arm. Its file-descriptor ceiling deviates from that arm — we
//!   take 150 from base's *score* arm where the POSIX arm base actually
//!   compiles on RTEMS 6 says 64 — and that ceiling is what caps concurrent
//!   clients; `doc/rtems-fd-ceiling-deviation.md` is the measured record.
//! * `csrc/rtems_init.c` — `POSIX_Init`: console, clock, libbsd, DHCP, `main`.
//! * [`stats`] — descriptor and heap usage, the two IOC-statistics values Rust
//!   cannot reach on this target, over `csrc/rtems_stats.c`.
//! * `build.rs` — compiles the C files with `cc` and emits the propagating
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
//! 1. **The C does not compile here.** `csrc/rtems_config.c` and
//!    `csrc/rtems_init.c` have never been through a compiler on this machine;
//!    the host tests guard their structure, not their syntax.
//!    `csrc/rtems_stats.c` is the exception and the pattern to copy:
//!    `arm-rtems6-gcc -c -march=armv7-a -mthumb -mfpu=neon -mfloat-abi=hard
//!    -mtune=cortex-a9 -Wall -Wextra` against the bring-up box's installed
//!    `xilinx_zynq_a9_qemu` headers exits 0 with no diagnostics and defines
//!    both of its symbols. That is why its two deviations from devIocStats are
//!    stated as measured rather than as reasoning.
//! 2. **The include path is a guess.** [`contract::bsp_include_dir`] assumes the
//!    standard RTEMS 6 layout. Take the real `-I` set from a BSP sample's
//!    *compile* line.
//! 3. **The fd ceiling — mostly closed; one audit still owed.**
//!    ~~150 is base's own score-arm value and our three crates make no
//!    `select`/`poll` call, but … the confdefs macro may be spelled
//!    `CONFIGURE_LIBIO_MAXIMUM_FILE_DESCRIPTORS` in RTEMS 6.~~ **Closed on the
//!    box**, and `csrc/rtems_config.c` §F is where the evidence lives:
//!    `CONFIGURE_MAXIMUM_FILE_DESCRIPTORS` is the RTEMS 6 spelling
//!    (`confdefs/libio.h:89` reads it; `confdefs/obsolete.h:109-111` makes the
//!    older name a rename `#warning`), the ceiling is measured at 142
//!    concurrent CA connections with the 143rd refused `ENFILE`, and
//!    `FD_SETSIZE` on this BSP is 256 rather than newlib's default 64, so
//!    base's `select()` caveat cannot fire at a cap of 150 whatever any library
//!    does. The deviation this leaves — we run base's *score*-arm 150 on a
//!    target where base compiles the *POSIX* arm and runs 64, and the cap is
//!    overridable through the `#ifndef` — is recorded with every measurement in
//!    `doc/rtems-fd-ceiling-deviation.md`.
//!    **Still genuinely open:** *libbsd's internals were never audited for
//!    `select()` use.* That does not bind at 150 (`FD_SETSIZE` is 256), so it
//!    is a precondition for raising the cap **above 256**, not a live risk
//!    today — and the memory wall makes such a cap useless on this guest
//!    anyway.
//! 4. **`CONFIGURE_MAXIMUM_USER_EXTENSIONS 1` may be one too few** once
//!    `CONFIGURE_STACK_CHECKER_ENABLED` also claims capacity; base reserves 5.
//! 5. **Library resolution is untested.** `-lbsd -lm -lz` before the `-qrtems`
//!    group is the measured C order, but rustc also emits `-Bdynamic` and RTEMS
//!    has no shared libraries.
//! 6. **Interworking is reasoned, not observed.** A32 Rust objects calling into
//!    the Thumb multilib is standard Armv7-A behaviour; only a link proves the
//!    veneers resolve.

pub mod contract;
pub mod stats;

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
/// # Which time types are checked
///
/// Both that the closure passes across the C boundary:
///
/// * `timespec` — `clock_gettime` under every `Instant`/`SystemTime` read, and
///   `pthread_cond_timedwait` under `Condvar::wait_timeout`, which is the one
///   production timed wait in the RTEMS closure
///   (`runtime/background/delayed_timer.rs`).
/// * `timeval` — `std` passes `optlen = size_of::<libc::timeval>()` to
///   `setsockopt` for `SO_RCVTIMEO`/`SO_SNDTIMEO`
///   (`std/src/sys/net/connection/socket/unix.rs`), and **both** blocking
///   drivers call `set_read_timeout(...)?` before serving a client
///   (`epics-ca-rs` `server/blocking.rs`, `epics-pva-rs`
///   `server_native/blocking.rs`). A short `timeval` either fails every
///   connection at setup or silently loses the timeout bound. We never name
///   the type; `std` names it for us, which is exactly why a guard is the only
///   place this becomes visible.
///
/// `dev_t`, `ino_t` and `rlim_t` are equally wrong but appear nowhere in
/// `epics-base-rs`, `epics-ca-rs` or `epics-pva-rs`; asserting types we never
/// touch would make this a `libc` conformance suite that fails for reasons
/// unrelated to us. Add one when the first use appears.
#[cfg(target_os = "rtems")]
pub const RTEMS_LIBC_TIME_LAYOUT_IS_CORRECT: bool = size_of::<libc::time_t>() == 8
    && size_of::<libc::timespec>() == 16
    && align_of::<libc::timespec>() == 8
    // At offset 4 this field reads the high word of the kernel's 64-bit
    // `tv_sec`, which is what turns the bug from garbage into a clean zero.
    && core::mem::offset_of!(libc::timespec, tv_nsec) == 8
    // Same root cause, different struct: with `time_t` as i32 this is 8 bytes
    // where the target's is 16, so the `optlen` std hands `setsockopt` is half
    // what the kernel reads.
    && size_of::<libc::timeval>() == 16
    && align_of::<libc::timeval>() == 8
    && core::mem::offset_of!(libc::timeval, tv_usec) == 8;

/// Whether this build's `libc` gives the socket address structs their BSD
/// length byte.
///
/// **False on a stock `libc`.** `src/unix/newlib/arm/mod.rs` defines
/// `sockaddr_in` as `{ sin_family, sin_port, sin_addr, sin_zero }` with no
/// `sin_len`, while `src/unix/newlib/aarch64/mod.rs` — the *same* crate, the
/// same `target_env = "newlib"`, the other RTEMS architecture — defines it as
/// `{ sin_len, sin_family, sin_port, sin_addr, sin_zero }`. The arm arm is the
/// odd one out, and RTEMS 6 networking is rtems-libbsd, whose `sockaddr_in`
/// carries `sin_len`.
///
/// # What that costs at runtime
///
/// `sa_family_t` is `u8` here, so the two layouts are the same 16 bytes and
/// differ only in where the family lives: offset 0 for us, offset 1 for the
/// kernel. We therefore write the address family into the kernel's *length*
/// byte and leave the kernel's family byte as uninitialised padding.
/// Measured on target: `bind()` succeeds, and then `local_addr()`, `accept()`
/// and `recv_from()` all fail with `InvalidInput` and **no** OS error — `std`
/// reads a family byte that was never written and refuses to decode the
/// address. An IOC that cannot report its own bound address cannot answer a
/// CA search or a PVA beacon.
///
/// The check is on the family's *offset*, not on the presence of `sin_len`:
/// naming a field that does not exist is a compile error, which would take the
/// toolchain-free portability gate down with it (see the note on
/// `_RTEMS_LIBC_TIME_LAYOUT`). An offset of 1 says a length byte precedes
/// the family, which is the property that matters.
///
/// # Which socket types are checked
///
/// The three the closure passes across the boundary by value or by pointer:
/// `sockaddr_in` (raw UDP `bind` in both blocking servers, and the CA
/// broadcast address list), `sockaddr_in6` and `sockaddr_storage`
/// (`recvmsg` destination-address recovery in `epics-base-rs` `net/` and
/// `epics-pva-rs` `client_native/udp.rs`).
///
/// Not checked, and why: `msghdr`, `cmsghdr`, `iovec`, `in_pktinfo`,
/// `in6_pktinfo`, `ifaddrs`, `sched_param`, `pthread_attr_t`,
/// `pthread_mutex_t` and `pthread_mutexattr_t` are all named by the closure,
/// but no measured target layout for them exists — only `libc`'s own, which is
/// the thing under suspicion. Asserting numbers nobody has measured would
/// encode a guess as a build gate. Add each when the box reports its layout.
#[cfg(target_os = "rtems")]
pub const RTEMS_LIBC_SOCKET_LAYOUT_IS_CORRECT: bool =
    // The anchor: the family is one byte, so an offset of 1 can only mean a
    // length byte sits in front of it.
    size_of::<libc::sa_family_t>() == 1
        && size_of::<libc::sockaddr_in>() == 16
        && core::mem::offset_of!(libc::sockaddr_in, sin_family) == 1
        && core::mem::offset_of!(libc::sockaddr_in6, sin6_family) == 1
        && core::mem::offset_of!(libc::sockaddr_storage, ss_family) == 1;

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

/// The socket-layout refusal, on the same arm and for the same reason.
#[cfg(all(target_os = "rtems", rtems_boot_linked))]
const _RTEMS_LIBC_SOCKET_LAYOUT: () = assert!(
    RTEMS_LIBC_SOCKET_LAYOUT_IS_CORRECT,
    "RTEMS libc layout bug: this build's `libc` defines the socket address \
     structs without their BSD length byte (src/unix/newlib/arm/mod.rs), while \
     the same crate's aarch64 arm -- the other RTEMS architecture -- defines \
     `sockaddr_in` WITH `sin_len`, and RTEMS 6 networking is rtems-libbsd. \
     `sa_family_t` is one byte, so the layouts are the same size and differ \
     only in position: we write the address family into the kernel's LENGTH \
     byte and leave the kernel's family byte uninitialised. Measured on \
     target: bind() succeeds, then local_addr(), accept() and recv_from() all \
     fail with InvalidInput and NO OS error, so the IOC cannot report its own \
     bound address and cannot answer a CA search or a PVA beacon. WORKAROUND: \
     patch libc's newlib/arm `sockaddr_in`/`sockaddr_in6`/`sockaddr_storage` \
     to carry the leading length byte, in BOTH this workspace and the copy \
     -Zbuild-std compiles for std. See RTEMS_LIBC_SOCKET_LAYOUT_IS_CORRECT."
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

    /// Every refusal must stay on the *linkable-image* arm — not narrower, not
    /// wider.
    ///
    /// Narrower (an added `feature = …`, or a `not(…)`) lets a bootable image
    /// through with zero-resolution `Instant` or an unreportable bound address.
    /// Wider (dropping `rtems_boot_linked`) deletes the toolchain-free
    /// portability gate, which is the one gate that runs without the bring-up
    /// box. Both directions are regressions, so the cfg is pinned exactly — and
    /// each occurrence of it must guard a refusal, nothing else.
    #[test]
    fn the_libc_layout_refusals_fire_for_every_image_that_can_boot() {
        let src = production_source();
        let cfg = concat!("#[cfg(all(target_os = \"rtems\", ", "rtems_boot_linked))]");
        let guarded: Vec<&str> = src
            .split(cfg)
            .skip(1)
            .map(|after| after.trim_start().split_once(':').unwrap().0)
            .collect();
        assert_eq!(
            guarded,
            vec![
                "const _RTEMS_LIBC_TIME_LAYOUT",
                "const _RTEMS_LIBC_SOCKET_LAYOUT"
            ],
            "that cfg guards the layout refusals and nothing else: an image that \
             links has a boot shim, and an image without one cannot link at all"
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

    /// The socket predicate, pinned the same way.
    ///
    /// `sa_family_t` is one byte on this target, so a family at offset 1 is the
    /// only observable trace of the BSD length byte the kernel writes — and
    /// relaxing the offset back to 0 is exactly the mutation that restores the
    /// defect while every host build stays green.
    #[test]
    fn the_socket_predicate_pins_the_length_byte() {
        let src = item_body("pub const RTEMS_LIBC_SOCKET_LAYOUT_IS_CORRECT: bool =");
        for required in [
            "size_of::<libc::sa_family_t>() == 1",
            "size_of::<libc::sockaddr_in>() == 16",
            "offset_of!(libc::sockaddr_in, sin_family) == 1",
            "offset_of!(libc::sockaddr_in6, sin6_family) == 1",
            "offset_of!(libc::sockaddr_storage, ss_family) == 1",
        ] {
            assert!(
                src.contains(required),
                "the RTEMS socket layout predicate lost `{required}`; the target's \
                 sockaddr_in leads with sin_len, so the family sits at offset 1"
            );
        }
    }

    /// `timeval` is checked even though the closure never names it — `std`
    /// names it, for every `set_read_timeout` both blocking drivers call.
    #[test]
    fn the_time_predicate_covers_the_type_std_passes_for_us() {
        let src = item_body("pub const RTEMS_LIBC_TIME_LAYOUT_IS_CORRECT: bool =");
        for required in [
            "size_of::<libc::timeval>() == 16",
            "align_of::<libc::timeval>() == 8",
            "offset_of!(libc::timeval, tv_usec) == 8",
        ] {
            assert!(
                src.contains(required),
                "the RTEMS libc layout predicate lost `{required}`; SO_RCVTIMEO \
                 carries optlen = size_of::<libc::timeval>() on every connection"
            );
        }
    }

    /// The socket refusal must name the defect and the way out, like the other.
    #[test]
    fn the_socket_refusal_message_names_the_defect_and_the_way_out() {
        let src = item_body("const _RTEMS_LIBC_SOCKET_LAYOUT: () = assert!(");
        for required in [
            // the consequence, in the words that make it searchable
            "InvalidInput",
            "local_addr",
            "bound address",
            // where the evidence is, and the workaround's easy-to-miss half
            "newlib/arm",
            "aarch64",
            "build-std",
        ] {
            assert!(
                src.contains(required),
                "the RTEMS socket layout refusal message must still name `{required}`"
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
