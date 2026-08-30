//! The RTEMS backend: `csrc/rtems_shell_cmds.c`, through its four entry points.
//!
//! Selected by [`super`] on exactly `all(target_os = "rtems",
//! rtems_boot_linked)` — the one configuration where this package's build
//! script compiled the C. The gate is on the `mod` declaration rather than on
//! each item here, for the same reason `stats/rtems.rs` carries it there: an
//! `extern` on a wider cfg would leave an undefined symbol in the
//! toolchain-free portability build that `scripts/rtems-check.sh` keeps
//! compiling.
//!
//! Everything here is a conversion. The decisions — which netstat flags stand
//! in for base's legacy calls, why `setlogmask` is not called, why the shell
//! environment is initialised inside the lookup — are all in the C file, next
//! to the code they govern.

use core::ffi::{c_char, c_int};
use std::ffi::{CStr, CString};

use super::ShellError;

pub(super) fn netstat(level: i32) -> Result<(), ShellError> {
    // SAFETY: takes an int by value and reads no memory of ours. It prints,
    // which is the whole command; base's has no failure path either.
    unsafe { ffi::epics_rtems_boot_netstat(level as c_int) };
    Ok(())
}

/// `argv` is handed to the shell command as a real `char **`, so it must be
/// writable: `getopt` permutes the array it is given, and several RTEMS shell
/// commands use it. The buffers are therefore owned `Vec<u8>` rather than
/// [`CString`] pointers cast to `*mut` — casting away the shared reference and
/// letting C write through it would be undefined behaviour that no test on
/// this workspace could observe, because the C never runs here.
pub(super) fn run_shell_command(name: &str, argv: &[String]) -> Result<i32, ShellError> {
    let name = CString::new(name).map_err(|_| ShellError::NotRepresentable)?;

    let mut owned: Vec<Vec<u8>> = Vec::with_capacity(argv.len());
    for arg in argv {
        if arg.as_bytes().contains(&0) {
            return Err(ShellError::NotRepresentable);
        }
        let mut buf = Vec::with_capacity(arg.len() + 1);
        buf.extend_from_slice(arg.as_bytes());
        buf.push(0);
        owned.push(buf);
    }
    // NULL-terminated, as every `main`-shaped C entry point expects even
    // though it is also handed `argc`.
    let mut ptrs: Vec<*mut c_char> = owned
        .iter_mut()
        .map(|buf| buf.as_mut_ptr().cast::<c_char>())
        .collect();
    ptrs.push(core::ptr::null_mut());

    let mut status: c_int = 0;
    // SAFETY: `name` is NUL-terminated and read-only; `ptrs` is a
    // NUL-terminated array of `owned.len()` writable NUL-terminated buffers,
    // all live for the call; `status` points at a live local. The C writes
    // `status` only on the path where it returns 0.
    let rc = unsafe {
        ffi::epics_rtems_boot_shell_run(
            name.as_ptr(),
            owned.len() as c_int,
            ptrs.as_mut_ptr(),
            &mut status,
        )
    };
    if rc != 0 {
        return Err(ShellError::NoSuchCommand);
    }
    Ok(status as i32)
}

pub(super) fn set_log_priority(name: &str) -> Result<(), ShellError> {
    let name = CString::new(name).map_err(|_| ShellError::NotRepresentable)?;
    // SAFETY: a NUL-terminated string the C only reads.
    let rc = unsafe { ffi::epics_rtems_boot_set_log_priority(name.as_ptr()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(ShellError::UnknownLevel)
    }
}

/// Walks the C library's `prioritynames` by index until it reports the end.
///
/// By index rather than by handing Rust the table pointer: `prioritynames` is
/// a definition inside `<syslog.h>` whose element type is the C library's, so
/// the shape this crate would have to declare to walk it itself is exactly the
/// shape that differs between C libraries.
pub(super) fn log_priority_names() -> Vec<String> {
    let mut names = Vec::new();
    for index in 0u32.. {
        // SAFETY: takes an index by value; returns either NULL or a pointer to
        // a string literal in the C library, which outlives the copy below.
        let name = unsafe { ffi::epics_rtems_boot_log_priority_name(index) };
        if name.is_null() {
            break;
        }
        // SAFETY: non-NULL and NUL-terminated by construction — it is one of
        // `prioritynames`' own `c_name` literals.
        let name = unsafe { CStr::from_ptr(name) };
        if let Ok(name) = name.to_str() {
            names.push(name.to_string());
        }
    }
    names
}

mod ffi {
    use core::ffi::{c_char, c_int, c_uint};

    unsafe extern "C" {
        pub fn epics_rtems_boot_netstat(level: c_int);
        pub fn epics_rtems_boot_shell_run(
            cmd: *const c_char,
            argc: c_int,
            argv: *mut *mut c_char,
            status: *mut c_int,
        ) -> c_int;
        pub fn epics_rtems_boot_set_log_priority(name: *const c_char) -> c_int;
        pub fn epics_rtems_boot_log_priority_name(index: c_uint) -> *const c_char;
    }
}
