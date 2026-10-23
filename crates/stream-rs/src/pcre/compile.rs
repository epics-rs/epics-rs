//! `pcre_compile.c`: pattern to bytecode, in the 8-bit, no-UTF, no-UCP
//! build. Every `#ifdef SUPPORT_UTF` / `SUPPORT_UCP` branch is the
//! compiled-out one, so `utf` is always false and XCLASS is never emitted.
//!
//! Pointers become indices: `ptr` indexes `Cd::pat`, `code` indexes
//! `Cd::code` (C's `cd->start_code` is index 0) and `hwm` indexes
//! `Cd::work`. The C gotos inside `compile_branch` are a label enum.

use super::chartables::DEFAULT_TABLES;
use super::internal::*;
use std::cell::Cell;

// ---- Code parameters and static tables ----

const COMPILE_WORK_SIZE: usize = 2048 * LINK_SIZE;
const COMPILE_WORK_SIZE_MAX: usize = 100 * COMPILE_WORK_SIZE;
const WORK_SIZE_SAFETY_MARGIN: usize = 100;
const OFLOW_MAX: i32 = i32::MAX - 20;

const REQ_CASELESS: i32 = 1 << 0;
const REQ_VARY: i32 = 1 << 1;
const REQ_UNSET: i32 = -2;
const REQ_NONE: i32 = -1;

/// Room past C's buffers so a write C would have made just past them (and
/// then rejected) stays in bounds here.
const CODE_SLACK: usize = 4096;
/// Zero bytes after the pattern and its two substitutes, for C's
/// look-ahead past the terminating NUL.
const PAT_PAD: usize = 64;

/// `escapes[]` for '0'..='z'; positive is a data value, negative an escape.
static ESCAPES: [i16; 75] = {
    let mut t = [0i16; 75];
    macro_rules! set {
        ($c:expr, $v:expr) => {
            t[($c - b'0') as usize] = $v as i16;
        };
    }
    set!(b':', b':');
    set!(b';', b';');
    set!(b'<', b'<');
    set!(b'=', b'=');
    set!(b'>', b'>');
    set!(b'?', b'?');
    set!(b'@', b'@');
    set!(b'A', -ESC_A);
    set!(b'B', -ESC_B);
    set!(b'C', -ESC_C);
    set!(b'D', -ESC_D);
    set!(b'E', -ESC_E);
    set!(b'G', -ESC_G);
    set!(b'H', -ESC_H);
    set!(b'K', -ESC_K);
    set!(b'N', -ESC_N);
    set!(b'P', -ESC_P);
    set!(b'Q', -ESC_Q);
    set!(b'R', -ESC_R);
    set!(b'S', -ESC_S);
    set!(b'V', -ESC_V);
    set!(b'W', -ESC_W);
    set!(b'X', -ESC_X);
    set!(b'Z', -ESC_Z);
    set!(b'[', b'[');
    set!(b'\\', b'\\');
    set!(b']', b']');
    set!(b'^', b'^');
    set!(b'_', b'_');
    set!(b'`', b'`');
    set!(b'a', ESC_LC_A);
    set!(b'b', -ESC_LC_B);
    set!(b'd', -ESC_LC_D);
    set!(b'e', ESC_LC_E);
    set!(b'f', ESC_LC_F);
    set!(b'h', -ESC_LC_H);
    set!(b'k', -ESC_LC_K);
    set!(b'n', ESC_LC_N);
    set!(b'p', -ESC_LC_P);
    set!(b'r', ESC_LC_R);
    set!(b's', -ESC_LC_S);
    set!(b't', ESC_TEE);
    set!(b'v', -ESC_LC_V);
    set!(b'w', -ESC_LC_W);
    set!(b'z', -ESC_LC_Z);
    t
};

/// `verbs[]`: name, op without argument (-1: argument mandatory), op with
/// argument (-1: not allowed). The empty name is MARK.
static VERBS: [(&[u8], i32, i32); 9] = [
    (b"", -1, OP_MARK as i32),
    (b"MARK", -1, OP_MARK as i32),
    (b"ACCEPT", OP_ACCEPT as i32, -1),
    (b"COMMIT", OP_COMMIT as i32, -1),
    (b"F", OP_FAIL as i32, -1),
    (b"FAIL", OP_FAIL as i32, -1),
    (b"PRUNE", OP_PRUNE as i32, OP_PRUNE_ARG as i32),
    (b"SKIP", OP_SKIP as i32, OP_SKIP_ARG as i32),
    (b"THEN", OP_THEN as i32, OP_THEN_ARG as i32),
];

const SUB_START_OF_WORD: &[u8] = b"\\b(?=\\w)";
const SUB_END_OF_WORD: &[u8] = b"\\b(?<=\\w)";

static POSIX_NAMES: [&[u8]; 14] = [
    b"alpha", b"lower", b"upper", b"alnum", b"ascii", b"blank", b"cntrl", b"digit", b"graph",
    b"print", b"punct", b"space", b"word", b"xdigit",
];

/// `posix_class_maps[]`: base map, second map or -1, add/remove + tweak.
static POSIX_CLASS_MAPS: [i32; 42] = [
    CBIT_WORD as i32,
    CBIT_DIGIT as i32,
    -2, // alpha
    CBIT_LOWER as i32,
    -1,
    0, // lower
    CBIT_UPPER as i32,
    -1,
    0, // upper
    CBIT_WORD as i32,
    -1,
    2, // alnum
    CBIT_PRINT as i32,
    CBIT_CNTRL as i32,
    0, // ascii
    CBIT_SPACE as i32,
    -1,
    1, // blank
    CBIT_CNTRL as i32,
    -1,
    0, // cntrl
    CBIT_DIGIT as i32,
    -1,
    0, // digit
    CBIT_GRAPH as i32,
    -1,
    0, // graph
    CBIT_PRINT as i32,
    -1,
    0, // print
    CBIT_PUNCT as i32,
    -1,
    0, // punct
    CBIT_SPACE as i32,
    -1,
    0, // space
    CBIT_WORD as i32,
    -1,
    0, // word
    CBIT_XDIGIT as i32,
    -1,
    0, // xdigit
];

/// `error_texts`, ERR0..ERR87.
static ERROR_TEXTS: [&str; ERRCOUNT as usize] = [
    "no error",
    "\\ at end of pattern",
    "\\c at end of pattern",
    "unrecognized character follows \\",
    "numbers out of order in {} quantifier",
    "number too big in {} quantifier",
    "missing terminating ] for character class",
    "invalid escape sequence in character class",
    "range out of order in character class",
    "nothing to repeat",
    "internal error: invalid forward reference offset",
    "internal error: unexpected repeat",
    "unrecognized character after (? or (?-",
    "POSIX named classes are supported only within a class",
    "missing )",
    "reference to non-existent subpattern",
    "erroffset passed as NULL",
    "unknown option bit(s) set",
    "missing ) after comment",
    "parentheses nested too deeply",
    "regular expression is too large",
    "failed to get memory",
    "unmatched parentheses",
    "internal error: code overflow",
    "unrecognized character after (?<",
    "lookbehind assertion is not fixed length",
    "malformed number or name after (?(",
    "conditional group contains more than two branches",
    "assertion expected after (?( or (?(?C)",
    "(?R or (?[+-]digits must be followed by )",
    "unknown POSIX class name",
    "POSIX collating elements are not supported",
    "this version of PCRE is compiled without UTF support",
    "spare error",
    "character value in \\x{} or \\o{} is too large",
    "invalid condition (?(0)",
    "\\C not allowed in lookbehind assertion",
    "PCRE does not support \\L, \\l, \\N{name}, \\U, or \\u",
    "number after (?C is > 255",
    "closing ) for (?C expected",
    "recursive call could loop indefinitely",
    "unrecognized character after (?P",
    "syntax error in subpattern name (missing terminator)",
    "two named subpatterns have the same name",
    "invalid UTF-8 string",
    "support for \\P, \\p, and \\X has not been compiled",
    "malformed \\P or \\p sequence",
    "unknown property name after \\P or \\p",
    "subpattern name is too long (maximum 32 characters)",
    "too many named subpatterns (maximum 10000)",
    "repeated subpattern is too long",
    "octal value is greater than \\377 in 8-bit non-UTF-8 mode",
    "internal error: overran compiling workspace",
    "internal error: previously-checked referenced subpattern not found",
    "DEFINE group contains more than one branch",
    "repeating a DEFINE group is not allowed",
    "inconsistent NEWLINE options",
    "\\g is not followed by a braced, angle-bracketed, or quoted name/number or by a plain number",
    "a numbered reference must not be zero",
    "an argument is not allowed for (*ACCEPT), (*FAIL), or (*COMMIT)",
    "(*VERB) not recognized or malformed",
    "number is too big",
    "subpattern name expected",
    "digit expected after (?+",
    "] is an invalid data character in JavaScript compatibility mode",
    "different names for subpatterns of the same number are not allowed",
    "(*MARK) must have an argument",
    "this version of PCRE is not compiled with Unicode property support",
    "\\c must be followed by an ASCII character",
    "\\k is not followed by a braced, angle-bracketed, or quoted name",
    "internal error: unknown opcode in find_fixedlength()",
    "\\N is not supported in a class",
    "too many forward references",
    "disallowed Unicode code point (>= 0xd800 && <= 0xdfff)",
    "invalid UTF-16 string",
    "name is too long in (*MARK), (*PRUNE), (*SKIP), or (*THEN)",
    "character value in \\u.... sequence is too large",
    "invalid UTF-32 string",
    "setting UTF is disabled by the application",
    "non-hex character in \\x{} (closing brace missing?)",
    "non-octal character in \\o{} (closing brace missing?)",
    "missing opening brace after \\o",
    "parentheses are too deeply nested",
    "invalid range in character class",
    "group name must start with a non-digit",
    "parentheses are too deeply nested (stack check)",
    "digits missing in \\x{} or \\o{}",
    "regular expression is too complicated",
];

/// `digitab[]`: 0x04 decimal digit, 0x08 hex digit, for the compiler's own
/// use, independent of the locale tables.
static DIGITAB: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 10 {
        t[b'0' as usize + i] = 0x0c;
        i += 1;
    }
    let mut i = 0;
    while i < 6 {
        t[b'a' as usize + i] = 0x08;
        t[b'A' as usize + i] = 0x08;
        i += 1;
    }
    t
};

const APTROWS: usize = (LAST_AUTOTAB_LEFT_OP - FIRST_AUTOTAB_OP + 1) as usize;
const APTCOLS: usize = (LAST_AUTOTAB_RIGHT_OP - FIRST_AUTOTAB_OP + 1) as usize;

#[rustfmt::skip]
static AUTOPOSSTAB: [[u8; APTCOLS]; APTROWS] = [
/* \D \d \S \s \W \w  . .+ \C \P \p \R \H \h \V \v \X \Z \z  $ $M */
  [ 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0 ],  /* \D */
  [ 1, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 1, 0, 1, 0, 1, 0, 1, 1, 1, 1 ],  /* \d */
  [ 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 0, 1, 0, 1, 1, 1, 1 ],  /* \S */
  [ 0, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0 ],  /* \s */
  [ 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0 ],  /* \W */
  [ 0, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 1, 0, 1, 0, 1, 0, 1, 1, 1, 1 ],  /* \w */
  [ 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 1, 0, 0 ],  /* .  */
  [ 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0 ],  /* .+ */
  [ 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0 ],  /* \C */
  [ 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 ],  /* \P */
  [ 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 ],  /* \p */
  [ 0, 1, 0, 1, 0, 1, 1, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0 ],  /* \R */
  [ 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0 ],  /* \H */
  [ 0, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 1, 1, 0, 0, 1, 0, 0, 1, 0, 0 ],  /* \h */
  [ 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 1, 0, 0 ],  /* \V */
  [ 0, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 1, 0, 0, 0, 1, 0, 0 ],  /* \v */
  [ 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0 ],  /* \X */
];

/// `opcode_possessify[]`, truncated at OP_CALLOUT as in C.
static OPCODE_POSSESSIFY: [u8; OP_CALLOUT as usize] = {
    let mut t = [0u8; OP_CALLOUT as usize];
    t[OP_STAR as usize] = OP_POSSTAR;
    t[OP_PLUS as usize] = OP_POSPLUS;
    t[OP_QUERY as usize] = OP_POSQUERY;
    t[OP_UPTO as usize] = OP_POSUPTO;
    t[OP_STARI as usize] = OP_POSSTARI;
    t[OP_PLUSI as usize] = OP_POSPLUSI;
    t[OP_QUERYI as usize] = OP_POSQUERYI;
    t[OP_UPTOI as usize] = OP_POSUPTOI;
    t[OP_NOTSTAR as usize] = OP_NOTPOSSTAR;
    t[OP_NOTPLUS as usize] = OP_NOTPOSPLUS;
    t[OP_NOTQUERY as usize] = OP_NOTPOSQUERY;
    t[OP_NOTUPTO as usize] = OP_NOTPOSUPTO;
    t[OP_NOTSTARI as usize] = OP_NOTPOSSTARI;
    t[OP_NOTPLUSI as usize] = OP_NOTPOSPLUSI;
    t[OP_NOTQUERYI as usize] = OP_NOTPOSQUERYI;
    t[OP_NOTUPTOI as usize] = OP_NOTPOSUPTOI;
    t[OP_TYPESTAR as usize] = OP_TYPEPOSSTAR;
    t[OP_TYPEPLUS as usize] = OP_TYPEPOSPLUS;
    t[OP_TYPEQUERY as usize] = OP_TYPEPOSQUERY;
    t[OP_TYPEUPTO as usize] = OP_TYPEPOSUPTO;
    t[OP_CRSTAR as usize] = OP_CRPOSSTAR;
    t[OP_CRPLUS as usize] = OP_CRPOSPLUS;
    t[OP_CRQUERY as usize] = OP_CRPOSQUERY;
    t[OP_CRRANGE as usize] = OP_CRPOSRANGE;
    t
};

/// HSPACE_LIST / VSPACE_LIST with C's NOTACHAR terminator.
static HSPACE_LIST_T: [u32; 20] = {
    let mut t = [NOTACHAR; 20];
    let mut i = 0;
    while i < 19 {
        t[i] = HSPACE_LIST[i];
        i += 1;
    }
    t
};
static VSPACE_LIST_T: [u32; 8] = {
    let mut t = [NOTACHAR; 8];
    let mut i = 0;
    while i < 7 {
        t[i] = VSPACE_LIST[i];
        i += 1;
    }
    t
};

fn is_hspace(c: u32) -> bool {
    HSPACE_LIST.contains(&c)
}

fn is_vspace(c: u32) -> bool {
    VSPACE_LIST.contains(&c)
}

#[inline]
fn is_digit(c: u8) -> bool {
    c.is_ascii_digit()
}

#[inline]
fn setbit(a: &mut [u8], b: u32) {
    a[(b / 8) as usize] |= 1 << (b & 7);
}

/// `find_error_text`.
pub fn find_error_text(n: i32) -> &'static str {
    ERROR_TEXTS
        .get(n as usize)
        .copied()
        .unwrap_or("Error text not found (please report)")
}

// ---- Compile-time state ----

/// `open_capitem`: number and the "has (*ACCEPT) inside" flag.
struct OpenCap {
    number: u32,
    flag: bool,
}

/// `named_group`: name as an offset into the pattern.
#[derive(Clone, Copy)]
struct NamedGroup {
    name: usize,
    length: i32,
    number: u32,
}

/// `branch_chain`; `current_branch` changes while inner calls hold the link.
struct BranchChain<'a> {
    outer: Option<&'a BranchChain<'a>>,
    current_branch: Cell<usize>,
}

/// `recurse_check`.
struct RecurseCheck<'a> {
    prev: Option<&'a RecurseCheck<'a>>,
    group: usize,
}

fn recursed(mut r: Option<&RecurseCheck<'_>>, group: usize) -> bool {
    while let Some(rc) = r {
        if rc.group == group {
            return true;
        }
        r = rc.prev;
    }
    false
}

/// `compile_data`.
struct Cd {
    tables: &'static Tables,
    /// The pattern up to its first NUL, a NUL, the two `[[:<:]]` substitutes
    /// each NUL-terminated, then zero padding.
    pat: Vec<u8>,
    end_pattern: usize,
    sub_start: usize,
    sub_end: usize,
    /// Pass 1: C's `cworkspace`, which is both `start_workspace` and
    /// `start_code`. Pass 2: the code block from `codestart`.
    code: Vec<u8>,
    /// Pass 2 forward-reference list; empty in pass 1, where `hwm` stays 0.
    work: Vec<u8>,
    /// C sets `cd->start_workspace = NULL` once the pattern is complete.
    workspace_live: bool,
    hwm: usize,
    workspace_size: usize,
    open_caps: Vec<OpenCap>,
    named_groups: Vec<NamedGroup>,
    name_table: Vec<u8>,
    names_found: i32,
    name_entry_size: i32,
    bracount: u32,
    final_bracount: i32,
    max_lookbehind: i32,
    top_backref: i32,
    backref_map: u32,
    namedrefcount: u32,
    parens_depth: i32,
    assert_depth: i32,
    external_options: u32,
    external_flags: u32,
    req_varyopt: i32,
    had_accept: bool,
    had_pruneorskip: bool,
    check_lookbehind: bool,
    dupnames: bool,
    dupgroups: bool,
    iscondassert: bool,
    nltype: i32,
    nllen: i32,
    nl: [u8; 4],
    /// The one `errorcode` every C `errorcodeptr` points at.
    errorcode: i32,
}

impl Cd {
    fn fcc(&self, c: u32) -> u8 {
        self.tables[FCC_OFFSET + c as usize]
    }
    fn cbits(&self, i: usize) -> u8 {
        self.tables[CBITS_OFFSET + i]
    }
    fn ctypes(&self, c: u32) -> u8 {
        self.tables[CTYPES_OFFSET + c as usize]
    }
    /// `ptr - cd->start_pattern`.
    fn poff(&self, ptr: usize) -> i32 {
        ptr as i32
    }
    fn starts(&self, ptr: usize, s: &[u8]) -> bool {
        self.pat[ptr..].starts_with(s)
    }
}

/// `expand_workspace`.
fn expand_workspace(cd: &mut Cd) -> i32 {
    let newsize = (cd.workspace_size * 2).min(COMPILE_WORK_SIZE_MAX);
    if cd.workspace_size >= COMPILE_WORK_SIZE_MAX
        || newsize - cd.workspace_size < WORK_SIZE_SAFETY_MARGIN
    {
        return ERR72;
    }
    cd.work.resize(newsize + CODE_SLACK, 0);
    cd.workspace_size = newsize;
    0
}

/// `is_counted_repeat`: `p` is just after '{'.
fn is_counted_repeat(pat: &[u8], mut p: usize) -> bool {
    if !is_digit(pat[p]) {
        return false;
    }
    p += 1;
    while is_digit(pat[p]) {
        p += 1;
    }
    if pat[p] == b'}' {
        return true;
    }
    if pat[p] != b',' {
        return false;
    }
    p += 1;
    if pat[p] == b'}' {
        return true;
    }
    if !is_digit(pat[p]) {
        return false;
    }
    p += 1;
    while is_digit(pat[p]) {
        p += 1;
    }
    pat[p] == b'}'
}

fn hexval(cc: u32) -> u32 {
    let cc = if cc >= b'a' as u32 { cc - 32 } else { cc };
    cc - if cc < b'A' as u32 {
        b'0' as u32
    } else {
        b'A' as u32 - 10
    }
}

