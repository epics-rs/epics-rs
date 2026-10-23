//! `pcre_exec` on the Rust compile returns the rc and leaves the ovector,
//! every slot, that C's compile and exec do: the patterns and subjects of
//! the pcre_c corpus, and the exec errors, limits and verbs.

use std::ffi::c_char;

use super::c::{CPattern, Rng, all_cases, c_compile, on_big_stack};
use stream_rs::pcre::compile::pcre_compile2;
use stream_rs::pcre::exec::pcre_exec;
use stream_rs::pcre::internal::*;

/// The ovector counts that cover none, a partial pair, exact triples, a
/// remainder and the temporary-vector path for back references.
const COUNTS: &[i32] = &[0, 1, 2, 3, 4, 6, 9, 30, 31, 300];
const SENTINEL: i32 = 0x5a5a_5a5a;

fn c_exec(c: &CPattern, subject: &[u8], start: i32, options: i32, count: i32) -> (i32, Vec<i32>) {
    let mut ov = vec![SENTINEL; count as usize];
    // A real allocation even for an empty subject: an empty Vec's dangling
    // pointer lets a lookbehind's `eptr - n` wrap past C's start check.
    let mut buf = subject.to_vec();
    buf.push(0);
    let rc = unsafe {
        stream_pcre_sys::pcre_exec(
            c.0,
            std::ptr::null(),
            buf.as_ptr() as *const c_char,
            subject.len() as i32,
            start,
            options,
            if count == 0 {
                std::ptr::null_mut()
            } else {
                ov.as_mut_ptr()
            },
            count,
        )
    };
    (rc, ov)
}

fn rs_exec(re: &Pcre, subject: &[u8], start: i32, options: i32, count: i32) -> (i32, Vec<i32>) {
    let mut ov = vec![SENTINEL; count as usize];
    let rc = pcre_exec(
        Some(re),
        Some(subject),
        subject.len() as i32,
        start,
        options,
        if count == 0 { None } else { Some(&mut ov) },
        count,
    );
    (rc, ov)
}

/// Every ovector count, against C. Returns C's rc with the widest vector.
fn same_as_c(c: &CPattern, re: &Pcre, subject: &[u8], start: i32, options: i32, what: &str) -> i32 {
    let mut last = 0;
    for &count in COUNTS {
        let want = c_exec(c, subject, start, options, count);
        let got = rs_exec(re, subject, start, options, count);
        assert_eq!(
            got,
            want,
            "{what}: subject {:?} start {start} options {options:#x} count {count}",
            String::from_utf8_lossy(subject)
        );
        last = want.0;
    }
    last
}

fn check(pattern: &[u8], compile: u32, subject: &[u8], start: i32, options: u32) -> i32 {
    let name = String::from_utf8_lossy(pattern);
    let c = c_compile(pattern, compile, None).unwrap_or_else(|e| panic!("C refuses {name}: {e:?}"));
    let re = pcre_compile2(pattern, compile, None).unwrap();
    same_as_c(&c, &re, subject, start, options as i32, &name)
}

