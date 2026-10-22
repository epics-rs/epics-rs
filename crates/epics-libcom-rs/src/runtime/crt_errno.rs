//! C `errno` and `strerror(errno)` after a failed file operation.
//!
//! On unix std's `raw_os_error` is that `errno` and its message is
//! `strerror`'s. On Windows it is the Win32 code, which the CRT maps to
//! `errno` before C sees it. The map and the texts are Wine's copy of the
//! MSVC CRT, wine-mirror 840a939ee526 `dlls/msvcrt/errno.c`
//! (`msvcrt_set_errno` :131-189, `MSVCRT__sys_errlist` :78-124, `strerror`
//! :272-282), with the numbers of its `include/msvcrt/errno.h`. A socket
//! error is not one of these: C prints its `FormatMessage` text.

use std::io;

/// The `errno` C reads after the file operation that failed with `e`.
pub fn errno(e: &io::Error) -> Option<i32> {
    e.raw_os_error().map(os_to_errno)
}

/// `strerror(errno)` for that failure; an error with no OS code is its own
/// message.
pub fn strerror(e: &io::Error) -> String {
    match e.raw_os_error() {
        #[cfg(windows)]
        Some(code) => sys_errlist(os_to_errno(code)).to_string(),
        // std's message is `strerror`'s with ` (os error N)` appended.
        #[cfg(not(windows))]
        Some(code) => {
            let text = e.to_string();
            text.strip_suffix(&format!(" (os error {code})"))
                .unwrap_or(&text)
                .to_string()
        }
        None => e.to_string(),
    }
}

#[cfg(not(windows))]
fn os_to_errno(code: i32) -> i32 {
    code
}

/// `msvcrt_set_errno`: every code it does not name is `EINVAL`.
#[cfg(windows)]
fn os_to_errno(code: i32) -> i32 {
    use windows_sys::Win32::Foundation as w;
    const ENOENT: i32 = 2;
    const EIO: i32 = 5;
    const E2BIG: i32 = 7;
    const ENOEXEC: i32 = 8;
    const EBADF: i32 = 9;
    const ECHILD: i32 = 10;
    const EAGAIN: i32 = 11;
    const ENOMEM: i32 = 12;
    const EACCES: i32 = 13;
    const EBUSY: i32 = 16;
    const EEXIST: i32 = 17;
    const ENODEV: i32 = 19;
    const EINVAL: i32 = 22;
    const EMFILE: i32 = 24;
    const ENOSPC: i32 = 28;
    const EPIPE: i32 = 32;
    const EDEADLK: i32 = 36;
    const ENOTEMPTY: i32 = 41;
    match code as u32 {
        w::ERROR_ACCESS_DENIED
        | w::ERROR_NETWORK_ACCESS_DENIED
        | w::ERROR_CANNOT_MAKE
        | w::ERROR_SEEK_ON_DEVICE
        | w::ERROR_LOCK_FAILED
        | w::ERROR_FAIL_I24
        | w::ERROR_CURRENT_DIRECTORY
        | w::ERROR_DRIVE_LOCKED
        | w::ERROR_NOT_LOCKED
        | w::ERROR_INVALID_ACCESS
        | w::ERROR_SHARING_VIOLATION
        | w::ERROR_LOCK_VIOLATION => EACCES,
        w::ERROR_FILE_NOT_FOUND
        | w::ERROR_NO_MORE_FILES
        | w::ERROR_BAD_PATHNAME
        | w::ERROR_BAD_NETPATH
        | w::ERROR_INVALID_DRIVE
        | w::ERROR_BAD_NET_NAME
        | w::ERROR_FILENAME_EXCED_RANGE
        | w::ERROR_PATH_NOT_FOUND => ENOENT,
        w::ERROR_IO_DEVICE => EIO,
        w::ERROR_BAD_FORMAT => ENOEXEC,
        w::ERROR_INVALID_HANDLE => EBADF,
        w::ERROR_OUTOFMEMORY
        | w::ERROR_INVALID_BLOCK
        | w::ERROR_NOT_ENOUGH_QUOTA
        | w::ERROR_ARENA_TRASHED => ENOMEM,
        w::ERROR_BUSY => EBUSY,
        w::ERROR_ALREADY_EXISTS | w::ERROR_FILE_EXISTS => EEXIST,
        w::ERROR_BAD_DEVICE => ENODEV,
        w::ERROR_TOO_MANY_OPEN_FILES => EMFILE,
        w::ERROR_DISK_FULL => ENOSPC,
        w::ERROR_BROKEN_PIPE => EPIPE,
        w::ERROR_POSSIBLE_DEADLOCK => EDEADLK,
        w::ERROR_DIR_NOT_EMPTY => ENOTEMPTY,
        w::ERROR_BAD_ENVIRONMENT => E2BIG,
        w::ERROR_WAIT_NO_CHILDREN | w::ERROR_CHILD_NOT_COMPLETE => ECHILD,
        w::ERROR_NO_PROC_SLOTS | w::ERROR_MAX_THRDS_REACHED | w::ERROR_NESTING_NOT_ALLOWED => {
            EAGAIN
        }
        _ => EINVAL,
    }
}