/// `check_escape`: `*ptrptr` is on the backslash on entry and on the last
/// character of the escape on exit.
fn check_escape(
    pat: &[u8],
    ptrptr: &mut usize,
    chptr: &mut u32,
    errorcodeptr: &mut i32,
    bracount: i32,
    options: u32,
    isclass: bool,
) -> i32 {
    let mut ptr = *ptrptr + 1;
    let mut c = pat[ptr] as u32;
    let mut escape = 0;

    if c == 0 {
        *errorcodeptr = ERR1;
    } else if c < b'0' as u32 || c > b'z' as u32 {
    } else if ESCAPES[(c - b'0' as u32) as usize] != 0 {
        let i = ESCAPES[(c - b'0' as u32) as usize] as i32;
        if i > 0 {
            c = i as u32;
        } else {
            escape = -i;
        }
    } else {
        // C's i, left 0 by the escapes[] lookup; the \0 and \x loops count on it.
        let mut i = 0;
        let ch = c as u8;
        'sw: {
            match ch {
                b'l' | b'L' => *errorcodeptr = ERR37,
                b'u' => {
                    if options & PCRE_JAVASCRIPT_COMPAT != 0 {
                        if (1..=4).all(|k| DIGITAB[pat[ptr + k] as usize] & CTYPE_XDIGIT != 0) {
                            c = 0;
                            for _ in 0..4 {
                                ptr += 1;
                                c = (c << 4) + hexval(pat[ptr] as u32);
                            }
                            if c > 0xff {
                                *errorcodeptr = ERR76;
                            }
                        }
                    } else {
                        *errorcodeptr = ERR37;
                    }
                }
                b'U' => {
                    if options & PCRE_JAVASCRIPT_COMPAT == 0 {
                        *errorcodeptr = ERR37;
                    }
                }
                b'g' => {
                    if isclass {
                        break 'sw;
                    }
                    if pat[ptr + 1] == b'<' || pat[ptr + 1] == b'\'' {
                        escape = ESC_LC_G;
                        break 'sw;
                    }
                    let braced;
                    if pat[ptr + 1] == b'{' {
                        let mut p = ptr + 2;
                        while pat[p] != 0 && pat[p] != b'}' {
                            if pat[p] != b'-' && !is_digit(pat[p]) {
                                break;
                            }
                            p += 1;
                        }
                        if pat[p] != 0 && pat[p] != b'}' {
                            escape = ESC_LC_K;
                            break 'sw;
                        }
                        braced = true;
                        ptr += 1;
                    } else {
                        braced = false;
                    }
                    let negated = if pat[ptr + 1] == b'-' {
                        ptr += 1;
                        true
                    } else {
                        false
                    };
                    let mut s: i32 = 0;
                    let mut overflow = false;
                    while is_digit(pat[ptr + 1]) {
                        if s > i32::MAX / 10 - 1 {
                            overflow = true;
                            break;
                        }
                        ptr += 1;
                        s = s * 10 + (pat[ptr] - b'0') as i32;
                    }
                    if overflow {
                        while is_digit(pat[ptr + 1]) {
                            ptr += 1;
                        }
                        *errorcodeptr = ERR61;
                        break 'sw;
                    }
                    if braced {
                        ptr += 1;
                        if pat[ptr] != b'}' {
                            *errorcodeptr = ERR57;
                            break 'sw;
                        }
                    }
                    if s == 0 {
                        *errorcodeptr = ERR58;
                        break 'sw;
                    }
                    if negated {
                        if s > bracount {
                            *errorcodeptr = ERR15;
                            break 'sw;
                        }
                        s = bracount - (s - 1);
                    }
                    escape = -s;
                }
                b'1'..=b'9' | b'0' => {
                    if ch != b'0' {
                        if !isclass {
                            let oldptr = ptr;
                            let mut s = (c - b'0' as u32) as i32;
                            let mut overflow = false;
                            while is_digit(pat[ptr + 1]) {
                                if s > i32::MAX / 10 - 1 {
                                    overflow = true;
                                    break;
                                }
                                ptr += 1;
                                s = s * 10 + (pat[ptr] - b'0') as i32;
                            }
                            if overflow {
                                while is_digit(pat[ptr + 1]) {
                                    ptr += 1;
                                }
                                *errorcodeptr = ERR61;
                                break 'sw;
                            }
                            if s < 8 || s <= bracount {
                                escape = -s;
                                break 'sw;
                            }
                            ptr = oldptr;
                        }
                        c = pat[ptr] as u32;
                        if c >= b'8' as u32 {
                            break 'sw;
                        }
                    }
                    // case CHAR_0 (and the fall-through above)
                    c -= b'0' as u32;
                    while {
                        let go = i < 2;
                        i += 1;
                        go
                    } && pat[ptr + 1] >= b'0'
                        && pat[ptr + 1] <= b'7'
                    {
                        ptr += 1;
                        c = c * 8 + pat[ptr] as u32 - b'0' as u32;
                    }
                    if c > 0xff {
                        *errorcodeptr = ERR51;
                    }
                }
                b'o' => {
                    if pat[ptr + 1] != b'{' {
                        *errorcodeptr = ERR81;
                    } else if pat[ptr + 2] == b'}' {
                        *errorcodeptr = ERR86;
                    } else {
                        ptr += 2;
                        c = 0;
                        let mut overflow = false;
                        while pat[ptr] >= b'0' && pat[ptr] <= b'7' {
                            let cc = pat[ptr] as u32;
                            ptr += 1;
                            if c == 0 && cc == b'0' as u32 {
                                continue;
                            }
                            c = (c << 3) + cc - b'0' as u32;
                            if c > 0xff {
                                overflow = true;
                                break;
                            }
                        }
                        if overflow {
                            while pat[ptr] >= b'0' && pat[ptr] <= b'7' {
                                ptr += 1;
                            }
                            *errorcodeptr = ERR34;
                        } else if pat[ptr] == b'}' {
                        } else {
                            *errorcodeptr = ERR80;
                        }
                    }
                }
                b'x' => {
                    if options & PCRE_JAVASCRIPT_COMPAT != 0 {
                        if DIGITAB[pat[ptr + 1] as usize] & CTYPE_XDIGIT != 0
                            && DIGITAB[pat[ptr + 2] as usize] & CTYPE_XDIGIT != 0
                        {
                            c = 0;
                            for _ in 0..2 {
                                ptr += 1;
                                c = (c << 4) + hexval(pat[ptr] as u32);
                            }
                        }
                    } else if pat[ptr + 1] == b'{' {
                        ptr += 2;
                        if pat[ptr] == b'}' {
                            *errorcodeptr = ERR86;
                            break 'sw;
                        }
                        c = 0;
                        let mut overflow = false;
                        while DIGITAB[pat[ptr] as usize] & CTYPE_XDIGIT != 0 {
                            let cc = pat[ptr] as u32;
                            ptr += 1;
                            if c == 0 && cc == b'0' as u32 {
                                continue;
                            }
                            c = (c << 4) + hexval(cc);
                            if c > 0xff {
                                overflow = true;
                                break;
                            }
                        }
                        if overflow {
                            while DIGITAB[pat[ptr] as usize] & CTYPE_XDIGIT != 0 {
                                ptr += 1;
                            }
                            *errorcodeptr = ERR34;
                        } else if pat[ptr] == b'}' {
                        } else {
                            *errorcodeptr = ERR79;
                        }
                    } else {
                        c = 0;
                        while {
                            let go = i < 2;
                            i += 1;
                            go
                        } && DIGITAB[pat[ptr + 1] as usize] & CTYPE_XDIGIT != 0
                        {
                            ptr += 1;
                            c = c * 16 + hexval(pat[ptr] as u32);
                        }
                    }
                }
                b'c' => {
                    ptr += 1;
                    c = pat[ptr] as u32;
                    if c == 0 {
                        *errorcodeptr = ERR2;
                        break 'sw;
                    }
                    if c > 127 {
                        *errorcodeptr = ERR68;
                        break 'sw;
                    }
                    if (b'a' as u32..=b'z' as u32).contains(&c) {
                        c -= 32;
                    }
                    c ^= 0x40;
                }
                _ => {
                    if options & PCRE_EXTRA != 0 {
                        *errorcodeptr = ERR3;
                    }
                }
            }
        }
    }

    if escape == ESC_N && pat[ptr + 1] == b'{' && !is_counted_repeat(pat, ptr + 2) {
        *errorcodeptr = ERR37;
    }
    if options & PCRE_UCP != 0 && (ESC_D..=ESC_LC_W).contains(&escape) {
        escape += ESC_DU - ESC_D;
    }
    *ptrptr = ptr;
    *chptr = c;
    escape
}

/// `read_repeat_counts`: `p` is just after '{'. Returns the '}' position on
/// success, or the current position with `*errorcodeptr` set.
fn read_repeat_counts(
    pat: &[u8],
    mut p: usize,
    minp: &mut i32,
    maxp: &mut i32,
    errorcodeptr: &mut i32,
) -> usize {
    let mut min: i32 = 0;
    let mut max: i32 = -1;
    while is_digit(pat[p]) {
        min = min * 10 + (pat[p] - b'0') as i32;
        p += 1;
        if min > 65535 {
            *errorcodeptr = ERR5;
            return p;
        }
    }
    if pat[p] == b'}' {
        max = min;
    } else {
        p += 1;
        if pat[p] != b'}' {
            max = 0;
            while is_digit(pat[p]) {
                max = max * 10 + (pat[p] - b'0') as i32;
                p += 1;
                if max > 65535 {
                    *errorcodeptr = ERR5;
                    return p;
                }
            }
            if max < min {
                *errorcodeptr = ERR4;
                return p;
            }
        }
    }
    *minp = min;
    *maxp = max;
    p
}

#[inline]
fn oplen(op: u8) -> usize {
    OP_LENGTHS[op as usize] as usize
}

/// `first_significant_code`.
fn first_significant_code(code: &[u8], mut cc: usize, skipassert: bool) -> usize {
    loop {
        match code[cc] {
            OP_ASSERT_NOT | OP_ASSERTBACK | OP_ASSERTBACK_NOT => {
                if !skipassert {
                    return cc;
                }
                loop {
                    cc += get(code, cc + 1);
                    if code[cc] != OP_ALT {
                        break;
                    }
                }
                cc += oplen(code[cc]);
            }
            OP_WORD_BOUNDARY | OP_NOT_WORD_BOUNDARY => {
                if !skipassert {
                    return cc;
                }
                cc += oplen(code[cc]);
            }
            OP_CALLOUT | OP_CREF | OP_DNCREF | OP_RREF | OP_DNRREF | OP_DEF => {
                cc += oplen(code[cc]);
            }
            _ => return cc,
        }
    }
}

/// Skip a group: `do cc += GET(cc, 1); while (*cc == OP_ALT);`.
#[inline]
fn skip_alts(code: &[u8], mut cc: usize) -> usize {
    loop {
        cc += get(code, cc + 1);
        if code[cc] != OP_ALT {
            return cc;
        }
    }
}

/// `find_fixedlength`; `code` is the whole buffer from `cd->start_code`.
fn find_fixedlength(
    code: &[u8],
    at: usize,
    atend: bool,
    recurses: Option<&RecurseCheck<'_>>,
) -> i32 {
    let mut length: i32 = -1;
    let mut branchlength: i32 = 0;
    let mut cc = at + 1 + LINK_SIZE;

    loop {
        let op = code[cc];
        match op {
            OP_CBRA | OP_BRA | OP_ONCE | OP_ONCE_NC | OP_COND => {
                let d = find_fixedlength(
                    code,
                    cc + if op == OP_CBRA { IMM2_SIZE } else { 0 },
                    atend,
                    recurses,
                );
                if d < 0 {
                    return d;
                }
                branchlength += d;
                cc = skip_alts(code, cc);
                cc += 1 + LINK_SIZE;
            }
            OP_ALT | OP_KET | OP_END | OP_ACCEPT | OP_ASSERT_ACCEPT => {
                if length < 0 {
                    length = branchlength;
                } else if length != branchlength {
                    return -1;
                }
                if code[cc] != OP_ALT {
                    return length;
                }
                cc += 1 + LINK_SIZE;
                branchlength = 0;
            }
            OP_RECURSE => {
                if !atend {
                    return -3;
                }
                let cs = get(code, cc + 1);
                let ce = skip_alts(code, cs);
                if cc > cs && cc < ce {
                    return -1;
                }
                if recursed(recurses, cs) {
                    return -1;
                }
                let this_recurse = RecurseCheck {
                    prev: recurses,
                    group: cs,
                };
                let d = find_fixedlength(code, cs + IMM2_SIZE, atend, Some(&this_recurse));
                if d < 0 {
                    return d;
                }
                branchlength += d;
                cc += 1 + LINK_SIZE;
            }
            OP_ASSERT | OP_ASSERT_NOT | OP_ASSERTBACK | OP_ASSERTBACK_NOT => {
                cc = skip_alts(code, cc);
                cc += 1 + LINK_SIZE;
            }
            OP_MARK | OP_PRUNE_ARG | OP_SKIP_ARG | OP_THEN_ARG => {
                cc += code[cc + 1] as usize + oplen(code[cc]);
            }
            OP_CALLOUT | OP_CIRC | OP_CIRCM | OP_CLOSE | OP_COMMIT | OP_CREF | OP_DEF
            | OP_DNCREF | OP_DNRREF | OP_DOLL | OP_DOLLM | OP_EOD | OP_EODN | OP_FAIL
            | OP_NOT_WORD_BOUNDARY | OP_PRUNE | OP_REVERSE | OP_RREF | OP_SET_SOM | OP_SKIP
            | OP_SOD | OP_SOM | OP_THEN | OP_WORD_BOUNDARY => {
                cc += oplen(code[cc]);
            }
            OP_CHAR | OP_CHARI | OP_NOT | OP_NOTI => {
                branchlength += 1;
                cc += 2;
            }
            OP_EXACT | OP_EXACTI | OP_NOTEXACT | OP_NOTEXACTI => {
                branchlength += get2(code, cc + 1) as i32;
                cc += 2 + IMM2_SIZE;
            }
            OP_TYPEEXACT => {
                branchlength += get2(code, cc + 1) as i32;
                if code[cc + 1 + IMM2_SIZE] == OP_PROP || code[cc + 1 + IMM2_SIZE] == OP_NOTPROP {
                    cc += 2;
                }
                cc += 1 + IMM2_SIZE + 1;
            }
            OP_PROP | OP_NOTPROP | OP_HSPACE | OP_VSPACE | OP_NOT_HSPACE | OP_NOT_VSPACE
            | OP_NOT_DIGIT | OP_DIGIT | OP_NOT_WHITESPACE | OP_WHITESPACE | OP_NOT_WORDCHAR
            | OP_WORDCHAR | OP_ANY | OP_ALLANY => {
                if op == OP_PROP || op == OP_NOTPROP {
                    cc += 2;
                }
                branchlength += 1;
                cc += 1;
            }
            OP_ANYBYTE => return -2,
            OP_CLASS | OP_NCLASS => {
                cc += oplen(OP_CLASS);
                match code[cc] {
                    OP_CRSTAR | OP_CRMINSTAR | OP_CRPLUS | OP_CRMINPLUS | OP_CRQUERY
                    | OP_CRMINQUERY | OP_CRPOSSTAR | OP_CRPOSPLUS | OP_CRPOSQUERY => return -1,
                    OP_CRRANGE | OP_CRMINRANGE | OP_CRPOSRANGE => {
                        if get2(code, cc + 1) != get2(code, cc + 1 + IMM2_SIZE) {
                            return -1;
                        }
                        branchlength += get2(code, cc + 1) as i32;
                        cc += 1 + 2 * IMM2_SIZE;
                    }
                    _ => branchlength += 1,
                }
            }
            OP_ANYNL | OP_BRAMINZERO | OP_BRAPOS | OP_BRAPOSZERO | OP_BRAZERO | OP_CBRAPOS
            | OP_EXTUNI | OP_KETRMAX | OP_KETRMIN | OP_KETRPOS | OP_MINPLUS | OP_MINPLUSI
            | OP_MINQUERY | OP_MINQUERYI | OP_MINSTAR | OP_MINSTARI | OP_MINUPTO | OP_MINUPTOI
            | OP_NOTMINPLUS | OP_NOTMINPLUSI | OP_NOTMINQUERY | OP_NOTMINQUERYI | OP_NOTMINSTAR
            | OP_NOTMINSTARI | OP_NOTMINUPTO | OP_NOTMINUPTOI | OP_NOTPLUS | OP_NOTPLUSI
            | OP_NOTPOSPLUS | OP_NOTPOSPLUSI | OP_NOTPOSQUERY | OP_NOTPOSQUERYI | OP_NOTPOSSTAR
            | OP_NOTPOSSTARI | OP_NOTPOSUPTO | OP_NOTPOSUPTOI | OP_NOTQUERY | OP_NOTQUERYI
            | OP_NOTSTAR | OP_NOTSTARI | OP_NOTUPTO | OP_NOTUPTOI | OP_PLUS | OP_PLUSI
            | OP_POSPLUS | OP_POSPLUSI | OP_POSQUERY | OP_POSQUERYI | OP_POSSTAR | OP_POSSTARI
            | OP_POSUPTO | OP_POSUPTOI | OP_QUERY | OP_QUERYI | OP_REF | OP_REFI | OP_DNREF
            | OP_DNREFI | OP_SBRA | OP_SBRAPOS | OP_SCBRA | OP_SCBRAPOS | OP_SCOND
            | OP_SKIPZERO | OP_STAR | OP_STARI | OP_TYPEMINPLUS | OP_TYPEMINQUERY
            | OP_TYPEMINSTAR | OP_TYPEMINUPTO | OP_TYPEPLUS | OP_TYPEPOSPLUS | OP_TYPEPOSQUERY
            | OP_TYPEPOSSTAR | OP_TYPEPOSUPTO | OP_TYPEQUERY | OP_TYPESTAR | OP_TYPEUPTO
            | OP_UPTO | OP_UPTOI => return -1,
            _ => return -4,
        }
    }
}

/// The opcode-length step shared by `find_bracket`, `find_recurse` and
/// `auto_possessify`: the \p/\P extra bytes and the verb argument.
#[inline]
fn extra_len(code: &[u8], cc: usize, c: u8) -> usize {
    match c {
        OP_TYPESTAR | OP_TYPEMINSTAR | OP_TYPEPLUS | OP_TYPEMINPLUS | OP_TYPEQUERY
        | OP_TYPEMINQUERY | OP_TYPEPOSSTAR | OP_TYPEPOSPLUS | OP_TYPEPOSQUERY => {
            if code[cc + 1] == OP_PROP || code[cc + 1] == OP_NOTPROP {
                2
            } else {
                0
            }
        }
        OP_TYPEUPTO | OP_TYPEMINUPTO | OP_TYPEEXACT | OP_TYPEPOSUPTO => {
            if code[cc + 1 + IMM2_SIZE] == OP_PROP || code[cc + 1 + IMM2_SIZE] == OP_NOTPROP {
                2
            } else {
                0
            }
        }
        OP_MARK | OP_PRUNE_ARG | OP_SKIP_ARG | OP_THEN_ARG => code[cc + 1] as usize,
        _ => 0,
    }
}

/// `PRIV(find_bracket)`: the capturing bracket `number`, or for a negative
/// number the first OP_REVERSE.
pub(crate) fn find_bracket(code: &[u8], mut cc: usize, number: i32) -> Option<usize> {
    loop {
        let c = code[cc];
        if c == OP_END {
            return None;
        }
        if c == OP_REVERSE {
            if number < 0 {
                return Some(cc);
            }
            cc += oplen(c);
        } else if c == OP_CBRA || c == OP_SCBRA || c == OP_CBRAPOS || c == OP_SCBRAPOS {
            let n = get2(code, cc + 1 + LINK_SIZE) as i32;
            if n == number {
                return Some(cc);
            }
            cc += oplen(c);
        } else {
            cc += extra_len(code, cc, c);
            cc += oplen(c);
        }
    }
}

/// `find_recurse`.
fn find_recurse(code: &[u8], mut cc: usize) -> Option<usize> {
    loop {
        let c = code[cc];
        if c == OP_END {
            return None;
        }
        if c == OP_RECURSE {
            return Some(cc);
        }
        cc += extra_len(code, cc, c);
        cc += oplen(c);
    }
}

/// `could_be_empty_branch`: scans `cd.code` from `at` to `endcode`.
fn could_be_empty_branch(
    cd: &Cd,
    at: usize,
    endcode: usize,
    recurses: Option<&RecurseCheck<'_>>,
) -> bool {
    let code = &cd.code[..];
    let mut cc = first_significant_code(code, at + oplen(code[at]), true);
    while cc < endcode {
        let mut c = code[cc];

        'next: {
            if c == OP_ASSERT {
                cc = skip_alts(code, cc);
                c = code[cc];
                break 'next;
            }

            if c == OP_RECURSE {
                let mut scode = get(code, cc + 1);
                if cd.workspace_live {
                    let mut tcode = 0;
                    while tcode < cd.hwm {
                        if get(&cd.work, tcode) == cc + 1 {
                            return true;
                        }
                        tcode += LINK_SIZE;
                    }
                    if get(code, scode + 1) == 0 {
                        return true;
                    }
                }
                let endgroup = skip_alts(code, scode);
                if cc >= scode && cc <= endgroup {
                    break 'next;
                }
                if recursed(recurses, scode) {
                    break 'next;
                }
                let mut empty_branch = false;
                let this_recurse = RecurseCheck {
                    prev: recurses,
                    group: scode,
                };
                loop {
                    if could_be_empty_branch(cd, scode, endcode, Some(&this_recurse)) {
                        empty_branch = true;
                        break;
                    }
                    scode += get(code, scode + 1);
                    if code[scode] != OP_ALT {
                        break;
                    }
                }
                if !empty_branch {
                    return false;
                }
                break 'next;
            }

            if c == OP_BRAZERO || c == OP_BRAMINZERO || c == OP_SKIPZERO || c == OP_BRAPOSZERO {
                cc += oplen(c);
                cc = skip_alts(code, cc);
                c = code[cc];
                break 'next;
            }

            if c == OP_SBRA || c == OP_SBRAPOS || c == OP_SCBRA || c == OP_SCBRAPOS {
                cc = skip_alts(code, cc);
                c = code[cc];
                break 'next;
            }

            if c == OP_BRA
                || c == OP_BRAPOS
                || c == OP_CBRA
                || c == OP_CBRAPOS
                || c == OP_ONCE
                || c == OP_ONCE_NC
                || c == OP_COND
                || c == OP_SCOND
            {
                if get(code, cc + 1) == 0 {
                    return true;
                }
                if c == OP_COND && code[cc + get(code, cc + 1)] != OP_ALT {
                    cc += get(code, cc + 1);
                } else {
                    let mut empty_branch = false;
                    loop {
                        if !empty_branch && could_be_empty_branch(cd, cc, endcode, recurses) {
                            empty_branch = true;
                        }
                        cc += get(code, cc + 1);
                        if code[cc] != OP_ALT {
                            break;
                        }
                    }
                    if !empty_branch {
                        return false;
                    }
                }
                c = code[cc];
                break 'next;
            }

            match c {
                OP_CLASS | OP_NCLASS => {
                    let ccode = cc + oplen(OP_CLASS);
                    match code[ccode] {
                        OP_CRSTAR | OP_CRMINSTAR | OP_CRQUERY | OP_CRMINQUERY | OP_CRPOSSTAR
                        | OP_CRPOSQUERY => {}
                        OP_CRRANGE | OP_CRMINRANGE | OP_CRPOSRANGE => {
                            if get2(code, ccode + 1) > 0 {
                                return false;
                            }
                        }
                        _ => return false,
                    }
                }
                OP_ANY | OP_ALLANY | OP_ANYBYTE | OP_PROP | OP_NOTPROP | OP_ANYNL
                | OP_NOT_HSPACE | OP_HSPACE | OP_NOT_VSPACE | OP_VSPACE | OP_EXTUNI
                | OP_NOT_DIGIT | OP_DIGIT | OP_NOT_WHITESPACE | OP_WHITESPACE | OP_NOT_WORDCHAR
                | OP_WORDCHAR | OP_CHAR | OP_CHARI | OP_NOT | OP_NOTI | OP_PLUS | OP_PLUSI
                | OP_MINPLUS | OP_MINPLUSI | OP_NOTPLUS | OP_NOTPLUSI | OP_NOTMINPLUS
                | OP_NOTMINPLUSI | OP_POSPLUS | OP_POSPLUSI | OP_NOTPOSPLUS | OP_NOTPOSPLUSI
                | OP_EXACT | OP_EXACTI | OP_NOTEXACT | OP_NOTEXACTI | OP_TYPEPLUS
                | OP_TYPEMINPLUS | OP_TYPEPOSPLUS | OP_TYPEEXACT => return false,
                OP_TYPESTAR | OP_TYPEMINSTAR | OP_TYPEPOSSTAR | OP_TYPEQUERY | OP_TYPEMINQUERY
                | OP_TYPEPOSQUERY => {
                    if code[cc + 1] == OP_PROP || code[cc + 1] == OP_NOTPROP {
                        cc += 2;
                    }
                }
                OP_TYPEUPTO | OP_TYPEMINUPTO | OP_TYPEPOSUPTO => {
                    if code[cc + 1 + IMM2_SIZE] == OP_PROP || code[cc + 1 + IMM2_SIZE] == OP_NOTPROP
                    {
                        cc += 2;
                    }
                }
                OP_KET | OP_KETRMAX | OP_KETRMIN | OP_KETRPOS | OP_ALT => return true,
                OP_MARK | OP_PRUNE_ARG | OP_SKIP_ARG | OP_THEN_ARG => {
                    cc += code[cc + 1] as usize;
                }
                _ => {}
            }
        }

        cc = first_significant_code(code, cc + oplen(c), true);
    }
    true
}

