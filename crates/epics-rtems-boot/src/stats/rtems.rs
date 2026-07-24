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

use core::ffi::c_char;
use std::ffi::CString;

use super::{FdUsage, MemUsage};

pub(super) fn fd_usage() -> Option<FdUsage> {
    let mut used = 0u32;
    let mut max = 0u32;
    // SAFETY: both pointers are to live locals of the right type, which is
    // the whole contract — the C's only failure mode is a null argument.
    let rc = unsafe { ffi::epics_rtems_boot_fd_usage(&mut used, &mut max) };
    (rc == 0).then_some(FdUsage { used, max })
}

/// All three fields come from one `_Protected_heap_Get_information` call, so on
/// this backend they are present or absent together — the per-field `Option`
/// exists for backends whose sources differ, not because RTEMS' can be partial.
pub(super) fn mem_usage() -> MemUsage {
    let mut free = 0u64;
    let mut used = 0u64;
    let mut largest_free = 0u64;
    // SAFETY: as above — three pointers to live locals of the right type.
    let rc = unsafe { ffi::epics_rtems_boot_mem_usage(&mut free, &mut used, &mut largest_free) };
    if rc != 0 {
        return MemUsage::default();
    }
    MemUsage {
        free: Some(free),
        used: Some(used),
        largest_free: Some(largest_free),
    }
}

pub(super) fn dump_tasks(tag: &str) {
    // SAFETY: takes a NUL-terminated tag and only reads it; the C side does
    // its own bounds-checked iteration and copies only ids inside the visitor.
    unsafe { with_tag(tag, ffi::epics_rtems_boot_dump_tasks) }
}

pub(super) fn stack_report(tag: &str) {
    // SAFETY: as above — a read-only NUL-terminated tag, and the report itself
    // is the shell command's own implementation.
    unsafe { with_tag(tag, ffi::epics_rtems_boot_stack_report) }
}

pub(super) fn fd_census(tag: &str) {
    // SAFETY: as above. The census is read-only on every descriptor it
    // touches, so it can run while the pumps own their sockets.
    unsafe { with_tag(tag, ffi::epics_rtems_boot_fd_census) }
}

/// Nothing to record: `rtems_task_iterate` walks the kernel's own thread table,
/// so the census sees every thread whether or not it announced itself — including
/// the ones that never call `enter_ioc_thread`, which the VxWorks backend cannot
/// see at all.
pub(super) fn register_task() {}

/// Hand a Rust tag to a C printer as a NUL-terminated string.
///
/// `unwrap_or_default` rather than `expect`: an interior NUL in a caller's tag
/// is a formatting slip, and a probe that aborts the IOC to complain about its
/// own label is worse than one that prints an empty tag.
///
/// # Safety
///
/// `f` must only read the pointer it is given, for the duration of the call.
unsafe fn with_tag(tag: &str, f: unsafe extern "C" fn(*const c_char)) {
    let c = CString::new(tag).unwrap_or_default();
    unsafe { f(c.as_ptr()) }
}

mod ffi {
    use core::ffi::{c_char, c_int};

    unsafe extern "C" {
        pub fn epics_rtems_boot_fd_usage(used: *mut u32, max: *mut u32) -> c_int;
        pub fn epics_rtems_boot_mem_usage(
            free_total: *mut u64,
            used_total: *mut u64,
            free_largest: *mut u64,
        ) -> c_int;
        pub fn epics_rtems_boot_dump_tasks(tag: *const c_char);
        pub fn epics_rtems_boot_stack_report(tag: *const c_char);
        pub fn epics_rtems_boot_fd_census(tag: *const c_char);
    }
}
