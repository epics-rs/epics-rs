//! `pcre_fullinfo` on the Rust compile answers as the vendored C's on its
//! own compile, for every request on patterns that set each flag it
//! reports and on every pcre_c corpus pattern.

use std::ffi::c_void;

use super::c::{CPattern, all_cases, c_compile, names, on_big_stack};
use stream_pcre_sys::pcre_fullinfo;
use stream_rs::pcre::compile::pcre_compile2;
use stream_rs::pcre::fullinfo::{InfoValue, fullinfo};
use stream_rs::pcre::internal::*;

const PATTERNS: &[(&[u8], i32)] = &[
    (b"abc", 0),
    (b"^abc", 0),
    (b"(?m)^abc", 0),
    (b"a(b)c\\1", 0),
    (b"(?i)xyz", 0),
    (b"(?<n>a)(?<m>b)|(?<n>c)", PCRE_DUPNAMES as i32),
    (b"(?<=ab)c", 0),
    (b"\\r\\n", 0),
    (b"(?J)(?<a>x)(?<a>y)", 0),
    (b"(*LIMIT_MATCH=123)(*LIMIT_RECURSION=45)a+", 0),
    (b"a*", 0),
    (b"[ab]x", PCRE_CASELESS as i32),
    (b"(?x) a b # c", PCRE_ANCHORED as i32 | PCRE_EXTENDED as i32),
];

/// Every request on one pattern, against C.
fn same_as_c(c: &CPattern, re: &Pcre, name: &str) {
    for what in -1..=26 {
        let mut buf = [0u8; 16];
        let c_rc = unsafe { pcre_fullinfo(c.0, std::ptr::null(), what, buf.as_mut_ptr().cast()) };
        match fullinfo(Some(re), what) {
            Err(rc) => assert_eq!(rc, c_rc, "{name} what {what}"),
            Ok(v) => {
                assert_eq!(c_rc, 0, "{name} what {what}");
                let ptr = usize::from_ne_bytes(buf[..8].try_into().unwrap());
                let i32_ = i32::from_ne_bytes(buf[..4].try_into().unwrap());
                match v {
                    InfoValue::ULong(l) => assert_eq!(l, ptr as u64, "{name} {what}"),
                    InfoValue::Size(s) => assert_eq!(s, ptr, "{name} {what}"),
                    InfoValue::Int(i) => assert_eq!(i, i32_, "{name} {what}"),
                    InfoValue::U32(u) => assert_eq!(u, i32_ as u32, "{name} {what}"),
                    InfoValue::FirstTable(t) => {
                        assert!(t.is_none());
                        assert_eq!(ptr, 0, "{name} {what}");
                    }
                    InfoValue::NameTable(t) => {
                        let (nc, es) = (re.name_count as usize, re.name_entry_size as usize);
                        let ct = unsafe { std::slice::from_raw_parts(ptr as *const u8, nc * es) };
                        assert_eq!(names(t, nc, es), names(ct, nc, es), "{name} {what}");
                    }
                    InfoValue::Tables(t) => {
                        let ct = unsafe { &*(ptr as *const c_void as *const Tables) };
                        assert_eq!(t, ct, "{name} {what}");
                    }
                }
            }
        }
    }
}

#[test]
fn every_request_answers_as_c() {
    for &(pattern, options) in PATTERNS {
        let c = c_compile(pattern, options as u32, None).expect("C compiles it");
        let re = pcre_compile2(pattern, options as u32, None).unwrap();
        same_as_c(&c, &re, &String::from_utf8_lossy(pattern));
    }
}

#[test]
fn every_corpus_pattern_answers_as_c() {
    on_big_stack(|| {
        let mut n = 0;
        for case in all_cases() {
            let Ok(c) = c_compile(&case.pat, case.options, case.tables) else {
                continue;
            };
            let re = pcre_compile2(&case.pat, case.options, case.tables).unwrap();
            same_as_c(&c, &re, &format!("{}:{}", case.file, case.line));
            n += 1;
        }
        assert!(n > 3000, "only {n} patterns compiled");
    });
}

#[test]
fn a_null_or_foreign_pattern_is_refused_as_c() {
    assert_eq!(fullinfo(None, PCRE_INFO_SIZE), Err(PCRE_ERROR_NULL));
    let mut re = pcre_compile2(b"a", 0, None).unwrap();
    re.magic_number = REVERSED_MAGIC_NUMBER;
    assert_eq!(
        fullinfo(Some(&re), PCRE_INFO_SIZE),
        Err(PCRE_ERROR_BADENDIANNESS)
    );
    re.magic_number = 0;
    assert_eq!(
        fullinfo(Some(&re), PCRE_INFO_SIZE),
        Err(PCRE_ERROR_BADMAGIC)
    );
    re.magic_number = MAGIC_NUMBER;
    re.flags &= !PCRE_MODE;
    assert_eq!(fullinfo(Some(&re), PCRE_INFO_SIZE), Err(PCRE_ERROR_BADMODE));
}