/// `could_be_empty`: the current branch and every enclosing one at or
/// after `at`.
fn could_be_empty(cd: &Cd, at: usize, endcode: usize, mut bcptr: Option<&BranchChain<'_>>) -> bool {
    while let Some(bc) = bcptr {
        if bc.current_branch.get() < at {
            break;
        }
        if !could_be_empty_branch(cd, bc.current_branch.get(), endcode, None) {
            return false;
        }
        bcptr = bc.outer;
    }
    true
}

/// `get_repeat_base`.
fn get_repeat_base(c: u8) -> u8 {
    if c > OP_TYPEPOSUPTO {
        c
    } else if c >= OP_TYPESTAR {
        OP_TYPESTAR
    } else if c >= OP_NOTSTARI {
        OP_NOTSTARI
    } else if c >= OP_NOTSTAR {
        OP_NOTSTAR
    } else if c >= OP_STARI {
        OP_STARI
    } else {
        OP_STAR
    }
}

/// `get_chr_property_list`: the position after the opcode, or None.
fn get_chr_property_list(
    code: &[u8],
    mut cc: usize,
    cd: &Cd,
    list: &mut [u32; 8],
) -> Option<usize> {
    let mut c = code[cc];
    list[0] = c as u32;
    list[1] = 0;
    cc += 1;

    if (OP_STAR..=OP_TYPEPOSUPTO).contains(&c) {
        let base = get_repeat_base(c);
        c -= base - OP_STAR;
        if c == OP_UPTO || c == OP_MINUPTO || c == OP_EXACT || c == OP_POSUPTO {
            cc += IMM2_SIZE;
        }
        list[1] = (c != OP_PLUS && c != OP_MINPLUS && c != OP_EXACT && c != OP_POSPLUS) as u32;
        match base {
            OP_STAR => list[0] = OP_CHAR as u32,
            OP_STARI => list[0] = OP_CHARI as u32,
            OP_NOTSTAR => list[0] = OP_NOT as u32,
            OP_NOTSTARI => list[0] = OP_NOTI as u32,
            OP_TYPESTAR => {
                list[0] = code[cc] as u32;
                cc += 1;
            }
            _ => {}
        }
        c = list[0] as u8;
    }

    match c {
        OP_NOT_DIGIT | OP_DIGIT | OP_NOT_WHITESPACE | OP_WHITESPACE | OP_NOT_WORDCHAR
        | OP_WORDCHAR | OP_ANY | OP_ALLANY | OP_ANYNL | OP_NOT_HSPACE | OP_HSPACE
        | OP_NOT_VSPACE | OP_VSPACE | OP_EXTUNI | OP_EODN | OP_EOD | OP_DOLL | OP_DOLLM => Some(cc),
        OP_CHAR | OP_NOT => {
            list[2] = code[cc] as u32;
            list[3] = NOTACHAR;
            Some(cc + 1)
        }
        OP_CHARI | OP_NOTI => {
            list[0] = if c == OP_CHARI { OP_CHAR } else { OP_NOT } as u32;
            let chr = code[cc] as u32;
            cc += 1;
            list[2] = chr;
            list[3] = cd.fcc(chr) as u32;
            if chr == list[3] {
                list[3] = NOTACHAR;
            } else {
                list[4] = NOTACHAR;
            }
            Some(cc)
        }
        OP_NCLASS | OP_CLASS => {
            let mut end = cc + 32;
            match code[end] {
                OP_CRSTAR | OP_CRMINSTAR | OP_CRQUERY | OP_CRMINQUERY | OP_CRPOSSTAR
                | OP_CRPOSQUERY => {
                    list[1] = 1;
                    end += 1;
                }
                OP_CRPLUS | OP_CRMINPLUS | OP_CRPOSPLUS => end += 1,
                OP_CRRANGE | OP_CRMINRANGE | OP_CRPOSRANGE => {
                    list[1] = (get2(code, end + 1) == 0) as u32;
                    end += 1 + 2 * IMM2_SIZE;
                }
                _ => {}
            }
            list[2] = (end - cc) as u32;
            Some(end)
        }
        _ => None,
    }
}

/// `compare_opcodes`: whether the base item can be possessified against
/// what follows at `cc`.
fn compare_opcodes(
    code: &[u8],
    mut cc: usize,
    cd: &Cd,
    base_list: &[u32; 8],
    base_end: usize,
    rec_limit: &mut i32,
) -> bool {
    let mut list = [0u32; 8];
    let mut entered_a_group = false;

    if *rec_limit == 0 {
        return false;
    }
    *rec_limit -= 1;

    loop {
        let mut c = code[cc];

        if c == OP_CALLOUT {
            cc += oplen(c);
            continue;
        }

        if c == OP_ALT {
            cc = skip_alts(code, cc);
            c = code[cc];
        }

        match c {
            OP_END | OP_KETRPOS => return base_list[1] != 0,
            OP_KET => {
                if base_list[1] == 0 {
                    return false;
                }
                match code[cc - get(code, cc + 1)] {
                    OP_ASSERT | OP_ASSERT_NOT | OP_ASSERTBACK | OP_ASSERTBACK_NOT | OP_ONCE
                    | OP_ONCE_NC => return !entered_a_group,
                    _ => {}
                }
                cc += oplen(c);
                continue;
            }
            OP_ONCE | OP_ONCE_NC | OP_BRA | OP_CBRA => {
                let mut next_code = cc + get(code, cc + 1);
                cc += oplen(c);
                while code[next_code] == OP_ALT {
                    if !compare_opcodes(code, cc, cd, base_list, base_end, rec_limit) {
                        return false;
                    }
                    cc = next_code + 1 + LINK_SIZE;
                    next_code += get(code, next_code + 1);
                }
                entered_a_group = true;
                continue;
            }
            OP_BRAZERO | OP_BRAMINZERO => {
                let mut next_code = cc + 1;
                let n = code[next_code];
                if n != OP_BRA && n != OP_CBRA && n != OP_ONCE && n != OP_ONCE_NC {
                    return false;
                }
                next_code = skip_alts(code, next_code);
                next_code += 1 + LINK_SIZE;
                if !compare_opcodes(code, next_code, cd, base_list, base_end, rec_limit) {
                    return false;
                }
                cc += oplen(c);
                continue;
            }
            _ => {}
        }

        cc = match get_chr_property_list(code, cc, cd, &mut list) {
            Some(n) => n,
            None => return false,
        };

        // Which side is the small character list, if either.
        let (chr_list, other): (&[u32], &[u32; 8]);
        let other_is_list;
        if base_list[0] == OP_CHAR as u32 {
            chr_list = &base_list[2..];
            other = &list;
            other_is_list = true;
        } else if list[0] == OP_CHAR as u32 {
            chr_list = &list[2..];
            other = base_list;
            other_is_list = false;
        } else if base_list[0] == OP_CLASS as u32
            || list[0] == OP_CLASS as u32
            || base_list[0] == OP_NCLASS as u32
            || list[0] == OP_NCLASS as u32
        {
            let set1;
            let list_ptr: &[u32; 8];
            let lp_is_list;
            if base_list[0] == OP_CLASS as u32 || base_list[0] == OP_NCLASS as u32 {
                set1 = base_end - base_list[2] as usize;
                list_ptr = &list;
                lp_is_list = true;
            } else {
                set1 = cc - list[2] as usize;
                list_ptr = base_list;
                lp_is_list = false;
            }
            let mut invert_bits = false;
            let set2: &[u8] = match list_ptr[0] as u8 {
                OP_CLASS | OP_NCLASS => {
                    let at = (if lp_is_list { cc } else { base_end }) - list_ptr[2] as usize;
                    &code[at..at + 32]
                }
                OP_NOT_DIGIT | OP_DIGIT => {
                    invert_bits = list_ptr[0] as u8 == OP_NOT_DIGIT;
                    &cd.tables[CBITS_OFFSET + CBIT_DIGIT..][..32]
                }
                OP_NOT_WHITESPACE | OP_WHITESPACE => {
                    invert_bits = list_ptr[0] as u8 == OP_NOT_WHITESPACE;
                    &cd.tables[CBITS_OFFSET + CBIT_SPACE..][..32]
                }
                OP_NOT_WORDCHAR | OP_WORDCHAR => {
                    invert_bits = list_ptr[0] as u8 == OP_NOT_WORDCHAR;
                    &cd.tables[CBITS_OFFSET + CBIT_WORD..][..32]
                }
                _ => return false,
            };
            let s1 = &code[set1..set1 + 32];
            for k in 0..32 {
                let b = if invert_bits { !set2[k] } else { set2[k] };
                if s1[k] & b != 0 {
                    return false;
                }
            }
            if list[1] == 0 {
                return true;
            }
            continue;
        } else {
            let leftop = base_list[0];
            let rightop = list[0];
            let f = FIRST_AUTOTAB_OP as u32;
            let accepted = leftop >= f
                && leftop <= LAST_AUTOTAB_LEFT_OP as u32
                && rightop >= f
                && rightop <= LAST_AUTOTAB_RIGHT_OP as u32
                && AUTOPOSSTAB[(leftop - f) as usize][(rightop - f) as usize] != 0;
            if !accepted {
                return false;
            }
            if list[1] == 0 {
                return true;
            }
            continue;
        }

        // One side is a small character list: check each against the other.
        let mut k = 0;
        loop {
            let chr = chr_list[k];
            match other[0] as u8 {
                OP_CHAR => {
                    let mut o = 2;
                    loop {
                        if chr == other[o] {
                            return false;
                        }
                        o += 1;
                        if other[o] == NOTACHAR {
                            break;
                        }
                    }
                }
                OP_NOT => {
                    let mut o = 2;
                    loop {
                        if chr == other[o] {
                            break;
                        }
                        o += 1;
                        if other[o] == NOTACHAR {
                            break;
                        }
                    }
                    if other[o] == NOTACHAR {
                        return false;
                    }
                }
                OP_DIGIT => {
                    if chr < 256 && cd.ctypes(chr) & CTYPE_DIGIT != 0 {
                        return false;
                    }
                }
                OP_NOT_DIGIT => {
                    if chr > 255 || cd.ctypes(chr) & CTYPE_DIGIT == 0 {
                        return false;
                    }
                }
                OP_WHITESPACE => {
                    if chr < 256 && cd.ctypes(chr) & CTYPE_SPACE != 0 {
                        return false;
                    }
                }
                OP_NOT_WHITESPACE => {
                    if chr > 255 || cd.ctypes(chr) & CTYPE_SPACE == 0 {
                        return false;
                    }
                }
                OP_WORDCHAR => {
                    // C tests `chr < 255` here, unlike its neighbours.
                    if chr < 255 && cd.ctypes(chr) & CTYPE_WORD != 0 {
                        return false;
                    }
                }
                OP_NOT_WORDCHAR => {
                    if chr > 255 || cd.ctypes(chr) & CTYPE_WORD == 0 {
                        return false;
                    }
                }
                OP_HSPACE => {
                    if is_hspace(chr) {
                        return false;
                    }
                }
                OP_NOT_HSPACE => {
                    if !is_hspace(chr) {
                        return false;
                    }
                }
                OP_ANYNL | OP_VSPACE => {
                    if is_vspace(chr) {
                        return false;
                    }
                }
                OP_NOT_VSPACE => {
                    if !is_vspace(chr) {
                        return false;
                    }
                }
                OP_DOLL | OP_EODN => {
                    if matches!(chr, 0x0d | 0x0a | 0x0b | 0x0c | 0x85 | 0x2028 | 0x2029) {
                        return false;
                    }
                }
                OP_EOD => {}
                OP_NCLASS | OP_CLASS => {
                    if other[0] as u8 == OP_NCLASS && chr > 255 {
                        return false;
                    }
                    if chr <= 255 {
                        let at = (if other_is_list { cc } else { base_end }) - other[2] as usize;
                        if code[at + (chr >> 3) as usize] & (1 << (chr & 7)) != 0 {
                            return false;
                        }
                    }
                }
                _ => return false,
            }
            k += 1;
            if chr_list[k] == NOTACHAR {
                break;
            }
        }

        if list[1] == 0 {
            return true;
        }
    }
}

/// `auto_possessify`: rewrites `cd.code` in place.
fn auto_possessify(cd: &mut Cd) {
    let mut cc = 0;
    let mut list = [0u32; 8];
    loop {
        let mut c = cd.code[cc];

        if c as usize >= OP_TABLE_LENGTH {
            return;
        }

        if (OP_STAR..=OP_TYPEPOSUPTO).contains(&c) {
            c -= get_repeat_base(c) - OP_STAR;
            let end = if c <= OP_MINUPTO {
                get_chr_property_list(&cd.code, cc, cd, &mut list)
            } else {
                None
            };
            list[1] = (c == OP_STAR || c == OP_PLUS || c == OP_QUERY || c == OP_UPTO) as u32;
            let mut rec_limit = 1000;
            if let Some(end) = end
                && compare_opcodes(&cd.code, end, cd, &list, end, &mut rec_limit)
            {
                let delta = match c {
                    OP_STAR => OP_POSSTAR - OP_STAR,
                    OP_MINSTAR => OP_POSSTAR - OP_MINSTAR,
                    OP_PLUS => OP_POSPLUS - OP_PLUS,
                    OP_MINPLUS => OP_POSPLUS - OP_MINPLUS,
                    OP_QUERY => OP_POSQUERY - OP_QUERY,
                    OP_MINQUERY => OP_POSQUERY - OP_MINQUERY,
                    OP_UPTO => OP_POSUPTO - OP_UPTO,
                    OP_MINUPTO => OP_POSUPTO - OP_MINUPTO,
                    _ => 0,
                };
                cd.code[cc] += delta;
            }
            c = cd.code[cc];
        } else if c == OP_CLASS || c == OP_NCLASS {
            let repeat_opcode = cc + 1 + 32;
            c = cd.code[repeat_opcode];
            if (OP_CRSTAR..=OP_CRMINRANGE).contains(&c) {
                let end = get_chr_property_list(&cd.code, cc, cd, &mut list)
                    .expect("a class always has a property list");
                list[1] = (c & 1 == 0) as u32;
                let mut rec_limit = 1000;
                if compare_opcodes(&cd.code, end, cd, &list, end, &mut rec_limit) {
                    cd.code[repeat_opcode] = match c {
                        OP_CRSTAR | OP_CRMINSTAR => OP_CRPOSSTAR,
                        OP_CRPLUS | OP_CRMINPLUS => OP_CRPOSPLUS,
                        OP_CRQUERY | OP_CRMINQUERY => OP_CRPOSQUERY,
                        _ => OP_CRPOSRANGE,
                    };
                }
            }
            c = cd.code[cc];
        }

        if c == OP_END {
            return;
        }
        cc += extra_len(&cd.code, cc, c);
        cc += oplen(c);
    }
}

/// `check_posix_syntax`: `ptr` is on the '['; returns the terminator
/// position.
fn check_posix_syntax(pat: &[u8], mut ptr: usize) -> Option<usize> {
    ptr += 1;
    let terminator = pat[ptr];
    ptr += 1;
    while pat[ptr] != 0 {
        if pat[ptr] == b'\\' && (pat[ptr + 1] == b']' || pat[ptr + 1] == b'\\') {
            ptr += 1;
        } else if (pat[ptr] == b'[' && pat[ptr + 1] == terminator) || pat[ptr] == b']' {
            return None;
        } else if pat[ptr] == terminator && pat[ptr + 1] == b']' {
            return Some(ptr);
        }
        ptr += 1;
    }
    None
}

/// `check_posix_name`.
fn check_posix_name(pat: &[u8], ptr: usize, len: usize) -> i32 {
    for (yield_, name) in POSIX_NAMES.iter().enumerate() {
        if len == name.len() && &pat[ptr..ptr + len] == *name {
            return yield_ as i32;
        }
    }
    -1
}

/// `adjust_recurse`: the code from `group` is about to move by `adjust`.
fn adjust_recurse(cd: &mut Cd, group: usize, adjust: i32, save_hwm_offset: usize) {
    let mut ptr = group;
    while let Some(p) = find_recurse(&cd.code, ptr) {
        ptr = p;
        let mut hc = save_hwm_offset;
        while hc < cd.hwm {
            let offset = get(&cd.work, hc);
            if offset == ptr + 1 {
                break;
            }
            hc += LINK_SIZE;
        }
        if hc >= cd.hwm {
            let offset = get(&cd.code, ptr + 1);
            if offset >= group {
                put(&mut cd.code, ptr + 1, (offset as i32 + adjust) as usize);
            }
        }
        ptr += 1 + LINK_SIZE;
    }
    let mut hc = save_hwm_offset;
    while hc < cd.hwm {
        let offset = get(&cd.work, hc);
        put(&mut cd.work, hc, (offset as i32 + adjust) as usize);
        hc += LINK_SIZE;
    }
}

/// `auto_callout`.
fn auto_callout(cd: &mut Cd, mut code: usize, ptr: usize) -> usize {
    cd.code[code] = OP_CALLOUT;
    cd.code[code + 1] = 255;
    code += 2;
    let off = cd.poff(ptr) as usize;
    put(&mut cd.code, code, off);
    put(&mut cd.code, code + LINK_SIZE, 0);
    code + 2 * LINK_SIZE
}

/// `complete_callout`.
fn complete_callout(cd: &mut Cd, previous_callout: usize, ptr: usize) {
    let length = cd.poff(ptr) - get(&cd.code, previous_callout + 2) as i32;
    put(
        &mut cd.code,
        previous_callout + 2 + LINK_SIZE,
        length as usize,
    );
}

/// `add_to_class` without the >255 extra data, which 8-bit non-UTF never
/// emits.
fn add_to_class(classbits: &mut [u8; 32], options: u32, cd: &Cd, start: u32, end: u32) -> i32 {
    let classbits_end = end.min(0xff);
    let mut n8 = 0;
    if options & PCRE_CASELESS != 0 {
        let mut c = start;
        while c <= classbits_end {
            setbit(classbits, cd.fcc(c) as u32);
            n8 += 1;
            c += 1;
        }
    }
    let mut c = start;
    while c <= classbits_end {
        setbit(classbits, c);
        n8 += 1;
        c += 1;
    }
    n8
}

/// `add_list_to_class`; `p` ends with NOTACHAR.
fn add_list_to_class(
    classbits: &mut [u8; 32],
    options: u32,
    cd: &Cd,
    p: &[u32],
    except: u32,
) -> i32 {
    let mut n8 = 0;
    let mut i = 0;
    while p[i] < NOTACHAR {
        let mut n = 0;
        if p[i] != except {
            while p[i + n + 1] == p[i].wrapping_add(n as u32 + 1) {
                n += 1;
            }
            n8 += add_to_class(classbits, options, cd, p[i], p[i + n]);
        }
        i += n + 1;
    }
    n8
}

/// `add_not_list_to_class`; `p` ends with NOTACHAR.
fn add_not_list_to_class(classbits: &mut [u8; 32], options: u32, cd: &Cd, p: &[u32]) -> i32 {
    let mut n8 = 0;
    let mut i = 0;
    if p[0] > 0 {
        n8 += add_to_class(classbits, options, cd, 0, p[0] - 1);
    }
    while p[i] < NOTACHAR {
        while p[i + 1] == p[i] + 1 {
            i += 1;
        }
        let end = if p[i + 1] == NOTACHAR {
            0xffff_ffff
        } else {
            p[i + 1] - 1
        };
        n8 += add_to_class(classbits, options, cd, p[i] + 1, end);
        i += 1;
    }
    n8
}

// ---- compile_branch ----

/// The in/out arguments `compile_branch` and `compile_regex` share:
/// `*optionsptr`, `*codeptr`, `*ptrptr` and the four first/req outputs.
#[derive(Clone, Copy, Default)]
struct BranchIo {
    options: u32,
    code: usize,
    ptr: usize,
    firstchar: u32,
    firstcharflags: i32,
    reqchar: u32,
    reqcharflags: i32,
}

/// The `compile_branch` goto targets reachable from more than one place.
#[derive(Clone, Copy)]
enum Lbl {
    Switch,
    NormalChar,
    OneChar,
    Repeat,
    OutputSingleRepeat,
    EndRepeat,
    NamedRefOrRecurse,
    HandleReference,
    HandleNumericalRecursion,
    HandleRecursion,
    OtherCharAfterQuery,
    DefineName,
    NumberedGroup,
}

/// How a piece of the item loop ends: `Next` is C's `continue`/`break`
/// (on to `ptr++`), `Redo` is `goto REDO_LOOP`, `Fail` is `goto FAILED`
/// and `Done` the successful `return TRUE`.
enum Flow {
    Next,
    Redo,
    Goto(Lbl),
    Fail,
    Done,
}

