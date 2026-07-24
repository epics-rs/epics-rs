//! Descriptor and heap usage, read out of RTEMS itself.
//!
//! These are the two IOC-statistics values on this target that Rust cannot
//! reach: `std` exposes neither, and both live behind RTEMS internals
//! (`rtems_libio_iops`, `RTEMS_Malloc_Heap`) rather than behind POSIX. The C
//! that reads them is `csrc/rtems_stats.c`, ported from devIocStats'
//! `os/RTEMS/osdFdUsage.c` and `osdMemUsage.c`; this module is the safe wrapper
//! and the *only* thing above it that needs to know a C boundary exists.
//!
//! # `None` means unavailable, not zero
//!
//! Both readers return [`Option`], and both return [`None`] on every build that
//! is not a linked RTEMS image. That distinction is load-bearing rather than
//! decorative: **zero free descriptors and zero free heap are both real
//! readings** on this target, and they are precisely the readings an operator
//! most needs to believe. devIocStats cannot express the difference — its
//! record graph substitutes a sentinel (`FD_FREE`'s `CALC` is `B>0?B-A:C` with
//! `C = 1000`, `iocStats/iocAdmin/Db/ioc.template:118-123`), so an IOC whose
//! descriptor support is missing publishes "1000 free" rather than "unknown".
//! An `Option` says the thing the sentinel cannot.
//!
//! # Cost
//!
//! [`fd_usage`] walks the descriptor table, so it is O(`CONFIGURE_MAXIMUM_
//! FILE_DESCRIPTORS`) — 150 entries on this BSP. [`mem_usage`] walks the heap
//! under the allocator's lock; upstream's own header comment says gathering
//! heap statistics "could be expensive" and warns against running it too
//! often. Neither is on any serving path; both are called once per tick by the
//! status pusher, whose interval is one second.

/// Open file descriptors and the ceiling they are counted against.
///
/// `max` is `rtems_libio_number_iops`, i.e. `CONFIGURE_MAXIMUM_FILE_DESCRIPTORS`
/// from `csrc/rtems_config.c`. Measured on the bring-up box, this is the limit
/// the IOC hits *first*: at 142 concurrent CA connections the 143rd was refused
/// by the libbsd socket zone, which is sized from this cap — ahead of the heap
/// ceiling, which had room for roughly ten more connections at that moment.
///
/// The cap of 150 is base's own score-arm value, run on a target where base
/// itself compiles the POSIX arm and runs 64 — a deviation in which arm's
/// number we take, not an invented number. `doc/rtems-fd-ceiling-deviation.md`
/// carries the measurements, including why `free()` at idle is numerically the
/// connection ceiling and why `CA_REFUSED_CNT` never sees this wall.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FdUsage {
    /// Descriptors currently open, across the whole image — sockets, the
    /// console and anything the filesystem holds, not only CA/PVA connections.
    pub used: u32,
    /// The configured size of the descriptor table.
    pub max: u32,
}

impl FdUsage {
    /// Descriptors still available.
    ///
    /// Saturating, so a table that reports more open than configured (which
    /// would be a kernel accounting bug, not a state this can be in) reads as
    /// zero free rather than wrapping to four billion.
    pub fn free(&self) -> u32 {
        self.max.saturating_sub(self.used)
    }
}

/// Malloc-heap usage.
///
/// The RTEMS *workspace* is a separate allocator and is not counted here;
/// devIocStats keeps it in its own OSD file for the same reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemUsage {
    /// Bytes free in the malloc heap, summed over every free block.
    pub free: u64,
    /// Bytes currently allocated from the malloc heap.
    pub used: u64,
    /// The largest single free block.
    ///
    /// Reported separately from [`free`](Self::free) because it is the
    /// fragmentation signal: an allocation fails on this number, not on the
    /// total. A heap with megabytes free in kilobyte fragments cannot start a
    /// connection whose thread wants a 512 KiB stack, and only this field says
    /// so.
    pub largest_free: u64,
}

impl MemUsage {
    /// Free plus allocated.
    ///
    /// Derived here rather than in the C, because it is arithmetic on two
    /// measurements rather than a third measurement. devIocStats computes the
    /// same sum (`osdMemUsage.c:73`) and its own header comment flags that this
    /// is *not* the true total the allocator was given — the difference is
    /// allocator overhead.
    pub fn total(&self) -> u64 {
        self.free.saturating_add(self.used)
    }
}

/// Read descriptor usage, or [`None`] on a build with no RTEMS boot shim.
pub fn fd_usage() -> Option<FdUsage> {
    #[cfg(all(target_os = "rtems", rtems_boot_linked))]
    {
        let mut used = 0u32;
        let mut max = 0u32;
        // SAFETY: both pointers are to live locals of the right type, which is
        // the whole contract — the C's only failure mode is a null argument.
        let rc = unsafe { ffi::epics_rtems_boot_fd_usage(&mut used, &mut max) };
        return (rc == 0).then_some(FdUsage { used, max });
    }
    #[cfg(not(all(target_os = "rtems", rtems_boot_linked)))]
    None
}

