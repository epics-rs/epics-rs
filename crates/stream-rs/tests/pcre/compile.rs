//! stream_rs::pcre::compile against the vendored C pcre_compile2: the same
//! header, name table and bytecode, or the same error code, text and offset,
//! over the pcre_c corpus, each pattern also under every public option, and
//! fuzzed patterns. A `/8` or `/W` pattern must fail as C does (ERR32,
//! ERR67). AUTO_CALLOUT with `[[:<:]]` or `[[:>:]]` is not compared: C's
//! callout offsets then point into its substitute string.

use std::panic::{AssertUnwindSafe, catch_unwind};

use super::c::{Rng, all_cases, c_compile, latin1_tables, names, on_big_stack};
use stream_rs::pcre::compile::{CompileError, pcre_compile2};
use stream_rs::pcre::internal::*;

/// What C's `real_pcre` holds, laid out as in pcre_internal.h.
#[derive(Debug, PartialEq, Eq)]
struct Compiled {
    magic_number: u32,
    size: u32,
    options: u32,
    flags: u32,
    limit_match: u32,
    limit_recursion: u32,
    first_char: u16,
    req_char: u16,
    max_lookbehind: u16,
    top_bracket: u16,
    top_backref: u16,
    name_table_offset: u16,
    name_entry_size: u16,
    name_count: u16,
    ref_count: u16,
    has_tables: bool,
    names: Vec<Vec<u8>>,
    /// The code through its final OP_END.
    code: Vec<u8>,
}

/// The top bracket's alternatives, its KET and the OP_END after it.
fn used_code(code: &[u8]) -> Vec<u8> {
    let mut at = 0;
    loop {
        at += get(code, at + 1);
        if code[at] != OP_ALT {
            break;
        }
    }
    assert_eq!(code[at], OP_KET);
    let end = at + 1 + LINK_SIZE;
    assert_eq!(code[end], OP_END);
    code[..=end].to_vec()
}

fn c_compiled(
    pat: &[u8],
    options: u32,
    tables: Option<&'static Tables>,
) -> Result<Compiled, CompileError> {
    let re = c_compile(pat, options, tables)?;
    let raw = re.block();
    let u32_at = |o: usize| u32::from_ne_bytes(raw[o..o + 4].try_into().unwrap());
    let u16_at = |o: usize| u16::from_ne_bytes(raw[o..o + 2].try_into().unwrap());
    let ptr_at =
        |o: usize| usize::from_ne_bytes(raw[o..o + size_of::<usize>()].try_into().unwrap());
    let (nto, es, nc) = (
        u16_at(34) as usize,
        u16_at(36) as usize,
        u16_at(38) as usize,
    );
    Ok(Compiled {
        magic_number: u32_at(0),
        size: u32_at(4),
        options: u32_at(8),
        flags: u32_at(12),
        limit_match: u32_at(16),
        limit_recursion: u32_at(20),
        first_char: u16_at(24),
        req_char: u16_at(26),
        max_lookbehind: u16_at(28),
        top_bracket: u16_at(30),
        top_backref: u16_at(32),
        name_table_offset: nto as u16,
        name_entry_size: es as u16,
        name_count: nc as u16,
        ref_count: u16_at(40),
        has_tables: ptr_at(48) != 0,
        names: names(&raw[nto..], nc, es),
        code: used_code(&raw[nto + nc * es..]),
    })
}

fn rs_compiled(
    pat: &[u8],
    options: u32,
    tables: Option<&'static Tables>,
) -> Result<Compiled, CompileError> {
    let re = pcre_compile2(pat, options, tables)?;
    let (es, nc) = (re.name_entry_size as usize, re.name_count as usize);
    Ok(Compiled {
        magic_number: re.magic_number,
        size: re.size,
        options: re.options,
        flags: re.flags,
        limit_match: re.limit_match,
        limit_recursion: re.limit_recursion,
        first_char: re.first_char,
        req_char: re.req_char,
        max_lookbehind: re.max_lookbehind,
        top_bracket: re.top_bracket,
        top_backref: re.top_backref,
        name_table_offset: re.name_table_offset,
        name_entry_size: re.name_entry_size,
        name_count: re.name_count,
        ref_count: re.ref_count,
        has_tables: re.tables.is_some(),
        names: names(re.name_table(), nc, es),
        code: used_code(re.code()),
    })
}