/// `IS_NEWLINE(p)` as `compile_branch` uses it on the pattern.
fn is_newline_pat(cd: &mut Cd, p: usize) -> bool {
    if cd.nltype != NLTYPE_FIXED {
        p < cd.end_pattern
            && super::newline::is_newline(&cd.pat, p, cd.nltype, cd.end_pattern, &mut cd.nllen)
    } else {
        p as isize <= cd.end_pattern as isize - cd.nllen as isize
            && cd.pat[p] == cd.nl[0]
            && (cd.nllen == 1 || cd.pat[p + 1] == cd.nl[1])
    }
}

/// The locals of one `compile_branch` call, including the ones C declares
/// per iteration, which keep their storage across iterations there too.
struct Br<'a, 'b> {
    lengthptr: Option<&'a mut i32>,
    bcptr: Option<&'b BranchChain<'b>>,
    cond_depth: i32,
    /// `*optionsptr`, which C writes as soon as an option setting ends.
    optionsptr: u32,
    out: BranchIo,

    repeat_type: i32,
    op_type: i32,
    repeat_min: i32,
    repeat_max: i32,
    bravalue: u8,
    greedy_default: i32,
    greedy_non_default: i32,
    firstchar: u32,
    reqchar: u32,
    firstcharflags: i32,
    reqcharflags: i32,
    zeroreqchar: u32,
    zerofirstchar: u32,
    zeroreqcharflags: i32,
    zerofirstcharflags: i32,
    req_caseopt: i32,
    reqvary: i32,
    tempreqvary: i32,
    options: u32,
    after_manual_callout: i32,
    length_prevgroup: i32,
    c: u32,
    escape: i32,
    code: usize,
    last_code: usize,
    orig_code: usize,
    tempcode: usize,
    inescq: bool,
    groupsetfirstchar: bool,
    ptr: usize,
    nestptr: Option<usize>,
    previous: Option<usize>,
    previous_callout: Option<usize>,
    item_hwm_offset: usize,
    classbits: [u8; 32],

    negate_class: bool,
    should_flip_negation: bool,
    possessive_quantifier: bool,
    is_quantifier: bool,
    is_recurse: bool,
    reset_bracount: bool,
    class_has_8bitchar: i32,
    class_one_char: i32,
    newoptions: u32,
    recno: i32,
    refsign: i32,
    skipbytes: i32,
    subreqchar: u32,
    subfirstchar: u32,
    subreqcharflags: i32,
    subfirstcharflags: i32,
    terminator: i32,
    mclength: u32,
    mcbuffer: u8,
    tempbracount: u32,
}

fn compile_branch(
    cd: &mut Cd,
    io: &mut BranchIo,
    bcptr: Option<&BranchChain<'_>>,
    cond_depth: i32,
    lengthptr: Option<&mut i32>,
) -> bool {
    let options = io.options;
    let greedy_default = (options & PCRE_UNGREEDY != 0) as i32;
    let mut b = Br {
        lengthptr,
        bcptr,
        cond_depth,
        optionsptr: options,
        out: *io,
        repeat_type: 0,
        op_type: 0,
        repeat_min: 0,
        repeat_max: 0,
        bravalue: 0,
        greedy_default,
        greedy_non_default: greedy_default ^ 1,
        firstchar: 0,
        reqchar: 0,
        firstcharflags: REQ_UNSET,
        reqcharflags: REQ_UNSET,
        zeroreqchar: 0,
        zerofirstchar: 0,
        zeroreqcharflags: REQ_UNSET,
        zerofirstcharflags: REQ_UNSET,
        req_caseopt: if options & PCRE_CASELESS != 0 {
            REQ_CASELESS
        } else {
            0
        },
        reqvary: 0,
        tempreqvary: 0,
        options,
        after_manual_callout: 0,
        length_prevgroup: 0,
        c: 0,
        escape: 0,
        code: io.code,
        last_code: io.code,
        orig_code: io.code,
        tempcode: 0,
        inescq: false,
        groupsetfirstchar: false,
        ptr: io.ptr,
        nestptr: None,
        previous: None,
        previous_callout: None,
        item_hwm_offset: 0,
        classbits: [0; 32],
        negate_class: false,
        should_flip_negation: false,
        possessive_quantifier: false,
        is_quantifier: false,
        is_recurse: false,
        reset_bracount: false,
        class_has_8bitchar: 0,
        class_one_char: 0,
        newoptions: 0,
        recno: 0,
        refsign: 0,
        skipbytes: 0,
        subreqchar: 0,
        subfirstchar: 0,
        subreqcharflags: 0,
        subfirstcharflags: 0,
        terminator: 0,
        mclength: 0,
        mcbuffer: 0,
        tempbracount: 0,
    };
    let ok = b.run(cd);
    if ok {
        *io = b.out;
    } else {
        io.ptr = b.ptr;
    }
    io.options = b.optionsptr;
    ok
}

impl Br<'_, '_> {
    fn pass1(&self) -> bool {
        self.lengthptr.is_some()
    }

    /// `*lengthptr += n` with the `OFLOW_MAX` check; false is ERR20.
    fn add_length(&mut self, cd: &mut Cd, n: i32) -> bool {
        let len = self.lengthptr.as_deref_mut().expect("pass 1");
        if OFLOW_MAX - *len < n {
            cd.errorcode = ERR20;
            return false;
        }
        *len += n;
        true
    }

    fn emit(&mut self, cd: &mut Cd, op: u8) {
        cd.code[self.code] = op;
        self.code += 1;
    }

    fn pc(&self, cd: &Cd, off: usize) -> u8 {
        cd.pat[self.ptr + off]
    }

    fn run(&mut self, cd: &mut Cd) -> bool {
        let mut advance = false;
        loop {
            if advance {
                self.ptr += 1;
            }
            advance = true;
            let mut flow = self.loop_top(cd);
            loop {
                match flow {
                    Flow::Next => break,
                    Flow::Redo => {
                        advance = false;
                        break;
                    }
                    Flow::Goto(l) => flow = self.dispatch(cd, l),
                    Flow::Fail => return false,
                    Flow::Done => return true,
                }
            }
        }
    }

    fn dispatch(&mut self, cd: &mut Cd, l: Lbl) -> Flow {
        match l {
            Lbl::Switch => self.switch(cd),
            Lbl::NormalChar => self.normal_char(cd),
            Lbl::OneChar => self.one_char(cd),
            Lbl::Repeat => self.repeat(cd),
            Lbl::OutputSingleRepeat => self.output_single_repeat(cd),
            Lbl::EndRepeat => self.end_repeat(cd),
            Lbl::NamedRefOrRecurse => self.named_ref_or_recurse(cd),
            Lbl::HandleReference => self.handle_reference(cd),
            Lbl::HandleNumericalRecursion => self.handle_numerical_recursion(cd),
            Lbl::HandleRecursion => self.handle_recursion(cd),
            Lbl::OtherCharAfterQuery => self.other_char_after_query(cd),
            Lbl::DefineName => self.define_name(cd),
            Lbl::NumberedGroup => self.numbered_group(cd),
        }
    }

    /// From `REDO_LOOP:` to the item `switch`.
    fn loop_top(&mut self, cd: &mut Cd) -> Flow {
        self.c = cd.pat[self.ptr] as u32;
        if self.c == 0
            && let Some(n) = self.nestptr.take()
        {
            self.ptr = n;
            self.c = cd.pat[self.ptr] as u32;
        }

        if self.pass1() {
            if self.code > cd.workspace_size - WORK_SIZE_SAFETY_MARGIN {
                cd.errorcode = if self.code >= cd.workspace_size {
                    ERR52
                } else {
                    ERR87
                };
                return Flow::Fail;
            }
            if self.code < self.last_code {
                self.code = self.last_code;
            }
            if !self.add_length(cd, (self.code - self.last_code) as i32) {
                return Flow::Fail;
            }
            match self.previous {
                Some(p) => {
                    if p > self.orig_code {
                        cd.code.copy_within(p..self.code, self.orig_code);
                        self.code -= p - self.orig_code;
                        self.previous = Some(self.orig_code);
                    }
                }
                None => self.code = self.orig_code,
            }
            self.last_code = self.code;
        } else if cd.hwm > cd.workspace_size {
            cd.errorcode = ERR52;
            return Flow::Fail;
        }

        if self.c != 0 {
            if self.c == b'\\' as u32 && self.pc(cd, 1) == b'E' {
                self.inescq = false;
                self.ptr += 1;
                return Flow::Next;
            } else if self.inescq {
                if let Some(pc) = self.previous_callout.take()
                    && !self.pass1()
                {
                    complete_callout(cd, pc, self.ptr);
                }
                if self.options & PCRE_AUTO_CALLOUT != 0 {
                    self.previous_callout = Some(self.code);
                    self.code = auto_callout(cd, self.code, self.ptr);
                }
                return Flow::Goto(Lbl::NormalChar);
            }
            if self.c == b'\\' as u32 && self.pc(cd, 1) == b'Q' {
                self.inescq = true;
                self.ptr += 1;
                return Flow::Next;
            }
        }

        if self.options & PCRE_EXTENDED != 0 {
            let wscptr = self.ptr;
            while self.c <= 255 && cd.ctypes(self.c) & CTYPE_SPACE != 0 {
                self.ptr += 1;
                self.c = cd.pat[self.ptr] as u32;
            }
            if self.c == b'#' as u32 {
                self.ptr += 1;
                while cd.pat[self.ptr] != 0 {
                    if is_newline_pat(cd, self.ptr) {
                        self.ptr += cd.nllen as usize;
                        break;
                    }
                    self.ptr += 1;
                }
            }
            if self.ptr > wscptr {
                return Flow::Redo;
            }
        }

        if self.c == b'(' as u32 && self.pc(cd, 1) == b'?' && self.pc(cd, 2) == b'#' {
            self.ptr += 3;
            while cd.pat[self.ptr] != 0 && cd.pat[self.ptr] != b')' {
                self.ptr += 1;
            }
            if cd.pat[self.ptr] == 0 {
                cd.errorcode = ERR18;
                return Flow::Fail;
            }
            return Flow::Next;
        }

        let c = self.c;
        self.is_quantifier = c == b'*' as u32
            || c == b'+' as u32
            || c == b'?' as u32
            || (c == b'{' as u32 && is_counted_repeat(&cd.pat, self.ptr + 1));

        if !self.is_quantifier && self.previous_callout.is_some() && self.nestptr.is_none() && {
            let v = self.after_manual_callout;
            self.after_manual_callout -= 1;
            v <= 0
        } {
            if !self.pass1() {
                complete_callout(cd, self.previous_callout.unwrap(), self.ptr);
            }
            self.previous_callout = None;
        }

        if self.options & PCRE_AUTO_CALLOUT != 0 && !self.is_quantifier && self.nestptr.is_none() {
            self.previous_callout = Some(self.code);
            self.code = auto_callout(cd, self.code, self.ptr);
        }

        Flow::Goto(Lbl::Switch)
    }

    fn switch(&mut self, cd: &mut Cd) -> Flow {
        match self.c as u8 {
            0 | b'|' | b')' => {
                self.out.firstchar = self.firstchar;
                self.out.firstcharflags = self.firstcharflags;
                self.out.reqchar = self.reqchar;
                self.out.reqcharflags = self.reqcharflags;
                self.out.code = self.code;
                self.out.ptr = self.ptr;
                if self.pass1() && !self.add_length(cd, (self.code - self.last_code) as i32) {
                    return Flow::Fail;
                }
                Flow::Done
            }

            b'^' => {
                self.previous = None;
                if self.options & PCRE_MULTILINE != 0 {
                    if self.firstcharflags == REQ_UNSET {
                        self.firstcharflags = REQ_NONE;
                        self.zerofirstcharflags = REQ_NONE;
                    }
                    self.emit(cd, OP_CIRCM);
                } else {
                    self.emit(cd, OP_CIRC);
                }
                Flow::Next
            }

            b'$' => {
                self.previous = None;
                let op = if self.options & PCRE_MULTILINE != 0 {
                    OP_DOLLM
                } else {
                    OP_DOLL
                };
                self.emit(cd, op);
                Flow::Next
            }

            b'.' => {
                if self.firstcharflags == REQ_UNSET {
                    self.firstcharflags = REQ_NONE;
                }
                self.zerofirstchar = self.firstchar;
                self.zerofirstcharflags = self.firstcharflags;
                self.zeroreqchar = self.reqchar;
                self.zeroreqcharflags = self.reqcharflags;
                self.previous = Some(self.code);
                self.item_hwm_offset = cd.hwm;
                let op = if self.options & PCRE_DOTALL != 0 {
                    OP_ALLANY
                } else {
                    OP_ANY
                };
                self.emit(cd, op);
                Flow::Next
            }

            b']' => {
                if cd.external_options & PCRE_JAVASCRIPT_COMPAT != 0 {
                    cd.errorcode = ERR64;
                    return Flow::Fail;
                }
                Flow::Goto(Lbl::NormalChar)
            }

            b'[' => self.class(cd),

            b'{' => {
                if !self.is_quantifier {
                    return Flow::Goto(Lbl::NormalChar);
                }
                self.ptr = read_repeat_counts(
                    &cd.pat,
                    self.ptr + 1,
                    &mut self.repeat_min,
                    &mut self.repeat_max,
                    &mut cd.errorcode,
                );
                if cd.errorcode != 0 {
                    return Flow::Fail;
                }
                Flow::Goto(Lbl::Repeat)
            }
            b'*' => {
                self.repeat_min = 0;
                self.repeat_max = -1;
                Flow::Goto(Lbl::Repeat)
            }
            b'+' => {
                self.repeat_min = 1;
                self.repeat_max = -1;
                Flow::Goto(Lbl::Repeat)
            }
            b'?' => {
                self.repeat_min = 0;
                self.repeat_max = 1;
                Flow::Goto(Lbl::Repeat)
            }

            b'(' => self.paren(cd),

            b'\\' => self.backslash(cd),

            _ => Flow::Goto(Lbl::NormalChar),
        }
    }

    /// `case CHAR_LEFT_SQUARE_BRACKET` up to `END_CLASS`.
    fn class(&mut self, cd: &mut Cd) -> Flow {
        if cd.starts(self.ptr + 1, b"[:<:]]") {
            self.nestptr = Some(self.ptr + 7);
            self.ptr = cd.sub_start;
            return Flow::Redo;
        }
        if cd.starts(self.ptr + 1, b"[:>:]]") {
            self.nestptr = Some(self.ptr + 7);
            self.ptr = cd.sub_end;
            return Flow::Redo;
        }

        self.previous = Some(self.code);
        self.item_hwm_offset = cd.hwm;

        let p1 = self.pc(cd, 1);
        if (p1 == b':' || p1 == b'.' || p1 == b'=')
            && check_posix_syntax(&cd.pat, self.ptr).is_some()
        {
            cd.errorcode = if p1 == b':' { ERR13 } else { ERR31 };
            return Flow::Fail;
        }

        self.negate_class = false;
        loop {
            self.ptr += 1;
            self.c = cd.pat[self.ptr] as u32;
            if self.c == b'\\' as u32 {
                if self.pc(cd, 1) == b'E' {
                    self.ptr += 1;
                } else if cd.starts(self.ptr + 1, b"Q\\E") {
                    self.ptr += 3;
                } else {
                    break;
                }
            } else if !self.negate_class && self.c == b'^' as u32 {
                self.negate_class = true;
            } else {
                break;
            }
        }

        if self.c == b']' as u32 && cd.external_options & PCRE_JAVASCRIPT_COMPAT != 0 {
            let op = if self.negate_class {
                OP_ALLANY
            } else {
                OP_FAIL
            };
            self.emit(cd, op);
            if self.firstcharflags == REQ_UNSET {
                self.firstcharflags = REQ_NONE;
            }
            self.zerofirstchar = self.firstchar;
            self.zerofirstcharflags = self.firstcharflags;
            return Flow::Next;
        }

        self.should_flip_negation = false;
        self.class_has_8bitchar = 0;
        self.class_one_char = 0;
        self.classbits = [0; 32];

        if self.c != 0 {
            loop {
                if let Some(f) = self.class_item(cd) {
                    return f;
                }
                self.ptr += 1;
                self.c = cd.pat[self.ptr] as u32;
                if self.c == 0
                    && let Some(n) = self.nestptr.take()
                {
                    self.ptr = n + 1;
                    self.c = cd.pat[self.ptr] as u32;
                }
                if self.c == 0 || (self.c == b']' as u32 && !self.inescq) {
                    break;
                }
            }
        }

        if self.c == 0 {
            cd.errorcode = ERR6;
            return Flow::Fail;
        }

        if self.firstcharflags == REQ_UNSET {
            self.firstcharflags = REQ_NONE;
        }
        self.zerofirstchar = self.firstchar;
        self.zerofirstcharflags = self.firstcharflags;
        self.zeroreqchar = self.reqchar;
        self.zeroreqcharflags = self.reqcharflags;

        let op = if self.negate_class == self.should_flip_negation {
            OP_CLASS
        } else {
            OP_NCLASS
        };
        self.emit(cd, op);
        if !self.pass1() {
            if self.negate_class {
                for b in self.classbits.iter_mut() {
                    *b = !*b;
                }
            }
            cd.code[self.code..self.code + 32].copy_from_slice(&self.classbits);
        }
        self.code += 32;
        Flow::Next
    }

    /// One pass of the class `do` body; `None` is its `continue`.
    fn class_item(&mut self, cd: &mut Cd) -> Option<Flow> {
        let mut check_range = false;
        if self.inescq {
            if self.c == b'\\' as u32 && self.pc(cd, 1) == b'E' {
                self.inescq = false;
                self.ptr += 1;
                return None;
            }
            check_range = true;
        }

        if !check_range {
            let p1 = self.pc(cd, 1);
            if self.c == b'[' as u32
                && (p1 == b':' || p1 == b'.' || p1 == b'=')
                && let Some(tempptr) = check_posix_syntax(&cd.pat, self.ptr)
            {
                return self.class_posix(cd, tempptr);
            }

            if self.c == b'\\' as u32 {
                let mut ec = 0;
                self.escape = check_escape(
                    &cd.pat,
                    &mut self.ptr,
                    &mut ec,
                    &mut cd.errorcode,
                    cd.bracount as i32,
                    self.options,
                    true,
                );
                if cd.errorcode != 0 {
                    return Some(Flow::Fail);
                }
                let escape = self.escape;
                if escape == 0 {
                    self.c = ec;
                } else if escape == ESC_LC_B {
                    self.c = 8;
                } else if escape == ESC_N {
                    cd.errorcode = ERR71;
                    return Some(Flow::Fail);
                } else if escape == ESC_Q {
                    if self.pc(cd, 1) == b'\\' && self.pc(cd, 2) == b'E' {
                        self.ptr += 2;
                    } else {
                        self.inescq = true;
                    }
                    return None;
                } else if escape == ESC_E {
                    return None;
                } else {
                    self.class_has_8bitchar += 1;
                    self.class_one_char += 2;
                    let options = self.options;
                    let (cbit, negated) = match escape {
                        ESC_LC_D => (CBIT_DIGIT, false),
                        ESC_D => (CBIT_DIGIT, true),
                        ESC_LC_W => (CBIT_WORD, false),
                        ESC_W => (CBIT_WORD, true),
                        ESC_LC_S => (CBIT_SPACE, false),
                        ESC_S => (CBIT_SPACE, true),
                        ESC_LC_H => {
                            add_list_to_class(
                                &mut self.classbits,
                                options,
                                cd,
                                &HSPACE_LIST_T,
                                NOTACHAR,
                            );
                            return None;
                        }
                        ESC_H => {
                            add_not_list_to_class(&mut self.classbits, options, cd, &HSPACE_LIST_T);
                            return None;
                        }
                        ESC_LC_V => {
                            add_list_to_class(
                                &mut self.classbits,
                                options,
                                cd,
                                &VSPACE_LIST_T,
                                NOTACHAR,
                            );
                            return None;
                        }
                        ESC_V => {
                            add_not_list_to_class(&mut self.classbits, options, cd, &VSPACE_LIST_T);
                            return None;
                        }
                        ESC_LC_P | ESC_P => {
                            cd.errorcode = ERR45;
                            return Some(Flow::Fail);
                        }
                        _ => {
                            if options & PCRE_EXTRA != 0 {
                                cd.errorcode = ERR7;
                                return Some(Flow::Fail);
                            }
                            self.class_has_8bitchar -= 1;
                            self.class_one_char -= 2;
                            self.c = cd.pat[self.ptr] as u32;
                            (usize::MAX, false)
                        }
                    };
                    if cbit != usize::MAX {
                        if negated {
                            self.should_flip_negation = true;
                        }
                        for i in 0..32 {
                            let bits = cd.cbits(i + cbit);
                            self.classbits[i] |= if negated { !bits } else { bits };
                        }
                        return None;
                    }
                }
                self.escape = 0;
            }
        }

        // CHECK_RANGE:
        while self.pc(cd, 1) == b'\\' && self.pc(cd, 2) == b'E' {
            self.inescq = false;
            self.ptr += 2;
        }
        let oldptr = self.ptr;
        let c = self.c;
        if c == b'\r' as u32 || c == b'\n' as u32 {
            cd.external_flags |= PCRE_HASCRORLF;
        }

        let mut single = true;
        if !self.inescq && self.pc(cd, 1) == b'-' {
            self.ptr += 2;
            while self.pc(cd, 0) == b'\\' && self.pc(cd, 1) == b'E' {
                self.ptr += 2;
            }
            while self.pc(cd, 0) == b'\\' && self.pc(cd, 1) == b'Q' {
                self.ptr += 2;
                if self.pc(cd, 0) == b'\\' && self.pc(cd, 1) == b'E' {
                    self.ptr += 2;
                    continue;
                }
                self.inescq = true;
                break;
            }

            if self.pc(cd, 0) == 0 || (!self.inescq && self.pc(cd, 0) == b']') {
                self.ptr = oldptr;
            } else {
                let mut d = self.pc(cd, 0) as u32;
                if !self.inescq {
                    let p1 = self.pc(cd, 1);
                    if d == b'\\' as u32 {
                        let descape = check_escape(
                            &cd.pat,
                            &mut self.ptr,
                            &mut d,
                            &mut cd.errorcode,
                            cd.bracount as i32,
                            self.options,
                            true,
                        );
                        if cd.errorcode != 0 {
                            return Some(Flow::Fail);
                        }
                        if descape != 0 {
                            if descape == ESC_LC_B {
                                d = 8;
                            } else {
                                cd.errorcode = ERR83;
                                return Some(Flow::Fail);
                            }
                        }
                    } else if d == b'[' as u32
                        && (p1 == b':' || p1 == b'.' || p1 == b'=')
                        && check_posix_syntax(&cd.pat, self.ptr).is_some()
                    {
                        cd.errorcode = ERR83;
                        return Some(Flow::Fail);
                    }
                }

                if d < c {
                    cd.errorcode = ERR8;
                    return Some(Flow::Fail);
                }
                if d != c {
                    self.class_one_char = 2;
                    if d == b'\r' as u32 || d == b'\n' as u32 {
                        cd.external_flags |= PCRE_HASCRORLF;
                    }
                    self.class_has_8bitchar +=
                        add_to_class(&mut self.classbits, self.options, cd, c, d);
                    single = false;
                }
            }
        }
        if !single {
            return None;
        }

        // CLASS_SINGLE_CHARACTER:
        if self.class_one_char < 2 {
            self.class_one_char += 1;
        }
        if !self.inescq && self.class_one_char == 1 && self.pc(cd, 1) == b']' {
            self.ptr += 1;
            self.zeroreqchar = self.reqchar;
            self.zeroreqcharflags = self.reqcharflags;
            if self.negate_class {
                if self.firstcharflags == REQ_UNSET {
                    self.firstcharflags = REQ_NONE;
                }
                self.zerofirstchar = self.firstchar;
                self.zerofirstcharflags = self.firstcharflags;
                let op = if self.options & PCRE_CASELESS != 0 {
                    OP_NOTI
                } else {
                    OP_NOT
                };
                self.emit(cd, op);
                self.emit(cd, c as u8);
                return Some(Flow::Next);
            }
            self.mcbuffer = c as u8;
            self.mclength = 1;
            return Some(Flow::Goto(Lbl::OneChar));
        }

        self.class_has_8bitchar += add_to_class(&mut self.classbits, self.options, cd, c, c);
        None
    }