#[test]
fn argument_errors_are_cs() {
    on_big_stack(|| {
        let c = c_compile(b"a(b)", 0, None).unwrap();
        let re = pcre_compile2(b"a(b)", 0, None).unwrap();
        let mut ov = [0i32; 6];
        let cases: &[(i32, i32, i32, i32)] = &[
            // length, start, options, count
            (3, 0, 0x7fff_ffff, 6),
            (3, 0, 0, -1),
            (-1, 0, 0, 6),
            (3, -1, 0, 6),
            (3, 4, 0, 6),
            (3, 3, 0, 6),
            (
                3,
                0,
                (PCRE_NEWLINE_CR | PCRE_NEWLINE_LF | PCRE_NEWLINE_ANY) as i32,
                6,
            ),
            (3, 0, (PCRE_BSR_ANYCRLF | PCRE_BSR_UNICODE) as i32, 6),
            (3, 0, PCRE_DOTALL as i32, 6),
        ];
        for &(length, start, options, count) in cases {
            let want = unsafe {
                stream_pcre_sys::pcre_exec(
                    c.0,
                    std::ptr::null(),
                    b"xab".as_ptr() as *const c_char,
                    length,
                    start,
                    options,
                    ov.as_mut_ptr(),
                    count,
                )
            };
            let got = pcre_exec(
                Some(&re),
                Some(b"xab"),
                length,
                start,
                options,
                Some(&mut ov),
                count,
            );
            assert_eq!(
                got, want,
                "length {length} start {start} options {options:#x} count {count}"
            );
        }
        assert_eq!(
            pcre_exec(None, Some(b"x"), 1, 0, 0, None, 0),
            PCRE_ERROR_NULL
        );
        assert_eq!(
            pcre_exec(Some(&re), None, 1, 0, 0, None, 0),
            PCRE_ERROR_NULL
        );
        assert_eq!(
            pcre_exec(Some(&re), Some(b"x"), 1, 0, 0, None, 3),
            PCRE_ERROR_NULL
        );
        let mut bad = re.clone();
        bad.magic_number = REVERSED_MAGIC_NUMBER;
        assert_eq!(
            pcre_exec(Some(&bad), Some(b"x"), 1, 0, 0, None, 0),
            PCRE_ERROR_BADENDIANNESS
        );
        bad.magic_number = 0;
        assert_eq!(
            pcre_exec(Some(&bad), Some(b"x"), 1, 0, 0, None, 0),
            PCRE_ERROR_BADMAGIC
        );
    });
}