/// One differential case; the error names the pattern and both answers.
fn differ(pat: &[u8], options: u32, tables: Option<&'static Tables>) -> Result<(), String> {
    if options & PCRE_AUTO_CALLOUT != 0 && pat.windows(6).any(|w| w == b"[[:<:]" || w == b"[[:>:]")
    {
        return Ok(());
    }
    let c = c_compiled(pat, options, tables);
    let rs = catch_unwind(AssertUnwindSafe(|| rs_compiled(pat, options, tables)));
    match rs {
        Ok(rs) if rs == c => Ok(()),
        Ok(rs) => Err(format!(
            "{:?} options {options:#x}:\n  C  {c:?}\n  rs {rs:?}",
            String::from_utf8_lossy(pat)
        )),
        Err(_) => Err(format!(
            "{:?} options {options:#x}: rs panicked, C {c:?}",
            String::from_utf8_lossy(pat)
        )),
    }
}

#[test]
fn testinputs_compile_as_c() {
    on_big_stack(|| {
        let cases = all_cases();
        let mut failed = Vec::new();
        for c in &cases {
            if let Err(e) = differ(&c.pat, c.options, c.tables) {
                failed.push(format!("{}:{}: {e}", c.file, c.line));
            }
        }
        assert!(cases.len() > 2500, "only {} patterns read", cases.len());
        assert!(
            failed.is_empty(),
            "{} of {} differ:\n{}",
            failed.len(),
            cases.len(),
            failed.join("\n")
        );
    });
}

/// Each corpus pattern under every single public option, so options the
/// testinputs never pair with a pattern reach the compiler too.
#[test]
fn testinputs_under_each_option_compile_as_c() {
    on_big_stack(|| {
        let cases = all_cases();
        let mut failed = Vec::new();
        for c in &cases {
            for bit in 0..32 {
                let o = 1u32 << bit;
                if PUBLIC_COMPILE_OPTIONS & o == 0 {
                    continue;
                }
                if let Err(e) = differ(&c.pat, c.options | o, c.tables) {
                    failed.push(format!("{}:{} +{o:#x}: {e}", c.file, c.line));
                }
            }
        }
        assert!(
            failed.is_empty(),
            "{} differ:\n{}",
            failed.len(),
            failed.join("\n")
        );
    });
}

const TOKENS: &[&[u8]] = &[
    b"a",
    b"b",
    b"Z",
    b"0",
    b"9",
    b" ",
    b"\n",
    b"#",
    b"-",
    b"]",
    b"[",
    b"[^",
    b"(",
    b")",
    b"|",
    b"*",
    b"+",
    b"?",
    b"{",
    b"}",
    b"{2}",
    b"{1,3}",
    b"{,2}",
    b"{3,}",
    b"*+",
    b"++",
    b"?+",
    b"*?",
    b"^",
    b"$",
    b".",
    b"\\",
    b"\\d",
    b"\\D",
    b"\\w",
    b"\\W",
    b"\\s",
    b"\\S",
    b"\\h",
    b"\\H",
    b"\\v",
    b"\\V",
    b"\\R",
    b"\\X",
    b"\\b",
    b"\\B",
    b"\\A",
    b"\\z",
    b"\\Z",
    b"\\G",
    b"\\K",
    b"\\Q",
    b"\\E",
    b"\\1",
    b"\\2",
    b"\\10",
    b"\\g1",
    b"\\g{-1}",
    b"\\g<1>",
    b"\\g'n'",
    b"\\k<n>",
    b"\\k{n}",
    b"\\x41",
    b"\\x{100}",
    b"\\x{41}",
    b"\\0",
    b"\\012",
    b"\\o{101}",
    b"\\c",
    b"\\cA",
    b"\\e",
    b"\\N",
    b"\\p{L}",
    b"\\u0041",
    b"\\U",
    b"\\L",
    b"\\C",
    b"(?:",
    b"(?|",
    b"(?>",
    b"(?=",
    b"(?!",
    b"(?<=",
    b"(?<!",
    b"(?<n>",
    b"(?'n'",
    b"(?P<n>",
    b"(?P=n)",
    b"(?P>n)",
    b"(?&n)",
    b"(?R)",
    b"(?1)",
    b"(?-1)",
    b"(?+1)",
    b"(?0)",
    b"(?(1)",
    b"(?(<n>)",
    b"(?(R)",
    b"(?(R1)",
    b"(?(R&n)",
    b"(?(DEFINE)",
    b"(?(?=a)",
    b"(?(n)",
    b"(?i)",
    b"(?-i)",
    b"(?x)",
    b"(?s)",
    b"(?m)",
    b"(?J)",
    b"(?U)",
    b"(?X)",
    b"(?i:",
    b"(?#c)",
    b"(?C)",
    b"(?C9)",
    b"(*ACCEPT)",
    b"(*FAIL)",
    b"(*F)",
    b"(*COMMIT)",
    b"(*PRUNE)",
    b"(*PRUNE:m)",
    b"(*SKIP)",
    b"(*SKIP:m)",
    b"(*THEN)",
    b"(*THEN:m)",
    b"(*MARK:m)",
    b"(*:m)",
    b"(*CR)",
    b"(*LF)",
    b"(*ANY)",
    b"(*UTF8)",
    b"(*UCP)",
    b"(*NO_START_OPT)",
    b"(*NO_AUTO_POSSESS)",
    b"(*LIMIT_MATCH=10)",
    b"(*LIMIT_RECURSION=5)",
    b"(*BSR_ANYCRLF)",
    b"[:alpha:]",
    b"[:^digit:]",
    b"[:word:]",
    b"[[:<:]]",
    b"[[:>:]]",
    b"[.a.]",
    b"[=a=]",
    b"[a-z]",
    b"[\\d-z]",
    b"[]a]",
    b"[^]a]",
    b"[\\x00-\\xff]",
    b"\\xe9",
    b"\xe9",
    b"\xc0",
    b"[\\Q]\\E]",
    b"[\\b]",
    b"[\\h\\V]",
];