    /// A POSIX `[:name:]` inside a class; `tempptr` is on its terminator.
    fn class_posix(&mut self, cd: &mut Cd, tempptr: usize) -> Option<Flow> {
        if self.pc(cd, 1) != b':' {
            cd.errorcode = ERR31;
            return Some(Flow::Fail);
        }
        self.ptr += 2;
        let mut local_negate = false;
        if self.pc(cd, 0) == b'^' {
            local_negate = true;
            self.should_flip_negation = true;
            self.ptr += 1;
        }
        let mut posix_class = check_posix_name(&cd.pat, self.ptr, tempptr - self.ptr);
        if posix_class < 0 {
            cd.errorcode = ERR30;
            return Some(Flow::Fail);
        }
        if self.options & PCRE_CASELESS != 0 && posix_class <= 2 {
            posix_class = 0;
        }
        let pc = posix_class as usize * 3;
        let mut pbits = [0u8; 32];
        for (i, b) in pbits.iter_mut().enumerate() {
            *b = cd.cbits(POSIX_CLASS_MAPS[pc] as usize + i);
        }
        let taboffset = POSIX_CLASS_MAPS[pc + 1];
        let mut tabopt = POSIX_CLASS_MAPS[pc + 2];
        if taboffset >= 0 {
            for (i, b) in pbits.iter_mut().enumerate() {
                let t = cd.cbits(i + taboffset as usize);
                if tabopt >= 0 {
                    *b |= t;
                } else {
                    *b &= !t;
                }
            }
        }
        if tabopt < 0 {
            tabopt = -tabopt;
        }
        if tabopt == 1 {
            pbits[1] &= !0x3c;
        } else if tabopt == 2 {
            pbits[11] &= 0x7f;
        }
        for (cb, pb) in self.classbits.iter_mut().zip(pbits) {
            *cb |= if local_negate { !pb } else { pb };
        }
        self.ptr = tempptr + 1;
        self.class_has_8bitchar = 1;
        self.class_one_char = 2;
        None
    }

    fn put_inc(&mut self, cd: &mut Cd, d: usize) {
        put(&mut cd.code, self.code, d);
        self.code += LINK_SIZE;
    }

    fn put2_inc(&mut self, cd: &mut Cd, d: i32) {
        put2(&mut cd.code, self.code, d as u32);
        self.code += IMM2_SIZE;
    }

    fn emit_rt(&mut self, cd: &mut Cd, base: u8) {
        self.emit(cd, (base as i32 + self.repeat_type) as u8);
    }

    /// Grow `cd.work` until the forward references between `base` and
    /// `this` fit after `hwm`.
    fn ensure_hwm_room(&mut self, cd: &mut Cd, base: usize, this: usize) -> bool {
        while cd.hwm as isize
            > cd.workspace_size as isize - WORK_SIZE_SAFETY_MARGIN as isize - (this - base) as isize
        {
            cd.errorcode = expand_workspace(cd);
            if cd.errorcode != 0 {
                return false;
            }
        }
        true
    }

    /// `REPEAT:` up to the per-item-type branches.
    fn repeat(&mut self, cd: &mut Cd) -> Flow {
        let Some(previous) = self.previous else {
            cd.errorcode = ERR9;
            return Flow::Fail;
        };
        if self.repeat_min == 0 {
            self.firstchar = self.zerofirstchar;
            self.firstcharflags = self.zerofirstcharflags;
            self.reqchar = self.zeroreqchar;
            self.reqcharflags = self.zeroreqcharflags;
        }
        self.reqvary = if self.repeat_min == self.repeat_max {
            0
        } else {
            REQ_VARY
        };
        self.op_type = 0;
        self.possessive_quantifier = false;
        self.tempcode = previous;

        if self.options & PCRE_EXTENDED != 0 {
            let mut p = self.ptr + 1;
            loop {
                while cd.ctypes(cd.pat[p] as u32) & CTYPE_SPACE != 0 {
                    p += 1;
                }
                if cd.pat[p] != b'#' {
                    break;
                }
                p += 1;
                while cd.pat[p] != 0 {
                    if is_newline_pat(cd, p) {
                        p += cd.nllen as usize;
                        break;
                    }
                    p += 1;
                }
            }
            self.ptr = p - 1;
        }

        if self.pc(cd, 1) == b'(' && self.pc(cd, 2) == b'?' && self.pc(cd, 3) == b'#' {
            self.ptr += 4;
            while cd.pat[self.ptr] != 0 && cd.pat[self.ptr] != b')' {
                self.ptr += 1;
            }
            if cd.pat[self.ptr] == 0 {
                cd.errorcode = ERR18;
                return Flow::Fail;
            }
        }

        if self.pc(cd, 1) == b'+' {
            self.repeat_type = 0;
            self.possessive_quantifier = true;
            self.ptr += 1;
        } else if self.pc(cd, 1) == b'?' {
            self.repeat_type = self.greedy_non_default;
            self.ptr += 1;
        } else {
            self.repeat_type = self.greedy_default;
        }

        if cd.code[previous] == OP_RECURSE {
            cd.code
                .copy_within(previous..previous + 1 + LINK_SIZE, previous + 1 + LINK_SIZE);
            cd.code[previous] = OP_ONCE;
            put(&mut cd.code, previous + 1, 2 + 2 * LINK_SIZE);
            cd.code[previous + 2 + 2 * LINK_SIZE] = OP_KET;
            put(
                &mut cd.code,
                previous + 3 + 2 * LINK_SIZE,
                2 + 2 * LINK_SIZE,
            );
            self.code += 2 + 2 * LINK_SIZE;
            self.length_prevgroup = 3 + 3 * LINK_SIZE as i32;
            if !self.pass1() && cd.hwm >= LINK_SIZE {
                let offset = get(&cd.work, cd.hwm - LINK_SIZE);
                if offset == previous + 1 {
                    put(&mut cd.work, cd.hwm - LINK_SIZE, offset + 1 + LINK_SIZE);
                }
            }
        }

        let op = cd.code[previous];
        if op == OP_CHAR || op == OP_CHARI || op == OP_NOT || op == OP_NOTI {
            self.op_type = match op {
                OP_CHAR => 0,
                OP_CHARI => (OP_STARI - OP_STAR) as i32,
                OP_NOT => (OP_NOTSTAR - OP_STAR) as i32,
                _ => (OP_NOTSTARI - OP_STAR) as i32,
            };
            self.c = cd.code[self.code - 1] as u32;
            if op <= OP_CHARI && self.repeat_min > 1 {
                self.reqchar = self.c;
                self.reqcharflags = self.req_caseopt | cd.req_varyopt;
            }
            return Flow::Goto(Lbl::OutputSingleRepeat);
        } else if op < OP_EODN {
            self.op_type = (OP_TYPESTAR - OP_STAR) as i32;
            self.c = op as u32;
            return Flow::Goto(Lbl::OutputSingleRepeat);
        } else if op == OP_CLASS
            || op == OP_NCLASS
            || op == OP_XCLASS
            || op == OP_REF
            || op == OP_REFI
            || op == OP_DNREF
            || op == OP_DNREFI
        {
            if self.repeat_max == 0 {
                self.code = previous;
                return Flow::Goto(Lbl::EndRepeat);
            }
            if self.repeat_min == 0 && self.repeat_max == -1 {
                self.emit_rt(cd, OP_CRSTAR);
            } else if self.repeat_min == 1 && self.repeat_max == -1 {
                self.emit_rt(cd, OP_CRPLUS);
            } else if self.repeat_min == 0 && self.repeat_max == 1 {
                self.emit_rt(cd, OP_CRQUERY);
            } else {
                self.emit_rt(cd, OP_CRRANGE);
                self.put2_inc(cd, self.repeat_min);
                if self.repeat_max == -1 {
                    self.repeat_max = 0;
                }
                self.put2_inc(cd, self.repeat_max);
            }
        } else if (OP_ASSERT..=OP_COND).contains(&op) {
            if let Some(f) = self.repeat_group(cd, previous) {
                return f;
            }
        } else if op == OP_FAIL {
            return Flow::Goto(Lbl::EndRepeat);
        } else {
            cd.errorcode = ERR11;
            return Flow::Fail;
        }
        self.possessive_tail(cd)
    }

    /// `OUTPUT_SINGLE_REPEAT:`. `OP_PROP`/`OP_NOTPROP` need UCP, so there
    /// are never property bytes to carry.
    fn output_single_repeat(&mut self, cd: &mut Cd) -> Flow {
        let previous = self.previous.expect("set before REPEAT");
        let oldcode = self.code;
        self.code = previous;
        if self.repeat_max == 0 {
            return Flow::Goto(Lbl::EndRepeat);
        }
        self.repeat_type += self.op_type;
        let c = self.c as u8;

        if self.repeat_min == 0 {
            if self.repeat_max == -1 {
                self.emit_rt(cd, OP_STAR);
            } else if self.repeat_max == 1 {
                self.emit_rt(cd, OP_QUERY);
            } else {
                self.emit_rt(cd, OP_UPTO);
                self.put2_inc(cd, self.repeat_max);
            }
        } else if self.repeat_min == 1 {
            if self.repeat_max == -1 {
                self.emit_rt(cd, OP_PLUS);
            } else {
                self.code = oldcode;
                if self.repeat_max == 1 {
                    return Flow::Goto(Lbl::EndRepeat);
                }
                self.emit_rt(cd, OP_UPTO);
                self.put2_inc(cd, self.repeat_max - 1);
            }
        } else {
            self.emit(cd, (OP_EXACT as i32 + self.op_type) as u8);
            self.put2_inc(cd, self.repeat_min);
            if self.repeat_max < 0 {
                self.emit(cd, c);
                self.emit_rt(cd, OP_STAR);
            } else if self.repeat_max != self.repeat_min {
                self.emit(cd, c);
                self.repeat_max -= self.repeat_min;
                if self.repeat_max == 1 {
                    self.emit_rt(cd, OP_QUERY);
                } else {
                    self.emit_rt(cd, OP_UPTO);
                    self.put2_inc(cd, self.repeat_max);
                }
            }
        }
        self.emit(cd, c);
        self.possessive_tail(cd)
    }

    /// The `OP_ASSERT..OP_COND` repeat; `Some` leaves the item.
    fn repeat_group(&mut self, cd: &mut Cd, mut previous: usize) -> Option<Flow> {
        let len = self.code - previous;
        let mut base_hwm_offset = self.item_hwm_offset;
        let mut bralink: Option<usize> = None;
        let mut brazeroptr: Option<usize> = None;
        let rt = self.repeat_type;

        if cd.code[previous] == OP_COND && cd.code[previous + LINK_SIZE + 1] == OP_DEF {
            return Some(Flow::Goto(Lbl::EndRepeat));
        }

        if cd.code[previous] < OP_ONCE {
            if self.repeat_min > 0 {
                return Some(Flow::Goto(Lbl::EndRepeat));
            }
            if self.repeat_max < 0 || self.repeat_max > 1 {
                self.repeat_max = 1;
            }
        }

        if self.repeat_min == 0 {
            if self.repeat_max <= 1 {
                cd.code[self.code] = OP_END;
                adjust_recurse(cd, previous, 1, self.item_hwm_offset);
                cd.code.copy_within(previous..previous + len, previous + 1);
                self.code += 1;
                if self.repeat_max == 0 {
                    cd.code[previous] = OP_SKIPZERO;
                    return Some(Flow::Goto(Lbl::EndRepeat));
                }
                brazeroptr = Some(previous);
                cd.code[previous] = (OP_BRAZERO as i32 + rt) as u8;
                previous += 1;
            } else {
                cd.code[self.code] = OP_END;
                adjust_recurse(cd, previous, 2 + LINK_SIZE as i32, self.item_hwm_offset);
                cd.code
                    .copy_within(previous..previous + len, previous + 2 + LINK_SIZE);
                self.code += 2 + LINK_SIZE;
                cd.code[previous] = (OP_BRAZERO as i32 + rt) as u8;
                cd.code[previous + 1] = OP_BRA;
                previous += 2;
                let offset = bralink.map_or(0, |b| previous - b);
                bralink = Some(previous);
                put(&mut cd.code, previous, offset);
                previous += LINK_SIZE;
            }
            self.repeat_max -= 1;
        } else {
            if self.repeat_min > 1 {
                if self.pass1() {
                    let delta = (self.repeat_min - 1).wrapping_mul(self.length_prevgroup);
                    if (self.repeat_min - 1) as i64 * self.length_prevgroup as i64 > i32::MAX as i64
                        || OFLOW_MAX - **self.lengthptr.as_ref().unwrap() < delta
                    {
                        cd.errorcode = ERR20;
                        return Some(Flow::Fail);
                    }
                    **self.lengthptr.as_mut().unwrap() += delta;
                } else {
                    if self.groupsetfirstchar && self.reqcharflags < 0 {
                        self.reqchar = self.firstchar;
                        self.reqcharflags = self.firstcharflags;
                    }
                    for _ in 1..self.repeat_min {
                        let this_hwm_offset = cd.hwm;
                        cd.code.copy_within(previous..previous + len, self.code);
                        if !self.ensure_hwm_room(cd, base_hwm_offset, this_hwm_offset) {
                            return Some(Flow::Fail);
                        }
                        let mut hc = base_hwm_offset;
                        while hc < this_hwm_offset {
                            let v = get(&cd.work, hc) + len;
                            put(&mut cd.work, cd.hwm, v);
                            cd.hwm += LINK_SIZE;
                            hc += LINK_SIZE;
                        }
                        base_hwm_offset = this_hwm_offset;
                        self.code += len;
                    }
                }
            }
            if self.repeat_max > 0 {
                self.repeat_max -= self.repeat_min;
            }
        }

        if self.repeat_max >= 0 {
            if self.pass1() && self.repeat_max > 0 {
                let per = self.length_prevgroup + 1 + 2 + 2 * LINK_SIZE as i32;
                let delta = self.repeat_max.wrapping_mul(per) - 2 - 2 * LINK_SIZE as i32;
                if self.repeat_max as i64 * per as i64 > i32::MAX as i64
                    || OFLOW_MAX - **self.lengthptr.as_ref().unwrap() < delta
                {
                    cd.errorcode = ERR20;
                    return Some(Flow::Fail);
                }
                **self.lengthptr.as_mut().unwrap() += delta;
            } else {
                for i in (0..self.repeat_max).rev() {
                    let this_hwm_offset = cd.hwm;
                    self.emit(cd, (OP_BRAZERO as i32 + rt) as u8);
                    if i != 0 {
                        self.emit(cd, OP_BRA);
                        let offset = bralink.map_or(0, |b| self.code - b);
                        bralink = Some(self.code);
                        self.put_inc(cd, offset);
                    }
                    cd.code.copy_within(previous..previous + len, self.code);
                    if !self.ensure_hwm_room(cd, base_hwm_offset, this_hwm_offset) {
                        return Some(Flow::Fail);
                    }
                    let extra = if i != 0 { 2 + LINK_SIZE } else { 1 };
                    let mut hc = base_hwm_offset;
                    while hc < this_hwm_offset {
                        let v = get(&cd.work, hc) + len + extra;
                        put(&mut cd.work, cd.hwm, v);
                        cd.hwm += LINK_SIZE;
                        hc += LINK_SIZE;
                    }
                    base_hwm_offset = this_hwm_offset;
                    self.code += len;
                }
            }

            while let Some(bl) = bralink {
                let offset = self.code - bl + 1;
                let bra = self.code - offset;
                let oldlinkoffset = get(&cd.code, bra + 1);
                bralink = if oldlinkoffset == 0 {
                    None
                } else {
                    Some(bl - oldlinkoffset)
                };
                self.emit(cd, OP_KET);
                self.put_inc(cd, offset);
                put(&mut cd.code, bra + 1, offset);
            }
        } else {
            let ketcode = self.code - 1 - LINK_SIZE;
            let bracode = ketcode - get(&cd.code, ketcode + 1);

            if (cd.code[bracode] == OP_ONCE || cd.code[bracode] == OP_ONCE_NC)
                && self.possessive_quantifier
            {
                cd.code[bracode] = OP_BRA;
            }

            if cd.code[bracode] == OP_ONCE || cd.code[bracode] == OP_ONCE_NC {
                cd.code[ketcode] = (OP_KETRMAX as i32 + rt) as u8;
            } else {
                if !self.pass1() {
                    let mut scode = bracode;
                    loop {
                        if could_be_empty_branch(cd, scode, ketcode, None) {
                            cd.code[bracode] += OP_SBRA - OP_BRA;
                            break;
                        }
                        scode += get(&cd.code, scode + 1);
                        if cd.code[scode] != OP_ALT {
                            break;
                        }
                    }
                }

                if cd.code[bracode] == OP_COND
                    && cd.code[bracode + get(&cd.code, bracode + 1)] != OP_ALT
                {
                    cd.code[bracode] = OP_SCOND;
                }

                if self.possessive_quantifier {
                    if cd.code[bracode] == OP_COND || cd.code[bracode] == OP_SCOND {
                        let mut nlen = self.code - bracode;
                        cd.code[self.code] = OP_END;
                        adjust_recurse(cd, bracode, 1 + LINK_SIZE as i32, self.item_hwm_offset);
                        cd.code
                            .copy_within(bracode..bracode + nlen, bracode + 1 + LINK_SIZE);
                        self.code += 1 + LINK_SIZE;
                        nlen += 1 + LINK_SIZE;
                        cd.code[bracode] = if cd.code[bracode] == OP_COND {
                            OP_BRAPOS
                        } else {
                            OP_SBRAPOS
                        };
                        self.emit(cd, OP_KETRPOS);
                        self.put_inc(cd, nlen);
                        put(&mut cd.code, bracode + 1, nlen);
                    } else {
                        cd.code[bracode] += 1;
                        cd.code[ketcode] = OP_KETRPOS;
                    }
                    if let Some(bz) = brazeroptr {
                        cd.code[bz] = OP_BRAPOSZERO;
                    }
                    if self.repeat_min < 2 {
                        self.possessive_quantifier = false;
                    }
                } else {
                    cd.code[ketcode] = (OP_KETRMAX as i32 + rt) as u8;
                }
            }
        }
        None
    }

    /// The possessive handling after every repeat that is not abandoned,
    /// then `END_REPEAT`.
    fn possessive_tail(&mut self, cd: &mut Cd) -> Flow {
        if self.possessive_quantifier {
            let mut tempcode = self.tempcode;
            match cd.code[tempcode] {
                OP_TYPEEXACT => {
                    let t = cd.code[tempcode + 1 + IMM2_SIZE];
                    tempcode += oplen(OP_TYPEEXACT)
                        + if t == OP_PROP || t == OP_NOTPROP {
                            2
                        } else {
                            0
                        };
                }
                op @ (OP_CHAR | OP_CHARI | OP_NOT | OP_NOTI | OP_EXACT | OP_EXACTI
                | OP_NOTEXACT | OP_NOTEXACTI) => tempcode += oplen(op),
                OP_CLASS | OP_NCLASS => tempcode += 1 + 32,
                OP_XCLASS => tempcode += get(&cd.code, tempcode + 1),
                _ => {}
            }

            if self.code > tempcode {
                let mut len = self.code - tempcode;
                let repcode = cd.code[tempcode];
                if repcode < OP_CALLOUT && OPCODE_POSSESSIFY[repcode as usize] > 0 {
                    cd.code[tempcode] = OPCODE_POSSESSIFY[repcode as usize];
                } else {
                    cd.code[self.code] = OP_END;
                    adjust_recurse(cd, tempcode, 1 + LINK_SIZE as i32, self.item_hwm_offset);
                    cd.code
                        .copy_within(tempcode..tempcode + len, tempcode + 1 + LINK_SIZE);
                    self.code += 1 + LINK_SIZE;
                    len += 1 + LINK_SIZE;
                    cd.code[tempcode] = OP_ONCE;
                    self.emit(cd, OP_KET);
                    self.put_inc(cd, len);
                    put(&mut cd.code, tempcode + 1, len);
                }
            }
        }
        Flow::Goto(Lbl::EndRepeat)
    }

