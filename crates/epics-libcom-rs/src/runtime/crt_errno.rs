//! C `errno` and `strerror(errno)` after a failed file operation.
//!
//! On unix std's `raw_os_error` is that `errno` and its message is
//! `strerror`'s. On Windows it is the Win32 code, which the CRT maps to
//! `errno` before C sees it, and the text is the CRT's own `strerror`. The map
//! is measured, since neither ucrtbase.dll nor msvcrt.dll exports the CRT's
//! mapper: UCRT `__acrt_errno_map_os_error` over every code 0..=65535 on
//! windows-latest, epics-rs CI run 39745491328. A socket error is not one of
//! these: C prints its `FormatMessage` text.

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
        Some(code) => {
            // SAFETY: the CRT returns a NUL-terminated string "in a
            // thread-local storage buffer owned by the runtime" (MS docs,
            // strerror), copied here before this thread can call it again.
            unsafe { std::ffi::CStr::from_ptr(libc::strerror(os_to_errno(code))) }
                .to_string_lossy()
                .into_owned()
        }
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

#[cfg(windows)]
fn os_to_errno(code: i32) -> i32 {
    match code {
        2 | 3 | 15 | 18 | 53 | 67 | 161 | 206 => libc::ENOENT,
        10 => libc::E2BIG,
        11 | 188..=202 => libc::ENOEXEC,
        6 | 114 | 130 => libc::EBADF,
        128 | 129 => libc::ECHILD,
        89 | 164 | 215 => libc::EAGAIN,
        7 | 8 | 9 | 1816 => libc::ENOMEM,
        5 | 16 | 19..=36 | 65 | 82 | 83 | 108 | 132 | 158 | 167 => libc::EACCES,
        80 | 183 => libc::EEXIST,
        17 => libc::EXDEV,
        4 => libc::EMFILE,
        112 => libc::ENOSPC,
        109 => libc::EPIPE,
        145 => libc::ENOTEMPTY,
        1113 => libc::EILSEQ,
        _ => libc::EINVAL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The edges of the measured map, with the texts the CRT printed for
    /// them in the same probe.
    #[cfg(windows)]
    #[test]
    fn a_win32_code_reads_as_the_crts_errno() {
        for (code, want, text) in [
            (2, 2, "No such file or directory"),
            (5, 13, "Permission denied"),
            (8, 12, "Not enough space"),
            (12, 22, "Invalid argument"),
            (17, 18, "Improper link"),
            (18, 2, "No such file or directory"),
            (19, 13, "Permission denied"),
            (36, 13, "Permission denied"),
            (37, 22, "Invalid argument"),
            (145, 41, "Directory not empty"),
            (187, 22, "Invalid argument"),
            (188, 8, "Exec format error"),
            (202, 8, "Exec format error"),
            (203, 22, "Invalid argument"),
            (1113, 42, "Illegal byte sequence"),
        ] {
            let e = io::Error::from_raw_os_error(code);
            assert_eq!(
                (errno(&e), strerror(&e).as_str()),
                (Some(want), text),
                "{code}"
            );
        }
    }
}