fn fuzz_pattern(r: &mut Rng) -> Vec<u8> {
    let mut p = Vec::new();
    for _ in 0..1 + r.below(24) {
        if r.below(12) == 0 {
            p.push(1 + r.below(255) as u8);
        } else {
            p.extend_from_slice(TOKENS[r.below(TOKENS.len())]);
        }
    }
    p
}

fn fuzz_options(r: &mut Rng) -> u32 {
    let mut o = 0;
    for _ in 0..r.below(4) {
        let bit = 1u32 << r.below(32);
        if PUBLIC_COMPILE_OPTIONS & bit != 0 {
            o |= bit;
        }
    }
    o
}

/// Random token strings and corpus mutations; PCRE_FUZZ_ITERS raises the
/// count and PCRE_FUZZ_SEED replays a run.
#[test]
fn fuzzed_patterns_compile_as_c() {
    let iters: usize = std::env::var("PCRE_FUZZ_ITERS").map_or(100_000, |v| v.parse().unwrap());
    let seed: u64 =
        std::env::var("PCRE_FUZZ_SEED").map_or(0x9e37_79b9_7f4a_7c15, |v| v.parse().unwrap());
    on_big_stack(move || {
        let cases = all_cases();
        let latin1 = latin1_tables();
        let mut r = Rng(seed);
        let mut failed = Vec::new();
        for _ in 0..iters {
            let (pat, options) = if r.below(2) == 0 {
                (fuzz_pattern(&mut r), fuzz_options(&mut r))
            } else {
                let c = &cases[r.below(cases.len())];
                let mut p = c.pat.clone();
                for _ in 0..1 + r.below(3) {
                    let at = r.below(p.len() + 1);
                    match r.below(3) {
                        0 => {
                            let t = TOKENS[r.below(TOKENS.len())];
                            p.splice(at..at, t.iter().copied());
                        }
                        1 if at < p.len() => {
                            p.remove(at);
                        }
                        _ if at < p.len() => p[at] = 1 + r.below(255) as u8,
                        _ => {}
                    }
                }
                (p, c.options | fuzz_options(&mut r))
            };
            let pat: Vec<u8> = pat.into_iter().filter(|&b| b != 0).collect();
            let tables = if r.below(8) == 0 { Some(latin1) } else { None };
            if let Err(e) = differ(&pat, options, tables) {
                failed.push(e);
                if failed.len() == 20 {
                    break;
                }
            }
        }
        assert!(
            failed.is_empty(),
            "seed {seed}: {} differ:\n{}",
            failed.len(),
            failed.join("\n")
        );
    });
}

/// The nesting limit and the deepest legal nesting, on the 2 MiB stack a
/// default test thread has.
#[test]
fn deepest_nesting_compiles_as_c_on_a_default_stack() {
    for depth in [
        PARENS_NEST_LIMIT as usize - 1,
        PARENS_NEST_LIMIT as usize,
        PARENS_NEST_LIMIT as usize + 1,
    ] {
        for (open, close) in [(&b"("[..], &b")"[..]), (b"(?:", b")"), (b"(?<=", b")")] {
            let mut p = open.repeat(depth);
            p.push(b'a');
            p.extend_from_slice(&close.repeat(depth));
            differ(&p, 0, None).unwrap();
        }
    }
}
