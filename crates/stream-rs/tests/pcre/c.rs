//! The vendored C library as the oracle, and pcretest's reading of the
//! testinput files every pcre rig runs.
//!
//! The corpus is every pattern of PCRE 8.45's testinput 1, 2, 3, 8, 11, 13,
//! 14, 17, 20, 23 and 25 (pcre_testdata/), the sets that need no UTF, UCP or
//! JIT; 8 and 20 are DFA sets whose patterns compile and match as any other.
//! Left out: 4–7, 9, 10, 15, 16, 18, 19, 22, 24 and 26 (UTF/UCP), 12 (JIT),
//! 21 and 22 (reloads of saved bytecode) and EBC (EBCDIC). `/L`
//! (testinput3's fr_FR) and `/T1` get Latin-1 tables built here, since no
//! fr_FR locale is assumed.

use std::ffi::{CStr, c_char, c_int, c_void};

use stream_rs::pcre::chartables::DEFAULT_TABLES;
use stream_rs::pcre::compile::CompileError;
use stream_rs::pcre::internal::*;

mod ffi {
    use std::ffi::{c_char, c_int, c_void};

    unsafe extern "C" {
        pub fn pcre_compile2(
            pattern: *const c_char,
            options: c_int,
            errorcodeptr: *mut c_int,
            errptr: *mut *const c_char,
            erroffset: *mut c_int,
            tableptr: *const u8,
        ) -> *mut c_void;
        pub static pcre_free: unsafe extern "C" fn(*mut c_void);
    }
}

/// A pattern the C compiled, freed on drop.
pub struct CPattern(pub *mut stream_pcre_sys::pcre);

impl Drop for CPattern {
    fn drop(&mut self) {
        unsafe { ffi::pcre_free(self.0.cast()) }
    }
}

impl CPattern {
    /// The whole `REAL_PCRE` block, `PCRE_INFO_SIZE` bytes.
    pub fn block(&self) -> &[u8] {
        let mut size: usize = 0;
        let rc = unsafe {
            stream_pcre_sys::pcre_fullinfo(
                self.0,
                std::ptr::null(),
                PCRE_INFO_SIZE,
                (&raw mut size).cast(),
            )
        };
        assert_eq!(rc, 0);
        unsafe { std::slice::from_raw_parts(self.0 as *const u8, size) }
    }
}

/// `pcre_compile2`, the pattern ending at its first NUL as a C string does.
pub fn c_compile(
    pat: &[u8],
    options: u32,
    tables: Option<&'static Tables>,
) -> Result<CPattern, CompileError> {
    let nul = pat.iter().position(|&b| b == 0).unwrap_or(pat.len());
    let mut cpat = pat[..nul].to_vec();
    cpat.push(0);
    let mut code: c_int = 0;
    let mut text: *const c_char = std::ptr::null();
    let mut offset: c_int = -1;
    let tp = tables.map_or(std::ptr::null(), |t| t.as_ptr());
    let re = unsafe {
        ffi::pcre_compile2(
            cpat.as_ptr().cast(),
            options as c_int,
            &mut code,
            &mut text,
            &mut offset,
            tp,
        )
    };
    if re.is_null() {
        let text = unsafe { CStr::from_ptr(text) }.to_str().unwrap();
        return Err(CompileError {
            code,
            text: Box::leak(text.to_owned().into_boxed_str()),
            offset,
        });
    }
    Ok(CPattern(re.cast::<c_void>().cast()))
}

/// Each name-table entry's group number and name through its NUL; C leaves
/// the bytes after the NUL uninitialised.
pub fn names(table: &[u8], count: usize, es: usize) -> Vec<Vec<u8>> {
    (0..count)
        .map(|i| {
            let e = &table[i * es..(i + 1) * es];
            let nul = e[2..].iter().position(|&b| b == 0).expect("name NUL");
            e[..2 + nul + 1].to_vec()
        })
        .collect()
}