    /// `END_REPEAT:`.
    fn end_repeat(&mut self, cd: &mut Cd) -> Flow {
        self.previous = None;
        cd.req_varyopt |= self.reqvary;
        Flow::Next
    }

    /// `*lengthptr += n` where C does it without the overflow check.
    fn len_add_raw(&mut self, n: i32) {
        **self.lengthptr.as_mut().expect("pass 1") += n;
    }

    fn is_word(cd: &Cd, c: u8) -> bool {
        cd.ctypes(c as u32) & CTYPE_WORD != 0
    }

    /// Flag the innermost open capture numbered `recno` as referenced from
    /// inside itself.
    fn flag_open_cap(cd: &mut Cd, recno: i32) {
        if let Some(oc) = cd
            .open_caps
            .iter_mut()
            .rev()
            .find(|oc| oc.number as i32 == recno)
        {
            oc.flag = true;
        }
    }

    /// `case CHAR_LEFT_PARENTHESIS`, up to its `switch` on the character
    /// after `(?`.
    fn paren(&mut self, cd: &mut Cd) -> Flow {
        self.ptr += 1;
        let p1 = self.pc(cd, 1);
        if self.pc(cd, 0) == b'*' && (p1 == b':' || cd.ctypes(p1 as u32) & CTYPE_LETTER != 0) {
            return self.verb(cd);
        }

        self.newoptions = self.options;
        self.skipbytes = 0;
        self.bravalue = OP_CBRA;
        self.item_hwm_offset = cd.hwm;
        self.reset_bracount = false;

        if self.pc(cd, 0) == b'?' {
            self.ptr += 1;
            match self.pc(cd, 0) {
                b'|' | b':' => {
                    if self.pc(cd, 0) == b'|' {
                        self.reset_bracount = true;
                        cd.dupgroups = true;
                    }
                    self.bravalue = OP_BRA;
                    self.ptr += 1;
                }
                b'(' => {
                    if let Some(f) = self.cond(cd) {
                        return f;
                    }
                }
                b'=' => {
                    self.bravalue = OP_ASSERT;
                    cd.assert_depth += 1;
                    self.ptr += 1;
                }
                b'!' => {
                    self.ptr += 1;
                    let p1 = self.pc(cd, 1);
                    if self.pc(cd, 0) == b')'
                        && p1 != b'*'
                        && p1 != b'+'
                        && p1 != b'?'
                        && (p1 != b'{' || !is_counted_repeat(&cd.pat, self.ptr + 2))
                    {
                        self.emit(cd, OP_FAIL);
                        self.previous = None;
                        return Flow::Next;
                    }
                    self.bravalue = OP_ASSERT_NOT;
                    cd.assert_depth += 1;
                }
                b'<' => match self.pc(cd, 1) {
                    b'=' => {
                        self.bravalue = OP_ASSERTBACK;
                        cd.assert_depth += 1;
                        self.ptr += 2;
                    }
                    b'!' => {
                        self.bravalue = OP_ASSERTBACK_NOT;
                        cd.assert_depth += 1;
                        self.ptr += 2;
                    }
                    p1 => {
                        if Self::is_word(cd, p1) {
                            return Flow::Goto(Lbl::DefineName);
                        }
                        self.ptr += 1;
                        cd.errorcode = ERR24;
                        return Flow::Fail;
                    }
                },
                b'>' => {
                    self.bravalue = OP_ONCE;
                    self.ptr += 1;
                }
                b'C' => {
                    self.previous_callout = Some(self.code);
                    self.after_manual_callout = 1;
                    self.emit(cd, OP_CALLOUT);
                    let mut n: u32 = 0;
                    self.ptr += 1;
                    while is_digit(self.pc(cd, 0)) {
                        n = n * 10 + (self.pc(cd, 0) - b'0') as u32;
                        self.ptr += 1;
                        if n > 255 {
                            cd.errorcode = ERR38;
                            return Flow::Fail;
                        }
                    }
                    if self.pc(cd, 0) != b')' {
                        cd.errorcode = ERR39;
                        return Flow::Fail;
                    }
                    self.emit(cd, n as u8);
                    let off = (cd.poff(self.ptr) + 1) as usize;
                    put(&mut cd.code, self.code, off);
                    put(&mut cd.code, self.code + LINK_SIZE, 0);
                    self.code += 2 * LINK_SIZE;
                    self.previous = None;
                    return Flow::Next;
                }
                b'P' => {
                    self.ptr += 1;
                    let ch = self.pc(cd, 0);
                    if ch == b'=' || ch == b'>' {
                        self.is_recurse = ch == b'>';
                        self.terminator = b')' as i32;
                        return Flow::Goto(Lbl::NamedRefOrRecurse);
                    } else if ch != b'<' {
                        cd.errorcode = ERR41;
                        return Flow::Fail;
                    }
                    return Flow::Goto(Lbl::DefineName);
                }
                b'\'' => return Flow::Goto(Lbl::DefineName),
                b'&' => {
                    self.terminator = b')' as i32;
                    self.is_recurse = true;
                    return Flow::Goto(Lbl::NamedRefOrRecurse);
                }
                b'R' => {
                    self.recno = 0;
                    self.ptr += 1;
                    if self.pc(cd, 0) != b')' {
                        cd.errorcode = ERR29;
                        return Flow::Fail;
                    }
                    return Flow::Goto(Lbl::HandleRecursion);
                }
                b'-' | b'+' | b'0'..=b'9' => {
                    self.terminator = b')' as i32;
                    return Flow::Goto(Lbl::HandleNumericalRecursion);
                }
                _ => return Flow::Goto(Lbl::OtherCharAfterQuery),
            }
        } else if self.options & PCRE_NO_AUTO_CAPTURE != 0 {
            self.bravalue = OP_BRA;
        } else {
            return Flow::Goto(Lbl::NumberedGroup);
        }
        self.group(cd)
    }

    /// `(*VERB)` and `(*VERB:NAME)`.
    fn verb(&mut self, cd: &mut Cd) -> Flow {
        let name = self.ptr + 1;
        let mut arglen = 0usize;
        let mut arg = 0;
        self.previous = None;
        self.ptr += 1;
        while cd.ctypes(self.pc(cd, 0) as u32) & CTYPE_LETTER != 0 {
            self.ptr += 1;
        }
        let namelen = self.ptr - name;

        if self.pc(cd, 0) == b':' {
            self.ptr += 1;
            arg = self.ptr;
            while self.pc(cd, 0) != 0 && self.pc(cd, 0) != b')' {
                self.ptr += 1;
            }
            arglen = self.ptr - arg;
            if arglen as u32 > MAX_MARK {
                cd.errorcode = ERR75;
                return Flow::Fail;
            }
        }

        if self.pc(cd, 0) != b')' {
            cd.errorcode = ERR60;
            return Flow::Fail;
        }

        for &(vname, op, op_arg) in VERBS.iter() {
            if namelen != vname.len() || &cd.pat[name..name + namelen] != vname {
                continue;
            }
            let setverb;
            if op == OP_ACCEPT as i32 {
                if arglen != 0 {
                    cd.errorcode = ERR59;
                    return Flow::Fail;
                }
                cd.had_accept = true;
                let numbers: Vec<u32> = cd.open_caps.iter().rev().map(|oc| oc.number).collect();
                for number in numbers {
                    if self.pass1() {
                        self.len_add_raw(1 + IMM2_SIZE as i32);
                    } else {
                        self.emit(cd, OP_CLOSE);
                        self.put2_inc(cd, number as i32);
                    }
                }
                setverb = if cd.assert_depth > 0 {
                    OP_ASSERT_ACCEPT
                } else {
                    OP_ACCEPT
                };
                self.emit(cd, setverb);
                if self.firstcharflags == REQ_UNSET {
                    self.firstcharflags = REQ_NONE;
                }
            } else if arglen == 0 {
                if op < 0 {
                    cd.errorcode = ERR66;
                    return Flow::Fail;
                }
                setverb = op as u8;
                self.emit(cd, setverb);
            } else {
                if op_arg < 0 {
                    cd.errorcode = ERR59;
                    return Flow::Fail;
                }
                setverb = op_arg as u8;
                self.emit(cd, setverb);
                if self.pass1() {
                    self.len_add_raw(arglen as i32);
                    self.emit(cd, 0);
                } else {
                    self.emit(cd, arglen as u8);
                    let (code, pat) = (&mut cd.code, &cd.pat);
                    code[self.code..self.code + arglen].copy_from_slice(&pat[arg..arg + arglen]);
                    self.code += arglen;
                }
                self.emit(cd, 0);
            }

            match setverb {
                OP_THEN | OP_THEN_ARG => cd.external_flags |= PCRE_HASTHEN,
                OP_PRUNE | OP_PRUNE_ARG | OP_SKIP | OP_SKIP_ARG => cd.had_pruneorskip = true,
                _ => {}
            }
            return Flow::Next;
        }

        cd.errorcode = ERR60;
        Flow::Fail
    }

    /// The name-table slot whose name is `pat[name..name + namelen]`, as
    /// `(index, offset)`, or `(names_found, end)` when there is none.
    fn find_name_slot(cd: &Cd, name: usize, namelen: usize) -> (i32, usize) {
        let mut slot = 0;
        for i in 0..cd.names_found {
            if name_matches(cd, name, namelen, slot) {
                return (i, slot);
            }
            slot += cd.name_entry_size as usize;
        }
        (cd.names_found, slot)
    }

    /// `(?(` conditions; `None` is the `break` to the group compile.
    fn cond(&mut self, cd: &mut Cd) -> Option<Flow> {
        self.bravalue = OP_COND;
        let mut tempptr = self.ptr;

        if self.pc(cd, 1) == b'?' && self.pc(cd, 2) == b'C' {
            let mut i = 3;
            while is_digit(self.pc(cd, i)) {
                i += 1;
            }
            if self.pc(cd, i) == b')' {
                tempptr += i + 1;
            }
            if cd.pat[tempptr] != b'(' {
                cd.errorcode = ERR28;
                return Some(Flow::Fail);
            }
        }

        let t = &cd.pat[tempptr..tempptr + 4];
        if t[1] == b'?'
            && (t[2] == b'=' || t[2] == b'!' || (t[2] == b'<' && (t[3] == b'=' || t[3] == b'!')))
        {
            cd.iscondassert = true;
            return None;
        }

        let code = self.code;
        cd.code[code + 1 + LINK_SIZE] = OP_CREF;
        self.skipbytes = 1 + IMM2_SIZE as i32;
        self.refsign = -1;
        let mut namelen: usize = 0;
        let mut name = 0;
        self.recno = 0;

        self.ptr += 1;
        if self.pc(cd, 0) == b'R' && self.pc(cd, 1) == b'&' {
            self.terminator = -1;
            self.ptr += 2;
            cd.code[code + 1 + LINK_SIZE] = OP_RREF;
        } else if self.pc(cd, 0) == b'<' {
            self.terminator = b'>' as i32;
            self.ptr += 1;
        } else if self.pc(cd, 0) == b'\'' {
            self.terminator = b'\'' as i32;
            self.ptr += 1;
        } else {
            self.terminator = 0;
            if self.pc(cd, 0) == b'-' || self.pc(cd, 0) == b'+' {
                self.refsign = self.pc(cd, 0) as i32;
                self.ptr += 1;
            } else if is_digit(self.pc(cd, 0)) {
                self.refsign = 0;
            }
        }

        if self.refsign >= 0 {
            while is_digit(self.pc(cd, 0)) {
                if self.recno > i32::MAX / 10 - 1 {
                    while is_digit(self.pc(cd, 0)) {
                        self.ptr += 1;
                    }
                    cd.errorcode = ERR61;
                    return Some(Flow::Fail);
                }
                self.recno = self.recno * 10 + (self.pc(cd, 0) - b'0') as i32;
                self.ptr += 1;
            }
        } else {
            if is_digit(self.pc(cd, 0)) {
                cd.errorcode = ERR84;
                return Some(Flow::Fail);
            }
            if !Self::is_word(cd, self.pc(cd, 0)) {
                cd.errorcode = ERR28;
                return Some(Flow::Fail);
            }
            name = self.ptr;
            self.ptr += 1;
            while Self::is_word(cd, self.pc(cd, 0)) {
                self.ptr += 1;
            }
            namelen = self.ptr - name;
            if self.pass1() {
                self.skipbytes += IMM2_SIZE as i32;
            }
        }

        let bad_terminator = self.terminator > 0 && {
            let t = self.pc(cd, 0);
            self.ptr += 1;
            t != self.terminator as u8
        };
        if bad_terminator || {
            let t = self.pc(cd, 0);
            self.ptr += 1;
            t != b')'
        } {
            self.ptr -= 1;
            cd.errorcode = ERR26;
            return Some(Flow::Fail);
        }

        if self.pass1() {
            return None;
        }

        if self.refsign >= 0 {
            if self.recno <= 0 {
                cd.errorcode = ERR35;
                return Some(Flow::Fail);
            }
            if self.refsign != 0 {
                self.recno = if self.refsign == b'-' as i32 {
                    cd.bracount as i32 - self.recno + 1
                } else {
                    self.recno + cd.bracount as i32
                };
            }
            if self.recno <= 0 || self.recno > cd.final_bracount {
                cd.errorcode = ERR15;
                return Some(Flow::Fail);
            }
            put2(&mut cd.code, code + 2 + LINK_SIZE, self.recno as u32);
            if self.recno > cd.top_backref {
                cd.top_backref = self.recno;
            }
            return None;
        }

        let (mut i, mut slot) = Self::find_name_slot(cd, name, namelen);
        if i < cd.names_found {
            let offset = i;
            i += 1;
            let mut count = 1;
            self.recno = get2(&cd.name_table, slot) as i32;
            if self.recno > cd.top_backref {
                cd.top_backref = self.recno;
            }
            while i < cd.names_found {
                slot += cd.name_entry_size as usize;
                if !name_matches(cd, name, namelen, slot) {
                    break;
                }
                count += 1;
                i += 1;
            }
            if count > 1 {
                put2(&mut cd.code, code + 2 + LINK_SIZE, offset as u32);
                put2(&mut cd.code, code + 2 + LINK_SIZE + IMM2_SIZE, count);
                self.skipbytes += IMM2_SIZE as i32;
                cd.code[code + 1 + LINK_SIZE] += 1;
            } else {
                put2(&mut cd.code, code + 2 + LINK_SIZE, self.recno as u32);
            }
        } else if self.terminator != 0 {
            cd.errorcode = ERR15;
            return Some(Flow::Fail);
        } else if cd.pat[name] == b'R' {
            self.recno = 0;
            for i in 1..namelen {
                let d = cd.pat[name + i];
                if !is_digit(d) {
                    cd.errorcode = ERR15;
                    return Some(Flow::Fail);
                }
                if self.recno > i32::MAX / 10 - 1 {
                    cd.errorcode = ERR61;
                    return Some(Flow::Fail);
                }
                self.recno = self.recno * 10 + (d - b'0') as i32;
            }
            if self.recno == 0 {
                self.recno = RREF_ANY as i32;
            }
            cd.code[code + 1 + LINK_SIZE] = OP_RREF;
            put2(&mut cd.code, code + 2 + LINK_SIZE, self.recno as u32);
        } else if namelen == 6 && cd.starts(name, b"DEFINE") {
            cd.code[code + 1 + LINK_SIZE] = OP_DEF;
            self.skipbytes = 1;
        } else {
            cd.errorcode = ERR15;
            return Some(Flow::Fail);
        }
        None
    }

    /// `DEFINE_NAME:`, `(?<name>`, `(?'name'` and `(?P<name>`.
    fn define_name(&mut self, cd: &mut Cd) -> Flow {
        self.terminator = if self.pc(cd, 0) == b'<' {
            b'>' as i32
        } else {
            b'\'' as i32
        };
        self.ptr += 1;
        let name = self.ptr;
        if is_digit(self.pc(cd, 0)) {
            cd.errorcode = ERR84;
            return Flow::Fail;
        }
        while Self::is_word(cd, self.pc(cd, 0)) {
            self.ptr += 1;
        }
        let namelen = (self.ptr - name) as i32;

        if self.pass1() {
            let number = cd.bracount + 1;
            if self.pc(cd, 0) != self.terminator as u8 {
                cd.errorcode = ERR42;
                return Flow::Fail;
            }
            if cd.names_found >= MAX_NAME_COUNT {
                cd.errorcode = ERR49;
                return Flow::Fail;
            }
            if namelen + IMM2_SIZE as i32 + 1 > cd.name_entry_size {
                cd.name_entry_size = namelen + IMM2_SIZE as i32 + 1;
                if namelen > MAX_NAME_SIZE {
                    cd.errorcode = ERR48;
                    return Flow::Fail;
                }
            }

            let mut i = 0;
            while i < cd.names_found as usize {
                let ng = cd.named_groups[i];
                if namelen == ng.length
                    && cd.pat[name..name + namelen as usize]
                        == cd.pat[ng.name..ng.name + namelen as usize]
                {
                    if ng.number == number {
                        break;
                    }
                    if self.options & PCRE_DUPNAMES == 0 {
                        cd.errorcode = ERR43;
                        return Flow::Fail;
                    }
                    cd.dupnames = true;
                } else if ng.number == number {
                    cd.errorcode = ERR65;
                    return Flow::Fail;
                }
                i += 1;
            }
            if i >= cd.names_found as usize {
                cd.named_groups.push(NamedGroup {
                    name,
                    length: namelen,
                    number,
                });
                cd.names_found += 1;
            }
        }

        self.ptr += 1;
        Flow::Goto(Lbl::NumberedGroup)
    }

    /// `NAMED_REF_OR_RECURSE:`; `ptr` is on the character before the name.
    fn named_ref_or_recurse(&mut self, cd: &mut Cd) -> Flow {
        self.ptr += 1;
        let name = self.ptr;
        if is_digit(self.pc(cd, 0)) {
            cd.errorcode = ERR84;
            return Flow::Fail;
        }
        while Self::is_word(cd, self.pc(cd, 0)) {
            self.ptr += 1;
        }
        let namelen = self.ptr - name;
        let mut i = 0;
        let mut slot = 0;

        if self.pass1() {
            self.recno = 0;
            if namelen == 0 {
                cd.errorcode = ERR62;
                return Flow::Fail;
            }
            if self.pc(cd, 0) != self.terminator as u8 {
                cd.errorcode = ERR42;
                return Flow::Fail;
            }
            if namelen as i32 > MAX_NAME_SIZE {
                cd.errorcode = ERR48;
                return Flow::Fail;
            }
            if !self.is_recurse {
                cd.namedrefcount += 1;
            }
            self.len_add_raw(IMM2_SIZE as i32);
            self.len_add_raw(4 + 4 * LINK_SIZE as i32);
            if cd.dupgroups {
                self.len_add_raw(4 + 4 * LINK_SIZE as i32);
            }
            for k in 0..cd.named_groups.len() {
                let ng = cd.named_groups[k];
                if namelen as i32 == ng.length
                    && cd.pat[name..name + namelen] == cd.pat[ng.name..ng.name + namelen]
                {
                    self.recno = ng.number as i32;
                    if self.is_recurse {
                        break;
                    }
                    Self::flag_open_cap(cd, self.recno);
                }
            }
        } else {
            (i, slot) = Self::find_name_slot(cd, name, namelen);
            if i < cd.names_found {
                self.recno = get2(&cd.name_table, slot) as i32;
            } else {
                cd.errorcode = ERR15;
                return Flow::Fail;
            }
        }

        if self.is_recurse {
            return Flow::Goto(Lbl::HandleRecursion);
        }

        if !self.pass1() && cd.dupnames {
            let es = cd.name_entry_size as usize;
            let mut count = 1;
            let index = i;
            let mut cslot = slot + es;
            i += 1;
            while i < cd.names_found {
                if !cstr_eq(&cd.name_table, slot + IMM2_SIZE, cslot + IMM2_SIZE) {
                    break;
                }
                count += 1;
                cslot += es;
                i += 1;
            }
            if count > 1 {
                if self.firstcharflags == REQ_UNSET {
                    self.firstcharflags = REQ_NONE;
                }
                self.previous = Some(self.code);
                self.item_hwm_offset = cd.hwm;
                let op = if self.options & PCRE_CASELESS != 0 {
                    OP_DNREFI
                } else {
                    OP_DNREF
                };
                self.emit(cd, op);
                self.put2_inc(cd, index);
                self.put2_inc(cd, count);
                while slot < cslot {
                    self.recno = get2(&cd.name_table, slot) as i32;
                    self.note_backref(cd);
                    slot += es;
                }
                return Flow::Next;
            }
        }
        Flow::Goto(Lbl::HandleReference)
    }

    /// The `backref_map`, `top_backref` and open-capture updates for a
    /// reference to `recno`.
    fn note_backref(&mut self, cd: &mut Cd) {
        let recno = self.recno;
        cd.backref_map |= if recno < 32 { 1u32 << recno } else { 1 };
        if recno > cd.top_backref {
            cd.top_backref = recno;
        }
        Self::flag_open_cap(cd, recno);
    }

    /// `HANDLE_REFERENCE:`.
    fn handle_reference(&mut self, cd: &mut Cd) -> Flow {
        if self.firstcharflags == REQ_UNSET {
            self.firstcharflags = REQ_NONE;
            self.zerofirstcharflags = REQ_NONE;
        }
        self.previous = Some(self.code);
        self.item_hwm_offset = cd.hwm;
        let op = if self.options & PCRE_CASELESS != 0 {
            OP_REFI
        } else {
            OP_REF
        };
        self.emit(cd, op);
        self.put2_inc(cd, self.recno);
        self.note_backref(cd);
        Flow::Next
    }