/// Read heap usage, or [`None`] on a build with no RTEMS boot shim.
pub fn mem_usage() -> Option<MemUsage> {
    #[cfg(all(target_os = "rtems", rtems_boot_linked))]
    {
        let mut free = 0u64;
        let mut used = 0u64;
        let mut largest_free = 0u64;
        // SAFETY: as above — three pointers to live locals of the right type.
        let rc =
            unsafe { ffi::epics_rtems_boot_mem_usage(&mut free, &mut used, &mut largest_free) };
        return (rc == 0).then_some(MemUsage {
            free,
            used,
            largest_free,
        });
    }
    #[cfg(not(all(target_os = "rtems", rtems_boot_linked)))]
    None
}

/// The declarations, on the one configuration where `csrc/rtems_stats.c` was
/// compiled. Naming them anywhere else would leave undefined symbols in an
/// image that is supposed to type-check without a toolchain.
#[cfg(all(target_os = "rtems", rtems_boot_linked))]
mod ffi {
    use core::ffi::c_int;

    unsafe extern "C" {
        pub fn epics_rtems_boot_fd_usage(used: *mut u32, max: *mut u32) -> c_int;
        pub fn epics_rtems_boot_mem_usage(
            free_total: *mut u64,
            used_total: *mut u64,
            free_largest: *mut u64,
        ) -> c_int;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The host has no boot shim, so both readers must say so rather than
    /// inventing a reading. A host build that reported `0` free descriptors
    /// would look like an exhausted IOC.
    #[test]
    fn a_host_build_reports_no_reading_rather_than_a_made_up_one() {
        assert_eq!(fd_usage(), None);
        assert_eq!(mem_usage(), None);
    }

    /// `free` is the number an operator watches approach zero, so it must not
    /// wrap into a huge "plenty available" on an impossible input.
    #[test]
    fn descriptor_headroom_saturates_instead_of_wrapping() {
        assert_eq!(
            FdUsage {
                used: 142,
                max: 150
            }
            .free(),
            8
        );
        assert_eq!(
            FdUsage {
                used: 150,
                max: 150
            }
            .free(),
            0
        );
        assert_eq!(
            FdUsage {
                used: 151,
                max: 150
            }
            .free(),
            0,
            "more open than configured is a kernel accounting bug; reading it \
             as 4 billion free is worse than reading it as none"
        );
    }

    /// The heap total is the sum of the two measurements, and nothing else —
    /// this is the property that keeps it from drifting from its parts.
    #[test]
    fn the_heap_total_is_exactly_free_plus_used() {
        let m = MemUsage {
            free: 3_000_000,
            used: 5_000_000,
            largest_free: 1_500_000,
        };
        assert_eq!(m.total(), 8_000_000);
    }

    /// The largest free block is a distinct measurement, not a restatement of
    /// the free total: the gap between them is the fragmentation an allocation
    /// actually fails on.
    #[test]
    fn the_largest_free_block_is_independent_of_the_free_total() {
        let fragmented = MemUsage {
            free: 4_000_000,
            used: 1_000_000,
            largest_free: 40_000,
        };
        assert!(
            fragmented.largest_free < fragmented.free,
            "a heap with 4 MB free in 40 KB fragments cannot start a connection \
             whose thread wants a 512 KiB stack, and only this field says so"
        );
    }

    /// The C is compiled on exactly one configuration, and the `extern` block
    /// must not outrun it — a declaration on a wider cfg leaves an undefined
    /// symbol in the toolchain-free portability build.
    #[test]
    fn the_extern_block_is_scoped_to_the_configuration_that_compiles_the_c() {
        let src = include_str!("stats.rs");
        let cfg = concat!("#[cfg(all(target_os = \"rtems\", ", "rtems_boot_linked))]");
        let before_extern = src
            .split_once("mod ffi {")
            .expect("the ffi module is still here")
            .0;
        assert!(
            before_extern.trim_end().ends_with(cfg),
            "the ffi declarations must sit directly under the linked-image cfg"
        );
    }

    /// The shim must stay in the build script's file list and its change list;
    /// dropping either leaves the `extern` block above pointing at nothing, or
    /// leaves a stale object behind after an edit.
    #[test]
    fn the_build_script_compiles_and_watches_the_shim() {
        let build = include_str!("../build.rs");
        assert!(build.contains(".file(\"csrc/rtems_stats.c\")"));
        assert!(build.contains("cargo::rerun-if-changed=csrc/rtems_stats.c"));
    }

    /// The two deviations from devIocStats are forced by RTEMS 6 and were
    /// measured against this BSP's installed headers. If someone "restores"
    /// upstream's spelling the file stops compiling, so the note is what keeps
    /// the next reader from trying.
    ///
    /// Searched from the first `#include` onward, never the whole file: the
    /// file's header comment *names* both upstream spellings in order to
    /// explain why they are gone, so a whole-file search would find the prose
    /// and pass while the code said the opposite. Measured — the negative
    /// assertion below failed exactly that way on its first run.
    #[test]
    fn the_shim_uses_the_rtems_6_atomic_flag_accessor() {
        let code = include_str!("../csrc/rtems_stats.c")
            .split_once("#include <stddef.h>")
            .expect("the shim still starts its code with the includes")
            .1;
        assert!(
            code.contains("rtems_libio_iop_flags(&rtems_libio_iops[i])"),
            "`iop->flags` is Atomic_Uint in RTEMS 6, so devIocStats' direct \
             `.flags &` does not compile here"
        );
        assert!(
            !code.contains("rtems_region_get_information"),
            "the pre-4.8 region fallback is dead code on RTEMS 6"
        );
    }
}