/// testinput3's fr_FR, near enough: ISO-8859-1 letters 0xC0-0xFE.
pub fn latin1_tables() -> &'static Tables {
    let mut t = DEFAULT_TABLES;
    for c in 0xc0usize..=0xff {
        if c == 0xd7 || c == 0xf7 {
            continue;
        }
        let upper = c < 0xdf;
        let lower = c >= 0xe0;
        if upper {
            t[LCC_OFFSET + c] = (c + 0x20) as u8;
            t[FCC_OFFSET + c] = (c + 0x20) as u8;
            t[FCC_OFFSET + c + 0x20] = c as u8;
        }
        let set = |t: &mut Tables, base: usize| t[CBITS_OFFSET + base + c / 8] |= 1 << (c % 8);
        if upper {
            set(&mut t, CBIT_UPPER);
        }
        if lower || c == 0xdf {
            set(&mut t, CBIT_LOWER);
        }
        set(&mut t, CBIT_WORD);
        set(&mut t, CBIT_GRAPH);
        set(&mut t, CBIT_PRINT);
        t[CTYPES_OFFSET + c] |= CTYPE_LETTER | CTYPE_WORD;
    }
    Box::leak(Box::new(t))
}

/// The default tables at another address, which C stores rather than NULL.
fn default_copy() -> &'static Tables {
    Box::leak(Box::new(DEFAULT_TABLES))
}

/// A pattern of a testinput file with its pcretest options and its data
/// lines, trimmed as pcretest trims them.
pub struct Case {
    pub file: &'static str,
    pub line: usize,
    pub pat: Vec<u8>,
    pub options: u32,
    pub tables: Option<&'static Tables>,
    pub data: Vec<Vec<u8>>,
}

fn mc_option(s: &[u8]) -> Option<(u32, usize)> {
    let lower = s.to_ascii_lowercase();
    for (name, opt) in [
        (&b"cr>"[..], PCRE_NEWLINE_CR),
        (b"lf>", PCRE_NEWLINE_LF),
        (b"crlf>", PCRE_NEWLINE_CRLF),
        (b"anycrlf>", PCRE_NEWLINE_ANYCRLF),
        (b"any>", PCRE_NEWLINE_ANY),
        (b"bsr_anycrlf>", PCRE_BSR_ANYCRLF),
        (b"bsr_unicode>", PCRE_BSR_UNICODE),
        (b"js>", PCRE_JAVASCRIPT_COMPAT),
    ] {
        if lower.starts_with(name) {
            return Some((opt, name.len()));
        }
    }
    None
}

/// pcretest's modifier switch; `None` where pcretest skips the pattern.
fn modifiers(
    m: &[u8],
    latin1: &'static Tables,
    copy: &'static Tables,
) -> Option<(u32, Option<&'static Tables>)> {
    let mut options = 0;
    let mut tables = None;
    let mut posix = false;
    let mut i = 0;
    while i < m.len() {
        let c = m[i];
        i += 1;
        match c {
            b'f' => options |= PCRE_FIRSTLINE,
            b'i' => options |= PCRE_CASELESS,
            b'm' => options |= PCRE_MULTILINE,
            b's' => options |= PCRE_DOTALL,
            b'x' => options |= PCRE_EXTENDED,
            b'A' => options |= PCRE_ANCHORED,
            b'C' => options |= PCRE_AUTO_CALLOUT,
            b'E' => options |= PCRE_DOLLAR_ENDONLY,
            b'J' => options |= PCRE_DUPNAMES,
            b'N' => options |= PCRE_NO_AUTO_CAPTURE,
            b'O' => options |= PCRE_NO_AUTO_POSSESS,
            b'U' => options |= PCRE_UNGREEDY,
            b'W' => options |= PCRE_UCP,
            b'X' => options |= PCRE_EXTRA,
            b'Y' => options |= PCRE_NO_START_OPTIMIZE,
            b'8' => options |= PCRE_UTF8,
            b'9' => options |= PCRE_NEVER_UTF,
            b'?' => options |= PCRE_NO_UTF8_CHECK,
            b'P' => posix = true,
            b'g' | b'G' | b'B' | b'D' | b'I' | b'K' | b'M' | b'Z' | b'F' | b'+' | b'=' | b' '
            | b'\r' | b'\n' => {}
            b'Q' => match m.get(i) {
                Some(b'0' | b'1') => i += 1,
                _ => return None,
            },
            b'S' => {
                while let Some(&s) = m.get(i) {
                    match s {
                        b'S' | b'!' | b'-' => i += 1,
                        b'+' => {
                            i += 1;
                            if m.get(i) == Some(&b'+') {
                                i += 1;
                            }
                            if matches!(m.get(i), Some(b'1'..=b'7')) {
                                i += 1;
                            }
                        }
                        _ => break,
                    }
                }
            }
            b'T' => {
                tables = Some(match m.get(i) {
                    Some(b'0') => copy,
                    Some(b'1') => latin1,
                    _ => return None,
                });
                i += 1;
            }
            b'L' => {
                while i < m.len() && !matches!(m[i], b'\n' | b'\r' | b' ') {
                    i += 1;
                }
                tables = Some(latin1);
            }
            b'>' => break,
            b'<' => {
                let (opt, len) = mc_option(&m[i..])?;
                options |= opt;
                i += len;
            }
            _ => return None,
        }
    }
    if posix {
        // pcreposix.c regcomp() maps its cflags onto these alone.
        let kept = PCRE_CASELESS
            | PCRE_MULTILINE
            | PCRE_DOTALL
            | PCRE_NO_AUTO_CAPTURE
            | PCRE_UTF8
            | PCRE_UCP
            | PCRE_UNGREEDY;
        return Some((options & kept, None));
    }
    Some((options, tables))
}

