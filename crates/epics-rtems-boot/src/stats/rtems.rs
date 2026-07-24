//! The RTEMS backend: `csrc/rtems_stats.c`, through its two entry points.
//!
//! Selected by [`super`] on exactly `all(target_os = "rtems",
//! rtems_boot_linked)` — the one configuration where this package's build
//! script compiled the C. The gate is on the `mod` declaration rather than on
//! each item here, so there is no way to add a declaration to this file that
//! outruns the C: an `extern` on a wider cfg would leave an undefined symbol in
//! the toolchain-free portability build that `scripts/rtems-check.sh` exists to
//! keep compiling.
//!
//! Descriptor and heap usage are the two values on this target that Rust cannot
//! reach: `std` exposes neither, and both live behind RTEMS internals
//! (`rtems_libio_iops`, `RTEMS_Malloc_Heap`) rather than behind POSIX. That is
//! why this backend is C at all, and why it is the only one that needs a
//! build-script cfg to say whether its symbols exist.

use super::{FdUsage, MemUsage};

pub(super) fn fd_usage() -> Option<FdUsage> {
    let mut used = 0u32;
    let mut max = 0u32;
    // SAFETY: both pointers are to live locals of the right type, which is
    // the whole contract — the C's only failure mode is a null argument.
    let rc = unsafe { ffi::epics_rtems_boot_fd_usage(&mut used, &mut max) };
    (rc == 0).then_some(FdUsage { used, max })
}

pub(super) fn mem_usage() -> Option<MemUsage> {
    let mut free = 0u64;
    let mut used = 0u64;
    let mut largest_free = 0u64;
    // SAFETY: as above — three pointers to live locals of the right type.
    let rc = unsafe { ffi::epics_rtems_boot_mem_usage(&mut free, &mut used, &mut largest_free) };
    (rc == 0).then_some(MemUsage {
        free,
        used,
        largest_free,
    })
}

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