    /// `HANDLE_NUMERICAL_RECURSION:`; `ptr` is on the sign or first digit.
    fn handle_numerical_recursion(&mut self, cd: &mut Cd) -> Flow {
        self.refsign = self.pc(cd, 0) as i32;
        if self.refsign == b'+' as i32 {
            self.ptr += 1;
            if !is_digit(self.pc(cd, 0)) {
                cd.errorcode = ERR63;
                return Flow::Fail;
            }
        } else if self.refsign == b'-' as i32 {
            if !is_digit(self.pc(cd, 1)) {
                return Flow::Goto(Lbl::OtherCharAfterQuery);
            }
            self.ptr += 1;
        }

        self.recno = 0;
        while is_digit(self.pc(cd, 0)) {
            if self.recno > i32::MAX / 10 - 1 {
                while is_digit(self.pc(cd, 0)) {
                    self.ptr += 1;
                }
                cd.errorcode = ERR61;
                return Flow::Fail;
            }
            self.recno = self.recno * 10 + (self.pc(cd, 0) - b'0') as i32;
            self.ptr += 1;
        }

        if self.pc(cd, 0) != self.terminator as u8 {
            cd.errorcode = ERR29;
            return Flow::Fail;
        }

        if self.refsign == b'-' as i32 {
            if self.recno == 0 {
                cd.errorcode = ERR58;
                return Flow::Fail;
            }
            self.recno = cd.bracount as i32 - self.recno + 1;
            if self.recno <= 0 {
                cd.errorcode = ERR15;
                return Flow::Fail;
            }
        } else if self.refsign == b'+' as i32 {
            if self.recno == 0 {
                cd.errorcode = ERR58;
                return Flow::Fail;
            }
            self.recno += cd.bracount as i32;
        }
        Flow::Goto(Lbl::HandleRecursion)
    }

    /// `HANDLE_RECURSION:` and the first-char tail after it.
    fn handle_recursion(&mut self, cd: &mut Cd) -> Flow {
        let code = self.code;
        self.previous = Some(code);
        self.item_hwm_offset = cd.hwm;
        let mut called = 0;

        if !self.pass1() {
            cd.code[code] = OP_END;
            let found = if self.recno != 0 {
                find_bracket(&cd.code, 0, self.recno)
            } else {
                Some(0)
            };
            match found {
                None => {
                    if self.recno > cd.final_bracount {
                        cd.errorcode = ERR15;
                        return Flow::Fail;
                    }
                    called = self.recno as usize;
                    if cd.hwm >= cd.workspace_size - WORK_SIZE_SAFETY_MARGIN {
                        cd.errorcode = expand_workspace(cd);
                        if cd.errorcode != 0 {
                            return Flow::Fail;
                        }
                    }
                    put(&mut cd.work, cd.hwm, code + 1);
                    cd.hwm += LINK_SIZE;
                }
                Some(cl) => {
                    if get(&cd.code, cl + 1) == 0
                        && self.cond_depth <= 0
                        && could_be_empty(cd, cl, code, self.bcptr)
                    {
                        cd.errorcode = ERR40;
                        return Flow::Fail;
                    }
                    called = cl;
                }
            }
        }

        cd.code[code] = OP_RECURSE;
        put(&mut cd.code, code + 1, called);
        self.code += 1 + LINK_SIZE;
        self.groupsetfirstchar = false;

        if self.firstcharflags == REQ_UNSET {
            self.firstcharflags = REQ_NONE;
        }
        self.zerofirstchar = self.firstchar;
        self.zerofirstcharflags = self.firstcharflags;
        Flow::Next
    }

    /// `OTHER_CHAR_AFTER_QUERY:`, option settings.
    fn other_char_after_query(&mut self, cd: &mut Cd) -> Flow {
        let mut set = 0;
        let mut unset = 0;
        let mut to_unset = false;
        while self.pc(cd, 0) != b')' && self.pc(cd, 0) != b':' {
            let ch = self.pc(cd, 0);
            self.ptr += 1;
            let bit = match ch {
                b'-' => {
                    to_unset = true;
                    continue;
                }
                b'J' => {
                    cd.external_flags |= PCRE_JCHANGED;
                    PCRE_DUPNAMES
                }
                b'i' => PCRE_CASELESS,
                b'm' => PCRE_MULTILINE,
                b's' => PCRE_DOTALL,
                b'x' => PCRE_EXTENDED,
                b'U' => PCRE_UNGREEDY,
                b'X' => PCRE_EXTRA,
                _ => {
                    cd.errorcode = ERR12;
                    self.ptr -= 1;
                    return Flow::Fail;
                }
            };
            if to_unset {
                unset |= bit;
            } else {
                set |= bit;
            }
        }

        self.newoptions = (self.options | set) & !unset;

        if self.pc(cd, 0) == b')' {
            self.greedy_default = (self.newoptions & PCRE_UNGREEDY != 0) as i32;
            self.greedy_non_default = self.greedy_default ^ 1;
            self.req_caseopt = if self.newoptions & PCRE_CASELESS != 0 {
                REQ_CASELESS
            } else {
                0
            };
            self.options = self.newoptions;
            self.optionsptr = self.newoptions;
            self.previous = None;
            return Flow::Next;
        }

        self.bravalue = OP_BRA;
        self.ptr += 1;
        self.group(cd)
    }

    /// `NUMBERED_GROUP:`.
    fn numbered_group(&mut self, cd: &mut Cd) -> Flow {
        cd.bracount += 1;
        put2(&mut cd.code, self.code + 1 + LINK_SIZE, cd.bracount);
        self.skipbytes = IMM2_SIZE as i32;
        self.group(cd)
    }

    /// The nested group compile that ends `case CHAR_LEFT_PARENTHESIS`.
    fn group(&mut self, cd: &mut Cd) -> Flow {
        cd.parens_depth += 1;
        if cd.parens_depth > PARENS_NEST_LIMIT {
            cd.errorcode = ERR82;
            return Flow::Fail;
        }

        let code = self.code;
        if (OP_ASSERT..=OP_ASSERTBACK_NOT).contains(&self.bravalue) && cd.iscondassert {
            self.previous = None;
            cd.iscondassert = false;
        } else {
            self.previous = Some(code);
            self.item_hwm_offset = cd.hwm;
        }

        cd.code[code] = self.bravalue;
        self.tempcode = code;
        self.tempreqvary = cd.req_varyopt;
        self.tempbracount = cd.bracount;
        self.length_prevgroup = 0;

        let mut io = BranchIo {
            options: self.newoptions,
            code,
            ptr: self.ptr,
            ..BranchIo::default()
        };
        let g = GroupArgs {
            lookbehind: self.bravalue == OP_ASSERTBACK || self.bravalue == OP_ASSERTBACK_NOT,
            reset_bracount: self.reset_bracount,
            skipbytes: self.skipbytes,
            cond_depth: self.cond_depth + (self.bravalue == OP_COND) as i32,
        };
        let ok = if self.pass1() {
            compile_regex(cd, &mut io, g, self.bcptr, Some(&mut self.length_prevgroup))
        } else {
            compile_regex(cd, &mut io, g, self.bcptr, None)
        };
        self.ptr = io.ptr;
        if !ok {
            return Flow::Fail;
        }
        self.tempcode = io.code;
        self.subfirstchar = io.firstchar;
        self.subfirstcharflags = io.firstcharflags;
        self.subreqchar = io.reqchar;
        self.subreqcharflags = io.reqcharflags;

        cd.parens_depth -= 1;

        if self.bravalue == OP_ONCE && cd.bracount <= self.tempbracount {
            cd.code[code] = OP_ONCE_NC;
        }

        if (OP_ASSERT..=OP_ASSERTBACK_NOT).contains(&self.bravalue) {
            cd.assert_depth -= 1;
        }

        if self.bravalue == OP_COND && !self.pass1() {
            let mut tc = code;
            let mut condcount = 0;
            loop {
                condcount += 1;
                tc += get(&cd.code, tc + 1);
                if cd.code[tc] == OP_KET {
                    break;
                }
            }
            if cd.code[code + LINK_SIZE + 1] == OP_DEF {
                if condcount > 1 {
                    cd.errorcode = ERR54;
                    return Flow::Fail;
                }
                self.bravalue = OP_DEF;
            } else {
                if condcount > 2 {
                    cd.errorcode = ERR27;
                    return Flow::Fail;
                }
                if condcount == 1 {
                    self.subfirstcharflags = REQ_NONE;
                    self.subreqcharflags = REQ_NONE;
                }
            }
        }

        if self.pc(cd, 0) != b')' {
            cd.errorcode = ERR14;
            return Flow::Fail;
        }

        if self.pass1() {
            if !self.add_length(cd, self.length_prevgroup - 2 - 2 * LINK_SIZE as i32) {
                return Flow::Fail;
            }
            self.code += 1;
            self.put_inc(cd, 1 + LINK_SIZE);
            self.emit(cd, OP_KET);
            self.put_inc(cd, 1 + LINK_SIZE);
            return Flow::Next;
        }

        self.code = self.tempcode;

        if self.bravalue == OP_DEF {
            return Flow::Next;
        }

        self.zeroreqchar = self.reqchar;
        self.zeroreqcharflags = self.reqcharflags;
        self.zerofirstchar = self.firstchar;
        self.zerofirstcharflags = self.firstcharflags;
        self.groupsetfirstchar = false;

        if self.bravalue >= OP_ONCE {
            if self.firstcharflags == REQ_UNSET {
                if self.subfirstcharflags >= 0 {
                    self.firstchar = self.subfirstchar;
                    self.firstcharflags = self.subfirstcharflags;
                    self.groupsetfirstchar = true;
                } else {
                    self.firstcharflags = REQ_NONE;
                }
                self.zerofirstcharflags = REQ_NONE;
            } else if self.subfirstcharflags >= 0 && self.subreqcharflags < 0 {
                self.subreqchar = self.subfirstchar;
                self.subreqcharflags = self.subfirstcharflags | self.tempreqvary;
            }

            if self.subreqcharflags >= 0 {
                self.reqchar = self.subreqchar;
                self.reqcharflags = self.subreqcharflags;
            }
        } else if self.bravalue == OP_ASSERT
            && self.subreqcharflags >= 0
            && self.subfirstcharflags >= 0
        {
            self.reqchar = self.subreqchar;
            self.reqcharflags = self.subreqcharflags;
        }
        Flow::Next
    }

    /// `case CHAR_BACKSLASH`.
    fn backslash(&mut self, cd: &mut Cd) -> Flow {
        let mut ec = 0;
        self.escape = check_escape(
            &cd.pat,
            &mut self.ptr,
            &mut ec,
            &mut cd.errorcode,
            cd.bracount as i32,
            self.options,
            false,
        );
        if cd.errorcode != 0 {
            return Flow::Fail;
        }
        let escape = self.escape;

        if escape == 0 {
            self.c = ec;
            self.mcbuffer = ec as u8;
            self.mclength = 1;
            return Flow::Goto(Lbl::OneChar);
        }

        if self.firstcharflags == REQ_UNSET && escape > ESC_LC_B && escape < ESC_Z {
            self.firstcharflags = REQ_NONE;
        }
        self.zerofirstchar = self.firstchar;
        self.zerofirstcharflags = self.firstcharflags;
        self.zeroreqchar = self.reqchar;
        self.zeroreqcharflags = self.reqcharflags;

        if escape == ESC_LC_G {
            self.item_hwm_offset = cd.hwm;
            self.ptr += 1;
            self.terminator = if self.pc(cd, 0) == b'<' {
                b'>' as i32
            } else {
                b'\'' as i32
            };
            self.skipbytes = 0;
            self.reset_bracount = false;
            let cf = self.pc(cd, 1);
            if cf != b'+' && cf != b'-' && !is_digit(cf) {
                self.is_recurse = true;
                return Flow::Goto(Lbl::NamedRefOrRecurse);
            }
            let mut p = self.ptr + 2;
            while is_digit(cd.pat[p]) {
                p += 1;
            }
            if cd.pat[p] != self.terminator as u8 {
                cd.errorcode = ERR57;
                return Flow::Fail;
            }
            self.ptr += 1;
            return Flow::Goto(Lbl::HandleNumericalRecursion);
        }

        if escape == ESC_LC_K {
            let p1 = self.pc(cd, 1);
            if p1 != b'<' && p1 != b'\'' && p1 != b'{' {
                cd.errorcode = ERR69;
                return Flow::Fail;
            }
            self.is_recurse = false;
            self.ptr += 1;
            self.terminator = match self.pc(cd, 0) {
                b'<' => b'>' as i32,
                b'\'' => b'\'' as i32,
                _ => b'}' as i32,
            };
            return Flow::Goto(Lbl::NamedRefOrRecurse);
        }

        if escape < 0 {
            self.recno = -escape;
            return Flow::Goto(Lbl::HandleReference);
        } else if escape == ESC_X || escape == ESC_P || escape == ESC_LC_P {
            cd.errorcode = ERR45;
            return Flow::Fail;
        } else {
            if (escape == ESC_LC_B || escape == ESC_B || escape == ESC_A) && cd.max_lookbehind == 0
            {
                cd.max_lookbehind = 1;
            }
            self.previous = if escape > ESC_LC_B && escape < ESC_Z {
                Some(self.code)
            } else {
                None
            };
            self.item_hwm_offset = cd.hwm;
            let op = if escape == ESC_C {
                OP_ALLANY
            } else {
                escape as u8
            };
            self.emit(cd, op);
        }
        Flow::Next
    }

    /// `NORMAL_CHAR:`.
    fn normal_char(&mut self, cd: &mut Cd) -> Flow {
        self.mclength = 1;
        self.mcbuffer = self.c as u8;
        self.one_char(cd)
    }

    /// `ONE_CHAR:`; `mclength` is always 1 without UTF.
    fn one_char(&mut self, cd: &mut Cd) -> Flow {
        self.previous = Some(self.code);
        self.item_hwm_offset = cd.hwm;
        let op = if self.options & PCRE_CASELESS != 0 {
            OP_CHARI
        } else {
            OP_CHAR
        };
        self.emit(cd, op);
        self.emit(cd, self.mcbuffer);

        if self.mcbuffer == b'\r' || self.mcbuffer == b'\n' {
            cd.external_flags |= PCRE_HASCRORLF;
        }

        if self.firstcharflags == REQ_UNSET {
            self.zerofirstcharflags = REQ_NONE;
            self.zeroreqchar = self.reqchar;
            self.zeroreqcharflags = self.reqcharflags;
            self.firstchar = self.mcbuffer as u32;
            self.firstcharflags = self.req_caseopt;
        } else {
            self.zerofirstchar = self.firstchar;
            self.zerofirstcharflags = self.firstcharflags;
            self.zeroreqchar = self.reqchar;
            self.zeroreqcharflags = self.reqcharflags;
            self.reqchar = cd.code[self.code - 1] as u32;
            self.reqcharflags = self.req_caseopt | cd.req_varyopt;
        }
        Flow::Next
    }
}

/// `STRNCMP_UC_UC(name, slot + IMM2_SIZE, namelen) == 0 &&
/// slot[IMM2_SIZE + namelen] == 0`, reading the table as C's `strncmp`
/// does, never past a mismatch.
fn name_matches(cd: &Cd, name: usize, namelen: usize, slot: usize) -> bool {
    let t = &cd.name_table;
    let at = |i: usize| t.get(i).copied().unwrap_or(0);
    for k in 0..namelen {
        let a = cd.pat[name + k];
        let b = at(slot + IMM2_SIZE + k);
        if a != b {
            return false;
        }
    }
    at(slot + IMM2_SIZE + namelen) == 0
}

/// `STRCMP_UC_UC` on two NUL-terminated names in `t`.
fn cstr_eq(t: &[u8], mut a: usize, mut b: usize) -> bool {
    loop {
        let (x, y) = (t[a], t[b]);
        if x != y {
            return false;
        }
        if x == 0 {
            return true;
        }
        a += 1;
        b += 1;
    }
}

// ---- compile_regex ----

/// `compile_regex`'s by-value arguments.
#[derive(Clone, Copy)]
struct GroupArgs {
    lookbehind: bool,
    reset_bracount: bool,
    skipbytes: i32,
    cond_depth: i32,
}

/// `compile_regex`: `io.code` is on the bracket opcode, `io.ptr` just past
/// the opening parenthesis. `io.options` is an input only.
fn compile_regex(
    cd: &mut Cd,
    io: &mut BranchIo,
    g: GroupArgs,
    bcptr: Option<&BranchChain<'_>>,
    mut lengthptr: Option<&mut i32>,
) -> bool {
    let pass1 = lengthptr.is_some();
    let mut options = io.options;
    let mut ptr = io.ptr;
    let mut code = io.code;
    let mut last_branch = code;
    let start_bracket = code;
    let mut reverse_count = 0;
    let mut capnumber = 0;
    let (mut firstchar, mut reqchar) = (0u32, 0u32);
    let (mut firstcharflags, mut reqcharflags) = (REQ_UNSET, REQ_UNSET);
    let bc = BranchChain {
        outer: bcptr,
        current_branch: Cell::new(code),
    };
    let save_hwm_offset = cd.hwm;
    let skipbytes = g.skipbytes as usize;

    let mut length = 2 + 2 * LINK_SIZE as i32 + g.skipbytes;

    if cd.code[code] == OP_CBRA {
        capnumber = get2(&cd.code, code + 1 + LINK_SIZE);
        cd.open_caps.push(OpenCap {
            number: capnumber,
            flag: false,
        });
    }

    put(&mut cd.code, code + 1, 0);
    code += 1 + LINK_SIZE + skipbytes;

    let orig_bracount = cd.bracount;
    let mut max_bracount = orig_bracount;

    loop {
        if g.reset_bracount {
            cd.bracount = orig_bracount;
        }

        if g.lookbehind {
            cd.code[code] = OP_REVERSE;
            code += 1;
            reverse_count = code;
            put(&mut cd.code, code, 0);
            code += LINK_SIZE;
            length += 1 + LINK_SIZE as i32;
        }

        let mut bio = BranchIo {
            options,
            code,
            ptr,
            ..BranchIo::default()
        };
        let ok = compile_branch(
            cd,
            &mut bio,
            Some(&bc),
            g.cond_depth,
            if pass1 { Some(&mut length) } else { None },
        );
        options = bio.options;
        ptr = bio.ptr;
        if !ok {
            io.ptr = ptr;
            return false;
        }
        code = bio.code;
        let (branchfirstchar, branchfirstcharflags) = (bio.firstchar, bio.firstcharflags);
        let (mut branchreqchar, mut branchreqcharflags) = (bio.reqchar, bio.reqcharflags);

        if cd.bracount > max_bracount {
            max_bracount = cd.bracount;
        }

        if !pass1 {
            if cd.code[last_branch] != OP_ALT {
                firstchar = branchfirstchar;
                firstcharflags = branchfirstcharflags;
                reqchar = branchreqchar;
                reqcharflags = branchreqcharflags;
            } else {
                if firstcharflags >= 0
                    && (firstcharflags != branchfirstcharflags || firstchar != branchfirstchar)
                {
                    if reqcharflags < 0 {
                        reqchar = firstchar;
                        reqcharflags = firstcharflags;
                    }
                    firstcharflags = REQ_NONE;
                }

                if firstcharflags < 0 && branchfirstcharflags >= 0 && branchreqcharflags < 0 {
                    branchreqchar = branchfirstchar;
                    branchreqcharflags = branchfirstcharflags;
                }

                if (reqcharflags & !REQ_VARY) != (branchreqcharflags & !REQ_VARY)
                    || reqchar != branchreqchar
                {
                    reqcharflags = REQ_NONE;
                } else {
                    reqchar = branchreqchar;
                    reqcharflags |= branchreqcharflags;
                }
            }

            if g.lookbehind {
                cd.code[code] = OP_END;
                let fixed_length = find_fixedlength(&cd.code, last_branch, false, None);
                if fixed_length == -3 {
                    cd.check_lookbehind = true;
                } else if fixed_length < 0 {
                    cd.errorcode = match fixed_length {
                        -2 => ERR36,
                        -4 => ERR70,
                        _ => ERR25,
                    };
                    io.ptr = ptr;
                    return false;
                } else {
                    if fixed_length > cd.max_lookbehind {
                        cd.max_lookbehind = fixed_length;
                    }
                    put(&mut cd.code, reverse_count, fixed_length as usize);
                }
            }
        }

        if cd.pat[ptr] != b'|' {
            if !pass1 {
                let mut branch_length = code - last_branch;
                loop {
                    let prev_length = get(&cd.code, last_branch + 1);
                    put(&mut cd.code, last_branch + 1, branch_length);
                    branch_length = prev_length;
                    last_branch -= branch_length;
                    if branch_length == 0 {
                        break;
                    }
                }
            }

            cd.code[code] = OP_KET;
            put(&mut cd.code, code + 1, code - start_bracket);
            code += 1 + LINK_SIZE;

            if capnumber > 0 {
                if cd.open_caps.last().expect("pushed above").flag {
                    cd.code[code] = OP_END;
                    adjust_recurse(cd, start_bracket, 1 + LINK_SIZE as i32, save_hwm_offset);
                    cd.code
                        .copy_within(start_bracket..code, start_bracket + 1 + LINK_SIZE);
                    cd.code[start_bracket] = OP_ONCE;
                    code += 1 + LINK_SIZE;
                    put(&mut cd.code, start_bracket + 1, code - start_bracket);
                    cd.code[code] = OP_KET;
                    put(&mut cd.code, code + 1, code - start_bracket);
                    code += 1 + LINK_SIZE;
                    length += 2 + 2 * LINK_SIZE as i32;
                }
                cd.open_caps.pop();
            }

            cd.bracount = max_bracount;

            io.code = code;
            io.ptr = ptr;
            io.firstchar = firstchar;
            io.firstcharflags = firstcharflags;
            io.reqchar = reqchar;
            io.reqcharflags = reqcharflags;

            if let Some(len) = lengthptr.as_deref_mut() {
                if OFLOW_MAX - *len < length {
                    cd.errorcode = ERR20;
                    return false;
                }
                *len += length;
            }
            return true;
        }

        if pass1 {
            code = start_bracket + 1 + LINK_SIZE + skipbytes;
            length += 1 + LINK_SIZE as i32;
        } else {
            cd.code[code] = OP_ALT;
            put(&mut cd.code, code + 1, code - last_branch);
            last_branch = code;
            bc.current_branch.set(code);
            code += 1 + LINK_SIZE;
        }
        ptr += 1;
    }
}