/// pcretest's reading of patterns: a delimiter, the pattern (lines joined
/// with their newlines) up to the unescaped delimiter, modifiers to the end
/// of the line, then data lines up to a blank one.
fn corpus(
    file: &'static str,
    text: &[u8],
    latin1: &'static Tables,
    copy: &'static Tables,
) -> Vec<Case> {
    let lines: Vec<&[u8]> = text.split_inclusive(|&b| b == b'\n').collect();
    let mut out = Vec::new();
    let mut l = 0;
    while l < lines.len() {
        let line = lines[l];
        let start = l;
        l += 1;
        let s = line.iter().position(|b| !b.is_ascii_whitespace());
        let Some(s) = s else { continue };
        let delim = line[s];
        // `<` lines are pcretest's forbid and reload directives.
        if delim == b'<' || delim.is_ascii_alphanumeric() || delim == b'\\' {
            continue;
        }
        let mut buf = line[s + 1..].to_vec();
        let mut p = 0;
        let end = loop {
            while p < buf.len() {
                if buf[p] == b'\\' && p + 1 < buf.len() {
                    p += 1;
                } else if buf[p] == delim {
                    break;
                }
                p += 1;
            }
            if p < buf.len() {
                break Some(p);
            }
            if l == lines.len() {
                break None;
            }
            buf.extend_from_slice(lines[l]);
            l += 1;
        };
        let Some(mut end) = end else { break };
        let mut pat = buf[..end].to_vec();
        if buf.get(end + 1) == Some(&b'\\') {
            pat.push(b'\\');
            end += 1;
        }
        let mods = &buf[end + 1..];
        let mut data = Vec::new();
        while l < lines.len() && lines[l].iter().any(|b| !b.is_ascii_whitespace()) {
            data.push(lines[l].trim_ascii().to_vec());
            l += 1;
        }
        if let Some((options, tables)) = modifiers(mods, latin1, copy) {
            out.push(Case {
                file,
                line: start + 1,
                pat,
                options,
                tables,
                data,
            });
        }
    }
    out
}

const FILES: [(&str, &[u8]); 11] = [
    ("testinput1", include_bytes!("../pcre_testdata/testinput1")),
    ("testinput2", include_bytes!("../pcre_testdata/testinput2")),
    ("testinput3", include_bytes!("../pcre_testdata/testinput3")),
    ("testinput8", include_bytes!("../pcre_testdata/testinput8")),
    (
        "testinput11",
        include_bytes!("../pcre_testdata/testinput11"),
    ),
    (
        "testinput13",
        include_bytes!("../pcre_testdata/testinput13"),
    ),
    (
        "testinput14",
        include_bytes!("../pcre_testdata/testinput14"),
    ),
    (
        "testinput17",
        include_bytes!("../pcre_testdata/testinput17"),
    ),
    (
        "testinput20",
        include_bytes!("../pcre_testdata/testinput20"),
    ),
    (
        "testinput23",
        include_bytes!("../pcre_testdata/testinput23"),
    ),
    (
        "testinput25",
        include_bytes!("../pcre_testdata/testinput25"),
    ),
];

pub fn all_cases() -> Vec<Case> {
    let (latin1, copy) = (latin1_tables(), default_copy());
    FILES
        .iter()
        .flat_map(|&(f, t)| corpus(f, t, latin1, copy))
        .collect()
}

/// C's match() and the compile's group nesting recurse natively; give them
/// the room pcretest's main thread has and more.
pub fn on_big_stack(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(1 << 30)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap();
}

/// xorshift64*, so a failing seed replays.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}
