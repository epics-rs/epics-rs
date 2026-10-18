//! The vendored build answers pcre_config() as build.rs intends.

use std::ffi::{CStr, c_int, c_long, c_void};

unsafe extern "C" {
    fn pcre_config(what: c_int, where_: *mut c_void) -> c_int;
}

fn int(what: c_int) -> c_int {
    let mut v: c_int = -99;
    assert_eq!(unsafe { pcre_config(what, (&raw mut v).cast()) }, 0);
    v
}

fn long(what: c_int) -> c_long {
    let mut v: c_long = -99;
    assert_eq!(unsafe { pcre_config(what, (&raw mut v).cast()) }, 0);
    v
}

#[test]
fn the_vendored_build_has_the_configure_defaults() {
    let version = unsafe { CStr::from_ptr(stream_pcre_sys::pcre_version()) };
    assert_eq!(version.to_bytes(), b"8.45 2021-06-15");
    assert_eq!(int(0), 0, "UTF8");
    assert_eq!(int(1), 10, "NEWLINE");
    assert_eq!(int(2), 2, "LINK_SIZE");
    assert_eq!(int(3), 10, "POSIX_MALLOC_THRESHOLD");
    assert_eq!(long(4), 10_000_000, "MATCH_LIMIT");
    assert_eq!(int(5), 1, "STACKRECURSE");
    assert_eq!(int(6), 0, "UNICODE_PROPERTIES");
    assert_eq!(long(7), 10_000_000, "MATCH_LIMIT_RECURSION");
    assert_eq!(int(8), 0, "BSR (0 = any Unicode newline)");
    assert_eq!(int(9), 0, "JIT");
    assert_eq!(long(13), 250, "PARENS_LIMIT");
}
