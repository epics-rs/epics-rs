//! The tables and configuration in stream_rs::pcre equal the vendored C's.

use std::ffi::{c_int, c_long, c_void};

use stream_rs::pcre::chartables::DEFAULT_TABLES;
use stream_rs::pcre::config::{ConfigValue, config};
use stream_rs::pcre::internal::*;

use stream_pcre_sys as _;

unsafe extern "C" {
    static _pcre_OP_lengths: [u8; OP_TABLE_LENGTH];
    static _pcre_default_tables: [u8; TABLES_LENGTH];
    static _pcre_hspace_list: [u32; 20];
    static _pcre_vspace_list: [u32; 8];
    fn pcre_config(what: c_int, where_: *mut c_void) -> c_int;
}

#[test]
fn op_lengths_are_cs() {
    assert_eq!(OP_LENGTHS, unsafe { _pcre_OP_lengths });
}

#[test]
fn default_tables_are_cs() {
    assert_eq!(DEFAULT_TABLES, unsafe { _pcre_default_tables });
}

#[test]
fn space_lists_are_cs() {
    let h = unsafe { _pcre_hspace_list };
    let v = unsafe { _pcre_vspace_list };
    assert_eq!(&h[..19], &HSPACE_LIST[..]);
    assert_eq!(h[19], NOTACHAR);
    assert_eq!(&v[..7], &VSPACE_LIST[..]);
    assert_eq!(v[7], NOTACHAR);
}

#[test]
fn config_answers_as_c() {
    for what in -1..=14 {
        let (rs_rc, rs_v) = match config(what) {
            Ok(v) => (0, v),
            Err((rc, v)) => (rc, v),
        };
        let mut buf = [0u8; 16];
        let c_rc = unsafe { pcre_config(what, buf.as_mut_ptr().cast()) };
        assert_eq!(rs_rc, c_rc, "rc for {what}");
        match rs_v {
            ConfigValue::Int(i)
                if rs_rc == 0 || what == PCRE_CONFIG_UTF16 || what == PCRE_CONFIG_UTF32 =>
            {
                assert_eq!(
                    i,
                    c_int::from_ne_bytes(buf[..4].try_into().unwrap()),
                    "{what}"
                )
            }
            ConfigValue::Long(l) => {
                let c = c_long::from_ne_bytes(buf[..size_of::<c_long>()].try_into().unwrap());
                assert_eq!(l as c_long, c, "{what}")
            }
            ConfigValue::Str(s) => {
                assert!(s.is_none());
                let p = usize::from_ne_bytes(buf[..size_of::<usize>()].try_into().unwrap());
                assert_eq!(p, 0)
            }
            ConfigValue::Int(_) => {}
        }
    }
}

#[test]
fn real_pcre_size_is_cs() {
    // pcre_compile's block for /a/ is the header plus 9 code bytes
    // (BRA link, CHAR a, KET link, END) and no names.
    let mut err = std::ptr::null();
    let mut off = 0;
    let re = unsafe {
        stream_pcre_sys::pcre_compile(c"a".as_ptr(), 0, &mut err, &mut off, std::ptr::null())
    };
    assert!(!re.is_null());
    let mut size: usize = 0;
    let rc = unsafe {
        stream_pcre_sys::pcre_fullinfo(re, std::ptr::null(), PCRE_INFO_SIZE, (&raw mut size).cast())
    };
    assert_eq!(rc, 0);
    assert_eq!(size, REAL_PCRE_SIZE + 9);
}

unsafe extern "C" {
    fn _pcre_is_newline(
        p: *const u8,
        t: c_int,
        end: *const u8,
        len: *mut c_int,
        utf: c_int,
    ) -> c_int;
    fn _pcre_was_newline(
        p: *const u8,
        t: c_int,
        start: *const u8,
        len: *mut c_int,
        utf: c_int,
    ) -> c_int;
}

#[test]
fn newline_checks_are_cs() {
    use stream_rs::pcre::newline::{is_newline, was_newline};
    for t in [NLTYPE_ANY, NLTYPE_ANYCRLF] {
        for a in 0..=255u8 {
            for b in [b'\r', b'\n', b'x', a] {
                let s = [b, a, b'\n'];
                for (ptr, end) in [(1, 2), (1, 3), (0, 1)] {
                    let (mut rl, mut cl) = (-7, -7);
                    let r = is_newline(&s, ptr, t, end, &mut rl);
                    let c = unsafe {
                        _pcre_is_newline(s.as_ptr().add(ptr), t, s.as_ptr().add(end), &mut cl, 0)
                    };
                    assert_eq!((r, rl), (c != 0, cl), "is {t} {s:?} {ptr}");
                }
                for (ptr, start) in [(2, 0), (2, 1), (1, 0)] {
                    let (mut rl, mut cl) = (-7, -7);
                    let r = was_newline(&s, ptr, t, start, &mut rl);
                    let c = unsafe {
                        _pcre_was_newline(s.as_ptr().add(ptr), t, s.as_ptr().add(start), &mut cl, 0)
                    };
                    assert_eq!((r, rl), (c != 0, cl), "was {t} {s:?} {ptr}");
                }
            }
        }
    }
}