#[test]
fn limits_count_as_cs() {
    on_big_stack(|| {
        let subject = b"aaaaaaaaaaaaaaaaaaaaaaaaab";
        for n in [1, 2, 10, 100, 1000, 5000, 20000] {
            let p = format!("(*LIMIT_MATCH={n})(a+)+$");
            assert!(check(p.as_bytes(), 0, b"aaaaaaaaaaaaaaaaaaaaaaaab", 0, 0) < 0);
            let p = format!("(*LIMIT_RECURSION={n})(a|b)*c");
            check(p.as_bytes(), 0, subject, 0, 0);
            let p = format!("(*LIMIT_RECURSION={n})(?:a(?R)?b)");
            check(p.as_bytes(), 0, b"aaaaaaaaabbbbbbbbb", 0, 0);
        }
        // The compile-time MATCH_LIMIT, 10000000, reached by counting.
        assert_eq!(
            check(b"(a+)+$", 0, b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab", 0, 0),
            PCRE_ERROR_MATCHLIMIT
        );
        assert_eq!(
            check(b"((?2))((?1))", 0, b"abc", 0, 0),
            PCRE_ERROR_RECURSELOOP
        );
        assert_eq!(check(b"^(a|b(?1))$", 0, b"bbbx", 0, 0), PCRE_ERROR_NOMATCH);
    });
}

#[test]
fn hand_cases_match_c() {
    on_big_stack(|| {
        let cases: &[(&[u8], u32, &[u8], u32)] = &[
            (b"a(?:b(*SKIP)c|bd)", 0, b"abd abc", 0),
            (b"a(*MARK:m)b(*SKIP:m)c|.", 0, b"abd", 0),
            (b"(*MARK:A)x(*SKIP:B)y|.", 0, b"xz", 0),
            (b"a+(*PRUNE)b|.", 0, b"aaac", 0),
            (b"a(*THEN)b|ac", 0, b"ac", 0),
            (b"(a(*THEN)b|a)c", 0, b"ac", 0),
            (b"a(*COMMIT)b|ac", 0, b"xac ac", 0),
            (b"(?=a(*COMMIT)b)ac|.", 0, b"ac", 0),
            (b"(?!a(*COMMIT)b)ac", 0, b"ac", 0),
            (b"(*FAIL)|b", 0, b"b", 0),
            (b"a(*ACCEPT)b", 0, b"ac", 0),
            (b"(a(*ACCEPT)b)c", 0, b"ac", 0),
            (b"(?<=ab)c", 0, b"abc ac", 0),
            (b"(?<!ab)c", 0, b"abc ac", 0),
            (b"foo\\Kbar", 0, b"foobar", 0),
            (b"(?(?=a)ab|cd)", 0, b"cd ab", 0),
            (b"(a)?(?(1)b|c)", 0, b"c ab", 0),
            (b"(?<n>a)?(?(<n>)b|c)", 0, b"xc", 0),
            (b"(?(R)a|b(?R))", 0, b"ba", 0),
            (b"(?<p>\\((?:[^()]|(?&p))*\\))", 0, b"x(a(b)c)y", 0),
            (b"^(a|b\\1)+$", 0, b"aba", 0),
            (b"(a)\\1{2,3}", PCRE_CASELESS, b"aAaa", 0),
            (b"(a)|\\1", 0, b"x", 0),
            (b"(a)|\\1", PCRE_JAVASCRIPT_COMPAT, b"x", 0),
            (b"(?>a+)b", 0, b"aaab", 0),
            (b"(a+)++b", 0, b"aaab", 0),
            (b"(?:a|b)*+c", 0, b"ababc", 0),
            (b"(ab)*+", 0, b"ababx", 0),
            (b"(a|)*+b", 0, b"aab", 0),
            (b"[a-c]{2,5}?d", 0, b"abcabd", 0),
            (b"[^x]*+y", 0, b"aay", 0),
            (b"\\d{2,}?\\D", 0, b"1234a", 0),
            (b"\\R+", 0, b"\r\n\x0b\x85x", 0),
            (b"\\R", PCRE_BSR_ANYCRLF, b"\x0b\r", 0),
            (b"\\h+\\v", 0, b" \t\xa0\n", 0),
            (b"^b", PCRE_MULTILINE, b"a\nb", PCRE_NOTBOL),
            (b"a$", PCRE_MULTILINE, b"a\nb", PCRE_NOTEOL),
            (b"a$", 0, b"a\n", 0),
            (b"a$", PCRE_DOLLAR_ENDONLY, b"a\n", 0),
            (b"a\\Z", 0, b"a\r\n", PCRE_NEWLINE_CRLF),
            (b"a\\z", 0, b"a\n", 0),
            (b"^", PCRE_MULTILINE, b"a\r\nb", PCRE_NEWLINE_ANY),
            (b".+", 0, b"ab\r\ncd", PCRE_NEWLINE_ANYCRLF),
            (b"a*", 0, b"bc", PCRE_NOTEMPTY),
            (b"a*", 0, b"bca", PCRE_NOTEMPTY_ATSTART),
            (b"b", 0, b"abc", PCRE_ANCHORED),
            (b"abc", 0, b"ab", PCRE_PARTIAL_SOFT),
            (b"abc", 0, b"ab", PCRE_PARTIAL_HARD),
            (b"abc|ab", 0, b"ab", PCRE_PARTIAL_SOFT),
            (b"abc|ab", 0, b"ab", PCRE_PARTIAL_HARD),
            (b"a\\b", 0, b"a", PCRE_PARTIAL_HARD),
            (b"(a)\\1", 0, b"aa", PCRE_PARTIAL_HARD),
            (b"a+", 0, b"aaa", PCRE_PARTIAL_HARD),
            (b"a.", 0, b"a\r", PCRE_PARTIAL_HARD | PCRE_NEWLINE_CRLF),
            (b"x", 0, b"aaax", PCRE_NO_START_OPTIMIZE),
            (b"(*CR)a.b", 0, b"a\rb a\nb", 0),
            (b"^x", PCRE_FIRSTLINE | PCRE_MULTILINE, b"a\nx", 0),
            (b"\\r?\\n", 0, b"a\r\nb", PCRE_NEWLINE_CRLF),
            (b"(?m)^b", 0, b"a\r\nb", PCRE_NEWLINE_ANY),
            (b"(a)(b)(c)(d)(e)(f)(g)(h)(i)(j)(k)", 0, b"abcdefghijk", 0),
            (b"(a)(b)(c)(d)(e)\\5", 0, b"abcdee", 0),
            (b"(a)|(b)|(c)", 0, b"c", 0),
            (b"(?|(a)|(b)(c))", 0, b"bc", 0),
        ];
        for &(pattern, compile, subject, options) in cases {
            for start in 0..=subject.len() as i32 {
                check(pattern, compile, subject, start, options);
            }
        }
    });
}

fn newline_option(name: &[u8]) -> Option<u32> {
    if name == b"JS" {
        return Some(PCRE_JAVASCRIPT_COMPAT);
    }
    Some(match name.to_ascii_lowercase().as_slice() {
        b"cr" => PCRE_NEWLINE_CR,
        b"lf" => PCRE_NEWLINE_LF,
        b"crlf" => PCRE_NEWLINE_CRLF,
        b"any" => PCRE_NEWLINE_ANY,
        b"anycrlf" => PCRE_NEWLINE_ANYCRLF,
        b"bsr_anycrlf" => PCRE_BSR_ANYCRLF,
        b"bsr_unicode" => PCRE_BSR_UNICODE,
        _ => return None,
    })
}

/// A pcretest data line: the subject, the start offset and the exec
/// options, or None for an escape this rig does not run (callouts,
/// ovector sizes, DFA, copy/get).
fn subject_line(line: &[u8]) -> Option<(Vec<u8>, i32, u32)> {
    let hex = |c: u8| (c as char).to_digit(16);
    let mut out = Vec::new();
    let mut start = 0;
    let mut options = 0;
    let mut i = 0;
    while i < line.len() {
        let c = line[i];
        i += 1;
        if c != b'\\' {
            out.push(c);
            continue;
        }
        let Some(&e) = line.get(i) else { break };
        i += 1;
        match e {
            b'a' => out.push(7),
            b'b' => out.push(8),
            b'e' => out.push(27),
            b'f' => out.push(12),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(11),
            b'0'..=b'7' => {
                let mut v = (e - b'0') as u32;
                for _ in 0..2 {
                    match line.get(i) {
                        Some(&d @ b'0'..=b'7') => {
                            v = v * 8 + (d - b'0') as u32;
                            i += 1;
                        }
                        _ => break,
                    }
                }
                out.push(v as u8);
            }
            b'x' => {
                if line.get(i) == Some(&b'{') {
                    let end = i + line[i..].iter().position(|&c| c == b'}')?;
                    let v = u32::from_str_radix(std::str::from_utf8(&line[i + 1..end]).ok()?, 16)
                        .ok()?;
                    out.push(u8::try_from(v).ok()?);
                    i = end + 1;
                } else {
                    let mut v = 0;
                    for _ in 0..2 {
                        match line.get(i).and_then(|&d| hex(d)) {
                            Some(d) => {
                                v = v * 16 + d;
                                i += 1;
                            }
                            None => break,
                        }
                    }
                    out.push(v as u8);
                }
            }
            b'A' => options |= PCRE_ANCHORED,
            b'B' => options |= PCRE_NOTBOL,
            b'Z' => options |= PCRE_NOTEOL,
            b'Y' => options |= PCRE_NO_START_OPTIMIZE,
            b'N' => {
                if options & PCRE_NOTEMPTY != 0 {
                    options = options & !PCRE_NOTEMPTY | PCRE_NOTEMPTY_ATSTART;
                } else {
                    options |= PCRE_NOTEMPTY;
                }
            }
            b'P' => {
                if options & PCRE_PARTIAL_SOFT != 0 {
                    options = options & !PCRE_PARTIAL_SOFT | PCRE_PARTIAL_HARD;
                } else {
                    options |= PCRE_PARTIAL_SOFT;
                }
            }
            b'>' => {
                let n = line[i..].iter().take_while(|c| c.is_ascii_digit()).count();
                start = std::str::from_utf8(&line[i..i + n]).ok()?.parse().ok()?;
                i += n;
            }
            b'<' => {
                let end = i + line[i..].iter().position(|&c| c == b'>')?;
                options |= newline_option(&line[i..end])?;
                i = end + 1;
            }
            // Ovector size (every size runs anyway), copy and get by number
            // or name, and the match-limit search print only.
            b'O' | b'C' | b'G' if line.get(i).is_some_and(|c| c.is_ascii_alphanumeric()) => {
                i += line[i..]
                    .iter()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .count();
            }
            b'L' | b'M' => {}
            e if e.is_ascii_alphanumeric() => return None,
            e => out.push(e),
        }
    }
    Some((out, start, options))
}

/// The subjects of the corpus patterns of one file that C compiles.
fn run_testinput(name: &str) -> (usize, usize) {
    let (mut patterns, mut subjects) = (0, 0);
    for case in all_cases().iter().filter(|c| c.file == name) {
        let Ok(c) = c_compile(&case.pat, case.options, case.tables) else {
            continue;
        };
        let re = pcre_compile2(&case.pat, case.options, case.tables).unwrap();
        patterns += 1;
        let what = format!("{name}:{}", case.line);
        for d in &case.data {
            let Some((subject, start, options)) = subject_line(d) else {
                continue;
            };
            if start as usize > subject.len() {
                continue;
            }
            same_as_c(&c, &re, &subject, start, options as i32, &what);
            subjects += 1;
        }
    }
    (patterns, subjects)
}

#[test]
fn testinput1_matches_c() {
    on_big_stack(|| {
        let (p, s) = run_testinput("testinput1");
        assert!(p >= 1220 && s >= 2475, "ran {p} patterns, {s} subjects");
    });
}

#[test]
fn testinput2_matches_c() {
    on_big_stack(|| {
        let (p, s) = run_testinput("testinput2");
        assert!(p >= 1106 && s >= 1445, "ran {p} patterns, {s} subjects");
    });
}

#[test]
fn other_testinputs_match_c() {
    on_big_stack(|| {
        let (mut p, mut s) = (0, 0);
        for name in [
            "testinput3",
            "testinput8",
            "testinput11",
            "testinput13",
            "testinput14",
            "testinput17",
            "testinput20",
            "testinput23",
            "testinput25",
        ] {
            let (np, ns) = run_testinput(name);
            p += np;
            s += ns;
        }
        assert!(p >= 1025 && s >= 2344, "ran {p} patterns, {s} subjects");
    });
}

/// The exec options a subject line can carry.
const EXEC_OPTIONS: &[u32] = &[
    PCRE_ANCHORED,
    PCRE_NOTBOL,
    PCRE_NOTEOL,
    PCRE_NOTEMPTY,
    PCRE_NOTEMPTY_ATSTART,
    PCRE_PARTIAL_SOFT,
    PCRE_PARTIAL_HARD,
    PCRE_NO_START_OPTIMIZE,
    PCRE_NEWLINE_CR,
    PCRE_NEWLINE_LF,
    PCRE_NEWLINE_ANYCRLF,
    PCRE_NEWLINE_ANY,
    PCRE_BSR_ANYCRLF,
    PCRE_BSR_UNICODE,
];

/// Corpus patterns on random short subjects drawn from the pattern's own
/// bytes, line ends and Latin-1, with random exec options and start.
/// Subjects stay at 12 bytes, and a pattern that reaches a limit on a
/// one-newline subject is left to limits_count_as_cs, so a run stays short.
/// PCRE_FUZZ_ITERS raises the count and PCRE_FUZZ_SEED replays a run.
#[test]
fn fuzzed_subjects_match_c() {
    let iters: usize = std::env::var("PCRE_FUZZ_ITERS").map_or(20_000, |v| v.parse().unwrap());
    let seed: u64 =
        std::env::var("PCRE_FUZZ_SEED").map_or(0x2545_f491_4f6c_dd1d, |v| v.parse().unwrap());
    on_big_stack(move || {
        let compiled: Vec<_> = all_cases()
            .into_iter()
            .filter_map(|case| {
                let c = c_compile(&case.pat, case.options, case.tables).ok()?;
                let rc = c_exec(&c, b"\n", 0, 0, 0).0;
                if rc == PCRE_ERROR_MATCHLIMIT || rc == PCRE_ERROR_RECURSIONLIMIT {
                    return None;
                }
                let re = pcre_compile2(&case.pat, case.options, case.tables).unwrap();
                Some((case, c, re))
            })
            .collect();
        let mut r = Rng(seed);
        for _ in 0..iters {
            let (case, c, re) = &compiled[r.below(compiled.len())];
            let mut subject = Vec::new();
            for _ in 0..r.below(13) {
                subject.push(match r.below(4) {
                    0 | 1 if !case.pat.is_empty() => case.pat[r.below(case.pat.len())],
                    2 => b"\r\n\x0b\x0c\x85 \t"[r.below(7)],
                    _ => r.below(256) as u8,
                });
            }
            let mut options = 0;
            for _ in 0..r.below(3) {
                options |= EXEC_OPTIONS[r.below(EXEC_OPTIONS.len())];
            }
            let start = r.below(subject.len() + 1) as i32;
            let what = format!("seed {seed} {}:{}", case.file, case.line);
            same_as_c(c, re, &subject, start, options as i32, &what);
        }
    });
}