/// `MSVCRT__sys_errlist`, indexed by `errno`; Wine took the texts from
/// glibc (:36).
#[cfg(any(windows, test))]
const SYS_ERRLIST: [&str; 44] = [
    "Success",
    "Operation not permitted",
    "No such file or directory",
    "No such process",
    "Interrupted system call",
    "Input/output error",
    "No such device or address",
    "Argument list too long",
    "Exec format error",
    "Bad file descriptor",
    "No child processes",
    "Resource temporarily unavailable",
    "Cannot allocate memory",
    "Permission denied",
    "Bad address",
    "Unknown error",
    "Device or resource busy",
    "File exists",
    "Invalid cross-device link",
    "No such device",
    "Not a directory",
    "Is a directory",
    "Invalid argument",
    "Too many open files in system",
    "Too many open files",
    "Inappropriate ioctl for device",
    "Unknown error",
    "File too large",
    "No space left on device",
    "Illegal seek",
    "Read-only file system",
    "Too many links",
    "Broken pipe",
    "Numerical argument out of domain",
    "Numerical result out of range",
    "Unknown error",
    "Resource deadlock avoided",
    "Unknown error",
    "File name too long",
    "No locks available",
    "Function not implemented",
    "Directory not empty",
    "Invalid or incomplete multibyte or wide character",
    "Unknown error",
];

/// Wine `strerror`: an `errno` outside `0..=_sys_nerr` reads as the last
/// entry.
#[cfg(any(windows, test))]
fn sys_errlist(errno: i32) -> &'static str {
    let nerr = SYS_ERRLIST.len() - 1;
    SYS_ERRLIST[usize::try_from(errno).map_or(nerr, |e| e.min(nerr))]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_errno_past_sys_nerr_reads_unknown_error() {
        assert_eq!(sys_errlist(13), "Permission denied");
        assert_eq!(
            sys_errlist(42),
            "Invalid or incomplete multibyte or wide character"
        );
        for errno in [-1, 43, 44, 80, 100] {
            assert_eq!(sys_errlist(errno), "Unknown error", "{errno}");
        }
    }

    #[test]
    fn an_error_with_no_os_code_is_its_own_message() {
        let e = io::Error::other("stream did not contain valid UTF-8");
        assert_eq!(
            (errno(&e), strerror(&e).as_str()),
            (None, "stream did not contain valid UTF-8")
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_errno_is_the_os_code_and_strerror_its_text() {
        let e = io::Error::from_raw_os_error(libc::EISDIR);
        assert_eq!(errno(&e), Some(libc::EISDIR));
        assert_eq!(
            strerror(&e),
            e.to_string()
                .trim_end_matches(&format!(" (os error {})", libc::EISDIR))
        );
        #[cfg(target_env = "gnu")]
        assert_eq!(strerror(&e), "Is a directory");
    }

    #[cfg(windows)]
    #[test]
    fn a_win32_code_reads_as_the_crts_errno() {
        use windows_sys::Win32::Foundation as w;
        for (code, want, text) in [
            (w::ERROR_ACCESS_DENIED, 13, "Permission denied"),
            (w::ERROR_FILE_NOT_FOUND, 2, "No such file or directory"),
            (w::ERROR_PATH_NOT_FOUND, 2, "No such file or directory"),
            (w::ERROR_DIR_NOT_EMPTY, 41, "Directory not empty"),
            (w::ERROR_INVALID_FUNCTION, 22, "Invalid argument"),
        ] {
            let e = io::Error::from_raw_os_error(code as i32);
            assert_eq!(
                (errno(&e), strerror(&e).as_str()),
                (Some(want), text),
                "{code}"
            );
        }
    }
}