// ---- Start-of-match analysis ----

/// `is_anchored`.
fn is_anchored(cd: &Cd, mut code: usize, bracket_map: u32, atomcount: i32) -> bool {
    let c = &cd.code[..];
    loop {
        let scode = first_significant_code(c, code + oplen(c[code]), false);
        let op = c[scode];
        if op == OP_BRA || op == OP_BRAPOS || op == OP_SBRA || op == OP_SBRAPOS {
            if !is_anchored(cd, scode, bracket_map, atomcount) {
                return false;
            }
        } else if op == OP_CBRA || op == OP_CBRAPOS || op == OP_SCBRA || op == OP_SCBRAPOS {
            let n = get2(c, scode + 1 + LINK_SIZE);
            let new_map = bracket_map | if n < 32 { 1u32 << n } else { 1 };
            if !is_anchored(cd, scode, new_map, atomcount) {
                return false;
            }
        } else if op == OP_ASSERT {
            if !is_anchored(cd, scode, bracket_map, atomcount) {
                return false;
            }
        } else if op == OP_COND {
            if c[scode + get(c, scode + 1)] != OP_ALT {
                return false;
            }
            if !is_anchored(cd, scode, bracket_map, atomcount) {
                return false;
            }
        } else if op == OP_ONCE || op == OP_ONCE_NC {
            if !is_anchored(cd, scode, bracket_map, atomcount + 1) {
                return false;
            }
        } else if op == OP_TYPESTAR || op == OP_TYPEMINSTAR || op == OP_TYPEPOSSTAR {
            if c[scode + 1] != OP_ALLANY
                || (bracket_map & cd.backref_map) != 0
                || atomcount > 0
                || cd.had_pruneorskip
            {
                return false;
            }
        } else if op != OP_SOD && op != OP_SOM && op != OP_CIRC {
            return false;
        }
        code += get(c, code + 1);
        if c[code] != OP_ALT {
            return true;
        }
    }
}

/// `is_startline`.
fn is_startline(
    cd: &Cd,
    mut code: usize,
    bracket_map: u32,
    atomcount: i32,
    inassert: bool,
) -> bool {
    let c = &cd.code[..];
    loop {
        let mut scode = first_significant_code(c, code + oplen(c[code]), false);
        let mut op = c[scode];

        if op == OP_COND {
            scode += 1 + LINK_SIZE;
            if c[scode] == OP_CALLOUT {
                scode += oplen(OP_CALLOUT);
            }
            match c[scode] {
                OP_CREF | OP_DNCREF | OP_RREF | OP_DNRREF | OP_DEF | OP_FAIL => return false,
                _ => {
                    if !is_startline(cd, scode, bracket_map, atomcount, true) {
                        return false;
                    }
                    scode = skip_alts(c, scode);
                    scode += 1 + LINK_SIZE;
                }
            }
            scode = first_significant_code(c, scode, false);
            op = c[scode];
        }

        if op == OP_BRA || op == OP_BRAPOS || op == OP_SBRA || op == OP_SBRAPOS {
            if !is_startline(cd, scode, bracket_map, atomcount, inassert) {
                return false;
            }
        } else if op == OP_CBRA || op == OP_CBRAPOS || op == OP_SCBRA || op == OP_SCBRAPOS {
            let n = get2(c, scode + 1 + LINK_SIZE);
            let new_map = bracket_map | if n < 32 { 1u32 << n } else { 1 };
            if !is_startline(cd, scode, new_map, atomcount, inassert) {
                return false;
            }
        } else if op == OP_ASSERT {
            if !is_startline(cd, scode, bracket_map, atomcount, true) {
                return false;
            }
        } else if op == OP_ONCE || op == OP_ONCE_NC {
            if !is_startline(cd, scode, bracket_map, atomcount + 1, inassert) {
                return false;
            }
        } else if op == OP_TYPESTAR || op == OP_TYPEMINSTAR || op == OP_TYPEPOSSTAR {
            if c[scode + 1] != OP_ANY
                || (bracket_map & cd.backref_map) != 0
                || atomcount > 0
                || cd.had_pruneorskip
                || inassert
            {
                return false;
            }
        } else if op != OP_CIRC && op != OP_CIRCM {
            return false;
        }
        code += get(c, code + 1);
        if c[code] != OP_ALT {
            return true;
        }
    }
}

/// `find_firstassertedchar`.
fn find_firstassertedchar(code: &[u8], mut at: usize, flags: &mut i32, inassert: bool) -> u32 {
    let mut c = 0u32;
    let mut cflags = REQ_NONE;
    *flags = REQ_NONE;
    loop {
        let op0 = code[at];
        let xl = if op0 == OP_CBRA || op0 == OP_SCBRA || op0 == OP_CBRAPOS || op0 == OP_SCBRAPOS {
            IMM2_SIZE
        } else {
            0
        };
        let mut scode = first_significant_code(code, at + 1 + LINK_SIZE + xl, true);
        let op = code[scode];
        match op {
            OP_BRA | OP_BRAPOS | OP_CBRA | OP_SCBRA | OP_CBRAPOS | OP_SCBRAPOS | OP_ASSERT
            | OP_ONCE | OP_ONCE_NC => {
                let mut dflags = 0;
                let d = find_firstassertedchar(code, scode, &mut dflags, op == OP_ASSERT);
                if dflags < 0 {
                    return 0;
                }
                if cflags < 0 {
                    c = d;
                    cflags = dflags;
                } else if c != d || cflags != dflags {
                    return 0;
                }
            }
            OP_EXACT | OP_CHAR | OP_PLUS | OP_MINPLUS | OP_POSPLUS | OP_EXACTI | OP_CHARI
            | OP_PLUSI | OP_MINPLUSI | OP_POSPLUSI => {
                if op == OP_EXACT || op == OP_EXACTI {
                    scode += IMM2_SIZE;
                }
                if !inassert {
                    return 0;
                }
                let caseless = matches!(
                    op,
                    OP_EXACTI | OP_CHARI | OP_PLUSI | OP_MINPLUSI | OP_POSPLUSI
                );
                if cflags < 0 {
                    c = code[scode + 1] as u32;
                    cflags = if caseless { REQ_CASELESS } else { 0 };
                } else if c != code[scode + 1] as u32 {
                    return 0;
                }
            }
            _ => return 0,
        }
        at += get(code, at + 1);
        if code[at] != OP_ALT {
            break;
        }
    }
    *flags = cflags;
    c
}

/// `add_name`: insert into the sorted name table.
fn add_name(cd: &mut Cd, name: usize, length: usize, groupno: u32) {
    let es = cd.name_entry_size as usize;
    let mut slot = 0;
    for i in 0..cd.names_found as usize {
        let mut crc = cd.pat[name..name + length]
            .cmp(&cd.name_table[slot + IMM2_SIZE..slot + IMM2_SIZE + length]);
        if crc.is_eq() && cd.name_table[slot + IMM2_SIZE + length] != 0 {
            crc = std::cmp::Ordering::Less;
        }
        if crc.is_lt() {
            let n = (cd.names_found as usize - i) * es;
            cd.name_table.copy_within(slot..slot + n, slot + es);
            break;
        }
        slot += es;
    }
    put2(&mut cd.name_table, slot, groupno);
    let (table, pat) = (&mut cd.name_table, &cd.pat);
    table[slot + IMM2_SIZE..slot + IMM2_SIZE + length].copy_from_slice(&pat[name..name + length]);
    table[slot + IMM2_SIZE + length] = 0;
    cd.names_found += 1;
}

// ---- pcre_compile2 ----

/// A failed `pcre_compile2`: `*errorcodeptr`, `*errorptr`, `*erroroffset`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompileError {
    pub code: i32,
    pub text: &'static str,
    pub offset: i32,
}

fn compile_error(code: i32, offset: usize) -> CompileError {
    CompileError {
        code,
        text: find_error_text(code),
        offset: offset as i32,
    }
}

/// `pcre_compile2`. The pattern ends at its first NUL, as the C string
/// does; `tables` of `None` or the default tables are stored as NULL.
pub fn pcre_compile2(
    pattern: &[u8],
    mut options: u32,
    tables: Option<&'static Tables>,
) -> Result<Pcre, CompileError> {
    let tables = tables.unwrap_or(&DEFAULT_TABLES);
    let plen = pattern
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(pattern.len());
    let mut pat =
        Vec::with_capacity(plen + SUB_START_OF_WORD.len() + SUB_END_OF_WORD.len() + 3 + PAT_PAD);
    pat.extend_from_slice(&pattern[..plen]);
    pat.push(0);
    let sub_start = pat.len();
    pat.extend_from_slice(SUB_START_OF_WORD);
    pat.push(0);
    let sub_end = pat.len();
    pat.extend_from_slice(SUB_END_OF_WORD);
    pat.push(0);
    pat.resize(pat.len() + PAT_PAD, 0);

    let mut cd = Cd {
        tables,
        pat,
        end_pattern: plen,
        sub_start,
        sub_end,
        code: vec![0; COMPILE_WORK_SIZE + CODE_SLACK],
        work: Vec::new(),
        workspace_live: true,
        hwm: 0,
        workspace_size: COMPILE_WORK_SIZE,
        open_caps: Vec::new(),
        named_groups: Vec::new(),
        name_table: Vec::new(),
        names_found: 0,
        name_entry_size: 0,
        bracount: 0,
        final_bracount: 0,
        max_lookbehind: 0,
        top_backref: 0,
        backref_map: 0,
        namedrefcount: 0,
        parens_depth: 0,
        assert_depth: 0,
        external_options: 0,
        external_flags: 0,
        req_varyopt: 0,
        had_accept: false,
        had_pruneorskip: false,
        check_lookbehind: false,
        dupnames: false,
        dupgroups: false,
        iscondassert: false,
        nltype: 0,
        nllen: 0,
        nl: [0; 4],
        errorcode: 0,
    };

    // PCRE_EARLY_ERROR_RETURN before the pre-compile: ptr is still the
    // pattern start.
    if options & !PUBLIC_COMPILE_OPTIONS != 0 {
        return Err(compile_error(ERR17, 0));
    }
    let never_utf = options & PCRE_NEVER_UTF != 0;

    let mut limit_match = u32::MAX;
    let mut limit_recursion = u32::MAX;
    let mut skipatstart = 0;
    while cd.pat[skipatstart] == b'(' && cd.pat[skipatstart + 1] == b'*' {
        let s = skipatstart + 2;
        let mut newnl = 0;
        let mut newbsr = 0;

        if cd.starts(s, b"UTF8)") {
            skipatstart += 7;
            options |= PCRE_UTF8;
            continue;
        } else if cd.starts(s, b"UTF)") {
            skipatstart += 6;
            options |= PCRE_UTF8;
            continue;
        } else if cd.starts(s, b"UCP)") {
            skipatstart += 6;
            options |= PCRE_UCP;
            continue;
        } else if cd.starts(s, b"NO_AUTO_POSSESS)") {
            skipatstart += 18;
            options |= PCRE_NO_AUTO_POSSESS;
            continue;
        } else if cd.starts(s, b"NO_START_OPT)") {
            skipatstart += 15;
            options |= PCRE_NO_START_OPTIMIZE;
            continue;
        } else if cd.starts(s, b"LIMIT_MATCH=") || cd.starts(s, b"LIMIT_RECURSION=") {
            let is_match = cd.starts(s, b"LIMIT_MATCH=");
            let mut c: u32 = 0;
            let mut p = skipatstart + if is_match { 14 } else { 18 };
            while is_digit(cd.pat[p]) {
                if c > u32::MAX / 10 - 1 {
                    break;
                }
                c = c * 10 + (cd.pat[p] - b'0') as u32;
                p += 1;
            }
            let close = cd.pat[p];
            p += 1;
            if close != b')' {
                break;
            }
            if is_match {
                if c < limit_match {
                    limit_match = c;
                    cd.external_flags |= PCRE_MLSET;
                }
            } else if c < limit_recursion {
                limit_recursion = c;
                cd.external_flags |= PCRE_RLSET;
            }
            skipatstart = p;
            continue;
        }

        if cd.starts(s, b"CR)") {
            skipatstart += 5;
            newnl = PCRE_NEWLINE_CR;
        } else if cd.starts(s, b"LF)") {
            skipatstart += 5;
            newnl = PCRE_NEWLINE_LF;
        } else if cd.starts(s, b"CRLF)") {
            skipatstart += 7;
            newnl = PCRE_NEWLINE_CR + PCRE_NEWLINE_LF;
        } else if cd.starts(s, b"ANY)") {
            skipatstart += 6;
            newnl = PCRE_NEWLINE_ANY;
        } else if cd.starts(s, b"ANYCRLF)") {
            skipatstart += 10;
            newnl = PCRE_NEWLINE_ANYCRLF;
        } else if cd.starts(s, b"BSR_ANYCRLF)") {
            skipatstart += 14;
            newbsr = PCRE_BSR_ANYCRLF;
        } else if cd.starts(s, b"BSR_UNICODE)") {
            skipatstart += 14;
            newbsr = PCRE_BSR_UNICODE;
        }

        if newnl != 0 {
            options = (options & !PCRE_NEWLINE_BITS) | newnl;
        } else if newbsr != 0 {
            options = (options & !(PCRE_BSR_ANYCRLF | PCRE_BSR_UNICODE)) | newbsr;
        } else {
            break;
        }
    }

    let utf = options & PCRE_UTF8 != 0;
    if utf && never_utf {
        return Err(compile_error(ERR78, 0));
    }
    if utf {
        return Err(compile_error(ERR32, 0));
    }
    if options & PCRE_UCP != 0 {
        return Err(compile_error(ERR67, 0));
    }
    if options & (PCRE_BSR_ANYCRLF | PCRE_BSR_UNICODE) == (PCRE_BSR_ANYCRLF | PCRE_BSR_UNICODE) {
        return Err(compile_error(ERR56, 0));
    }

    let newline: i32 = match options & PCRE_NEWLINE_BITS {
        0 => NEWLINE,
        PCRE_NEWLINE_CR => b'\r' as i32,
        PCRE_NEWLINE_LF => b'\n' as i32,
        PCRE_NEWLINE_CRLF => ((b'\r' as i32) << 8) | b'\n' as i32,
        PCRE_NEWLINE_ANY => -1,
        PCRE_NEWLINE_ANYCRLF => -2,
        _ => return Err(compile_error(ERR56, 0)),
    };
    if newline == -2 {
        cd.nltype = NLTYPE_ANYCRLF;
    } else if newline < 0 {
        cd.nltype = NLTYPE_ANY;
    } else {
        cd.nltype = NLTYPE_FIXED;
        if newline > 255 {
            cd.nllen = 2;
            cd.nl[0] = (newline >> 8) as u8;
            cd.nl[1] = newline as u8;
        } else {
            cd.nllen = 1;
            cd.nl[0] = newline as u8;
        }
    }

    // Pre-compile: accumulate the length in the workspace.
    cd.external_options = options;
    let mut length: i32 = 1;
    cd.code[0] = OP_BRA;
    let none = GroupArgs {
        lookbehind: false,
        reset_bracount: false,
        skipbytes: 0,
        cond_depth: 0,
    };
    let mut io = BranchIo {
        options: cd.external_options,
        code: 0,
        ptr: skipatstart,
        ..BranchIo::default()
    };
    compile_regex(&mut cd, &mut io, none, None, Some(&mut length));
    let mut ptr = io.ptr;
    if cd.errorcode != 0 {
        return Err(compile_error(cd.errorcode, ptr));
    }
    if length > MAX_PATTERN_SIZE {
        return Err(compile_error(ERR20, ptr));
    }

    let name_count = cd.names_found;
    let name_entry_size = cd.name_entry_size;
    let table_len = (name_count * name_entry_size) as usize;
    let size = REAL_PCRE_SIZE + length as usize + table_len;
    let re_options = cd.external_options;

    cd.final_bracount = cd.bracount as i32;
    cd.parens_depth = 0;
    cd.assert_depth = 0;
    cd.bracount = 0;
    cd.max_lookbehind = 0;
    cd.name_table = vec![0; table_len];
    cd.code = vec![0; length as usize + CODE_SLACK];
    cd.work = vec![0; COMPILE_WORK_SIZE + CODE_SLACK];
    cd.hwm = 0;
    cd.iscondassert = false;
    cd.req_varyopt = 0;
    cd.had_accept = false;
    cd.had_pruneorskip = false;
    cd.check_lookbehind = false;
    cd.open_caps.clear();

    if cd.names_found > 0 {
        cd.names_found = 0;
        for i in 0..cd.named_groups.len() {
            let ng = cd.named_groups[i];
            add_name(&mut cd, ng.name, ng.length as usize, ng.number);
        }
    }

    cd.code[0] = OP_BRA;
    let mut io = BranchIo {
        options: re_options,
        code: 0,
        ptr: skipatstart,
        ..BranchIo::default()
    };
    compile_regex(&mut cd, &mut io, none, None, None);
    ptr = io.ptr;
    let mut code = io.code;
    let mut firstchar = io.firstchar;
    let mut firstcharflags = io.firstcharflags;
    let mut reqchar = io.reqchar;
    let mut reqcharflags = io.reqcharflags;

    let top_bracket = cd.bracount;
    let top_backref = cd.top_backref;
    let max_lookbehind = cd.max_lookbehind;
    let mut flags = cd.external_flags | PCRE_MODE;

    if cd.had_accept {
        reqchar = 0;
        reqcharflags = REQ_NONE;
    }

    let mut errorcode = cd.errorcode;
    if errorcode == 0 && cd.pat[ptr] != 0 {
        errorcode = ERR22;
    }

    cd.code[code] = OP_END;
    code += 1;
    if code > length as usize {
        errorcode = ERR23;
    }

    if cd.hwm > 0 {
        let mut prev_recno = -1;
        let mut groupptr = None;
        while errorcode == 0 && cd.hwm > 0 {
            cd.hwm -= LINK_SIZE;
            let offset = get(&cd.work, cd.hwm);
            if offset == 0 || cd.code[offset - 1] != OP_RECURSE {
                errorcode = ERR10;
                break;
            }
            let recno = get(&cd.code, offset) as i32;
            if recno != prev_recno {
                groupptr = find_bracket(&cd.code, 0, recno);
                prev_recno = recno;
            }
            match groupptr {
                None => errorcode = ERR53,
                Some(g) => put(&mut cd.code, offset, g),
            }
        }
    }

    cd.workspace_live = false;

    if errorcode == 0 && top_backref > top_bracket as i32 {
        errorcode = ERR15;
    }

    if errorcode == 0 && options & PCRE_NO_AUTO_POSSESS == 0 {
        auto_possessify(&mut cd);
    }

    if errorcode == 0 && cd.check_lookbehind {
        let mut cc = find_bracket(&cd.code, 0, -1);
        while let Some(c) = cc {
            if get(&cd.code, c + 1) == 0 {
                let be = c - 1 - LINK_SIZE + get(&cd.code, c - LINK_SIZE);
                let end_op = cd.code[be];
                cd.code[be] = OP_END;
                let fixed_length = find_fixedlength(&cd.code, c, true, None);
                cd.code[be] = end_op;
                if fixed_length < 0 {
                    errorcode = match fixed_length {
                        -2 => ERR36,
                        -4 => ERR70,
                        _ => ERR25,
                    };
                    break;
                }
                if fixed_length > cd.max_lookbehind {
                    cd.max_lookbehind = fixed_length;
                }
                put(&mut cd.code, c + 1, fixed_length as usize);
            }
            cc = find_bracket(&cd.code, c + 1 + LINK_SIZE, -1);
        }
    }

    if errorcode != 0 {
        return Err(compile_error(errorcode, ptr));
    }

    let mut options_out = re_options;
    let mut first_char = 0;
    let mut req_char = 0;
    if options_out & PCRE_ANCHORED == 0 {
        if is_anchored(&cd, 0, 0, 0) {
            options_out |= PCRE_ANCHORED;
        } else {
            if firstcharflags < 0 {
                firstchar = find_firstassertedchar(&cd.code, 0, &mut firstcharflags, false);
            }
            if firstcharflags >= 0 {
                first_char = firstchar & 0xff;
                if firstcharflags & REQ_CASELESS != 0 && cd.fcc(first_char) as u32 != first_char {
                    flags |= PCRE_FCH_CASELESS;
                }
                flags |= PCRE_FIRSTSET;
            } else if is_startline(&cd, 0, 0, 0, false) {
                flags |= PCRE_STARTLINE;
            }
        }
    }

    if reqcharflags >= 0 && (options_out & PCRE_ANCHORED == 0 || reqcharflags & REQ_VARY != 0) {
        req_char = reqchar & 0xff;
        if reqcharflags & REQ_CASELESS != 0 && cd.fcc(req_char) as u32 != req_char {
            flags |= PCRE_RCH_CASELESS;
        }
        flags |= PCRE_REQCHSET;
    }

    let mut cs = 0;
    loop {
        if could_be_empty_branch(&cd, cs, code, None) {
            flags |= PCRE_MATCH_EMPTY;
            break;
        }
        cs += get(&cd.code, cs + 1);
        if cd.code[cs] != OP_ALT {
            break;
        }
    }

    let mut data = std::mem::take(&mut cd.name_table);
    data.extend_from_slice(&cd.code[..length as usize]);

    Ok(Pcre {
        magic_number: MAGIC_NUMBER,
        size: size as u32,
        options: options_out,
        flags,
        limit_match,
        limit_recursion,
        first_char: first_char as u16,
        req_char: req_char as u16,
        max_lookbehind: max_lookbehind as u16,
        top_bracket: top_bracket as u16,
        top_backref: top_backref as u16,
        name_table_offset: REAL_PCRE_SIZE as u16,
        name_entry_size: name_entry_size as u16,
        name_count: name_count as u16,
        ref_count: 0,
        tables: if std::ptr::eq(tables, &DEFAULT_TABLES) {
            None
        } else {
            Some(tables)
        },
        data,
    })
}
