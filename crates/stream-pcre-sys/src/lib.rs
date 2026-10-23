//! The PCRE1 calls StreamDevice's RegexpConverter.cc makes, against PCRE 8.45
//! vendored in `pcre/` and built with the options build.rs names.

#![no_std]

use core::ffi::{c_char, c_int, c_uchar, c_void};

/// Opaque compiled pattern (`pcre` in pcre.h).
#[repr(C)]
pub struct pcre {
    _private: [u8; 0],
}

/// Opaque study data (`pcre_extra` in pcre.h); RegexpConverter passes NULL.
#[repr(C)]
pub struct pcre_extra {
    _private: [u8; 0],
}

pub const PCRE_INFO_CAPTURECOUNT: c_int = 2;
pub const PCRE_ERROR_NOMATCH: c_int = -1;

unsafe extern "C" {
    pub fn pcre_compile(
        pattern: *const c_char,
        options: c_int,
        errptr: *mut *const c_char,
        erroffset: *mut c_int,
        tableptr: *const c_uchar,
    ) -> *mut pcre;

    pub fn pcre_fullinfo(
        code: *const pcre,
        extra: *const pcre_extra,
        what: c_int,
        where_: *mut c_void,
    ) -> c_int;

    pub fn pcre_exec(
        code: *const pcre,
        extra: *const pcre_extra,
        subject: *const c_char,
        length: c_int,
        startoffset: c_int,
        options: c_int,
        ovector: *mut c_int,
        ovecsize: c_int,
    ) -> c_int;

    pub fn pcre_version() -> *const c_char;
}
