//! `pcre_internal.h` and `pcre.h` for the build stream-pcre-sys/build.rs
//! makes: 8-bit, no UTF/UCP/JIT, LINK_SIZE 2, NEWLINE 10. Opcode values,
//! option and flag bits and the compiled-pattern layout are C's, so the
//! bytecode compares with C's byte for byte.

#![allow(dead_code)]

use super::chartables::DEFAULT_TABLES;

// ---- Build configuration (build.rs) ----

pub const LINK_SIZE: usize = 2;
pub const IMM2_SIZE: usize = 2;
/// `MAX_PATTERN_SIZE` for LINK_SIZE 2.
pub const MAX_PATTERN_SIZE: i32 = 1 << 16;
pub const NEWLINE: i32 = 10;
pub const MATCH_LIMIT: u64 = 10_000_000;
pub const MATCH_LIMIT_RECURSION: u64 = MATCH_LIMIT;
pub const PARENS_NEST_LIMIT: i32 = 250;
pub const POSIX_MALLOC_THRESHOLD: i32 = 10;
pub const MAX_NAME_SIZE: i32 = 32;
pub const MAX_NAME_COUNT: i32 = 10000;
/// 8-bit mode: `MAX_MARK` is `(1u << 8) - 1`.
pub const MAX_MARK: u32 = (1 << 8) - 1;
pub const NOTACHAR: u32 = 0xffff_ffff;
pub const PCRE_UINT32_MAX: u32 = u32::MAX;

// ---- GET/PUT for LINK_SIZE 2 and IMM2_SIZE 2 ----

#[inline]
pub fn get(a: &[u8], n: usize) -> usize {
    ((a[n] as usize) << 8) | a[n + 1] as usize
}

#[inline]
pub fn put(a: &mut [u8], n: usize, d: usize) {
    a[n] = (d >> 8) as u8;
    a[n + 1] = (d & 255) as u8;
}

#[inline]
pub fn get2(a: &[u8], n: usize) -> u32 {
    ((a[n] as u32) << 8) | a[n + 1] as u32
}

#[inline]
pub fn put2(a: &mut [u8], n: usize, d: u32) {
    a[n] = (d >> 8) as u8;
    a[n + 1] = (d & 255) as u8;
}

// ---- Public options (pcre.h) ----

pub const PCRE_CASELESS: u32 = 0x0000_0001;
pub const PCRE_MULTILINE: u32 = 0x0000_0002;
pub const PCRE_DOTALL: u32 = 0x0000_0004;
pub const PCRE_EXTENDED: u32 = 0x0000_0008;
pub const PCRE_ANCHORED: u32 = 0x0000_0010;
pub const PCRE_DOLLAR_ENDONLY: u32 = 0x0000_0020;
pub const PCRE_EXTRA: u32 = 0x0000_0040;
pub const PCRE_NOTBOL: u32 = 0x0000_0080;
pub const PCRE_NOTEOL: u32 = 0x0000_0100;
pub const PCRE_UNGREEDY: u32 = 0x0000_0200;
pub const PCRE_NOTEMPTY: u32 = 0x0000_0400;
pub const PCRE_UTF8: u32 = 0x0000_0800;
pub const PCRE_NO_AUTO_CAPTURE: u32 = 0x0000_1000;
pub const PCRE_NO_UTF8_CHECK: u32 = 0x0000_2000;
pub const PCRE_AUTO_CALLOUT: u32 = 0x0000_4000;
pub const PCRE_PARTIAL_SOFT: u32 = 0x0000_8000;
pub const PCRE_PARTIAL: u32 = 0x0000_8000;
pub const PCRE_NEVER_UTF: u32 = 0x0001_0000;
pub const PCRE_DFA_SHORTEST: u32 = 0x0001_0000;
pub const PCRE_NO_AUTO_POSSESS: u32 = 0x0002_0000;
pub const PCRE_DFA_RESTART: u32 = 0x0002_0000;
pub const PCRE_FIRSTLINE: u32 = 0x0004_0000;
pub const PCRE_DUPNAMES: u32 = 0x0008_0000;
pub const PCRE_NEWLINE_CR: u32 = 0x0010_0000;
pub const PCRE_NEWLINE_LF: u32 = 0x0020_0000;
pub const PCRE_NEWLINE_CRLF: u32 = 0x0030_0000;
pub const PCRE_NEWLINE_ANY: u32 = 0x0040_0000;
pub const PCRE_NEWLINE_ANYCRLF: u32 = 0x0050_0000;
pub const PCRE_BSR_ANYCRLF: u32 = 0x0080_0000;
pub const PCRE_BSR_UNICODE: u32 = 0x0100_0000;
pub const PCRE_JAVASCRIPT_COMPAT: u32 = 0x0200_0000;
pub const PCRE_NO_START_OPTIMIZE: u32 = 0x0400_0000;
pub const PCRE_PARTIAL_HARD: u32 = 0x0800_0000;
pub const PCRE_NOTEMPTY_ATSTART: u32 = 0x1000_0000;
pub const PCRE_UCP: u32 = 0x2000_0000;

// ---- Exec error codes (pcre.h) ----

pub const PCRE_ERROR_NOMATCH: i32 = -1;
pub const PCRE_ERROR_NULL: i32 = -2;
pub const PCRE_ERROR_BADOPTION: i32 = -3;
pub const PCRE_ERROR_BADMAGIC: i32 = -4;
pub const PCRE_ERROR_UNKNOWN_OPCODE: i32 = -5;
pub const PCRE_ERROR_NOMEMORY: i32 = -6;
pub const PCRE_ERROR_NOSUBSTRING: i32 = -7;
pub const PCRE_ERROR_MATCHLIMIT: i32 = -8;
pub const PCRE_ERROR_CALLOUT: i32 = -9;
pub const PCRE_ERROR_BADUTF8: i32 = -10;
pub const PCRE_ERROR_BADUTF8_OFFSET: i32 = -11;
pub const PCRE_ERROR_PARTIAL: i32 = -12;
pub const PCRE_ERROR_BADPARTIAL: i32 = -13;
pub const PCRE_ERROR_INTERNAL: i32 = -14;
pub const PCRE_ERROR_BADCOUNT: i32 = -15;
pub const PCRE_ERROR_RECURSIONLIMIT: i32 = -21;
pub const PCRE_ERROR_BADNEWLINE: i32 = -23;
pub const PCRE_ERROR_BADOFFSET: i32 = -24;
pub const PCRE_ERROR_SHORTUTF8: i32 = -25;
pub const PCRE_ERROR_RECURSELOOP: i32 = -26;
pub const PCRE_ERROR_BADMODE: i32 = -28;
pub const PCRE_ERROR_BADENDIANNESS: i32 = -29;
pub const PCRE_ERROR_BADLENGTH: i32 = -32;
pub const PCRE_ERROR_UNSET: i32 = -33;

// ---- pcre_fullinfo / pcre_config request codes (pcre.h) ----

pub const PCRE_INFO_OPTIONS: i32 = 0;
pub const PCRE_INFO_SIZE: i32 = 1;
pub const PCRE_INFO_CAPTURECOUNT: i32 = 2;
pub const PCRE_INFO_BACKREFMAX: i32 = 3;
pub const PCRE_INFO_FIRSTBYTE: i32 = 4;
pub const PCRE_INFO_FIRSTTABLE: i32 = 5;
pub const PCRE_INFO_LASTLITERAL: i32 = 6;
pub const PCRE_INFO_NAMEENTRYSIZE: i32 = 7;
pub const PCRE_INFO_NAMECOUNT: i32 = 8;
pub const PCRE_INFO_NAMETABLE: i32 = 9;
pub const PCRE_INFO_STUDYSIZE: i32 = 10;
pub const PCRE_INFO_DEFAULT_TABLES: i32 = 11;
pub const PCRE_INFO_OKPARTIAL: i32 = 12;
pub const PCRE_INFO_JCHANGED: i32 = 13;
pub const PCRE_INFO_HASCRORLF: i32 = 14;
pub const PCRE_INFO_MINLENGTH: i32 = 15;
pub const PCRE_INFO_JIT: i32 = 16;
pub const PCRE_INFO_JITSIZE: i32 = 17;
pub const PCRE_INFO_MAXLOOKBEHIND: i32 = 18;
pub const PCRE_INFO_FIRSTCHARACTER: i32 = 19;
pub const PCRE_INFO_FIRSTCHARACTERFLAGS: i32 = 20;
pub const PCRE_INFO_REQUIREDCHAR: i32 = 21;
pub const PCRE_INFO_REQUIREDCHARFLAGS: i32 = 22;
pub const PCRE_INFO_MATCHLIMIT: i32 = 23;
pub const PCRE_INFO_RECURSIONLIMIT: i32 = 24;
pub const PCRE_INFO_MATCH_EMPTY: i32 = 25;

pub const PCRE_CONFIG_UTF8: i32 = 0;
pub const PCRE_CONFIG_NEWLINE: i32 = 1;
pub const PCRE_CONFIG_LINK_SIZE: i32 = 2;
pub const PCRE_CONFIG_POSIX_MALLOC_THRESHOLD: i32 = 3;
pub const PCRE_CONFIG_MATCH_LIMIT: i32 = 4;
pub const PCRE_CONFIG_STACKRECURSE: i32 = 5;
pub const PCRE_CONFIG_UNICODE_PROPERTIES: i32 = 6;
pub const PCRE_CONFIG_MATCH_LIMIT_RECURSION: i32 = 7;
pub const PCRE_CONFIG_BSR: i32 = 8;
pub const PCRE_CONFIG_JIT: i32 = 9;
pub const PCRE_CONFIG_UTF16: i32 = 10;
pub const PCRE_CONFIG_JITTARGET: i32 = 11;
pub const PCRE_CONFIG_UTF32: i32 = 12;
pub const PCRE_CONFIG_PARENS_LIMIT: i32 = 13;

// ---- Private flags in `RealPcre::flags` ----

pub const PCRE_MODE8: u32 = 0x0000_0001;
pub const PCRE_MODE16: u32 = 0x0000_0002;
pub const PCRE_MODE32: u32 = 0x0000_0004;
pub const PCRE_FIRSTSET: u32 = 0x0000_0010;
pub const PCRE_FCH_CASELESS: u32 = 0x0000_0020;
pub const PCRE_REQCHSET: u32 = 0x0000_0040;
pub const PCRE_RCH_CASELESS: u32 = 0x0000_0080;
pub const PCRE_STARTLINE: u32 = 0x0000_0100;
pub const PCRE_NOPARTIAL: u32 = 0x0000_0200;
pub const PCRE_JCHANGED: u32 = 0x0000_0400;
pub const PCRE_HASCRORLF: u32 = 0x0000_0800;
pub const PCRE_HASTHEN: u32 = 0x0000_1000;
pub const PCRE_MLSET: u32 = 0x0000_2000;
pub const PCRE_RLSET: u32 = 0x0000_4000;
pub const PCRE_MATCH_EMPTY: u32 = 0x0000_8000;
pub const PCRE_MODE: u32 = PCRE_MODE8;
pub const PCRE_MODE_MASK: u32 = PCRE_MODE8 | PCRE_MODE16 | PCRE_MODE32;

pub const PCRE_STUDY_MAPPED: u32 = 0x0001;
pub const PCRE_STUDY_MINLEN: u32 = 0x0002;

pub const PCRE_NEWLINE_BITS: u32 =
    PCRE_NEWLINE_CR | PCRE_NEWLINE_LF | PCRE_NEWLINE_ANY | PCRE_NEWLINE_ANYCRLF;

pub const PUBLIC_COMPILE_OPTIONS: u32 = PCRE_CASELESS
    | PCRE_EXTENDED
    | PCRE_ANCHORED
    | PCRE_MULTILINE
    | PCRE_DOTALL
    | PCRE_DOLLAR_ENDONLY
    | PCRE_EXTRA
    | PCRE_UNGREEDY
    | PCRE_UTF8
    | PCRE_NO_AUTO_CAPTURE
    | PCRE_NO_AUTO_POSSESS
    | PCRE_NO_UTF8_CHECK
    | PCRE_AUTO_CALLOUT
    | PCRE_FIRSTLINE
    | PCRE_DUPNAMES
    | PCRE_NEWLINE_BITS
    | PCRE_BSR_ANYCRLF
    | PCRE_BSR_UNICODE
    | PCRE_JAVASCRIPT_COMPAT
    | PCRE_UCP
    | PCRE_NO_START_OPTIMIZE
    | PCRE_NEVER_UTF;

pub const PUBLIC_EXEC_OPTIONS: u32 = PCRE_ANCHORED
    | PCRE_NOTBOL
    | PCRE_NOTEOL
    | PCRE_NOTEMPTY
    | PCRE_NOTEMPTY_ATSTART
    | PCRE_NO_UTF8_CHECK
    | PCRE_PARTIAL_HARD
    | PCRE_PARTIAL_SOFT
    | PCRE_NEWLINE_BITS
    | PCRE_BSR_ANYCRLF
    | PCRE_BSR_UNICODE
    | PCRE_NO_START_OPTIMIZE;

pub const MAGIC_NUMBER: u32 = 0x5043_5245;
pub const REVERSED_MAGIC_NUMBER: u32 = 0x4552_4350;
pub const REQ_BYTE_MAX: i32 = 1000;

// ---- Newline types ----

pub const NLTYPE_FIXED: i32 = 0;
pub const NLTYPE_ANY: i32 = 1;
pub const NLTYPE_ANYCRLF: i32 = 2;

// ---- Character values the code names (ASCII, non-UTF) ----

pub const CHAR_NEL: u32 = 0x85;
pub const CHAR_NBSP: u32 = 0xa0;
pub const CHAR_ESC: u32 = 0x1b;
pub const CHAR_DEL: u32 = 0x7f;

/// `HSPACE_LIST` (`_pcre_hspace_list` in pcre_tables.c).
pub const HSPACE_LIST: [u32; 19] = [
    0x09, 0x20, 0xa0, 0x1680, 0x180e, 0x2000, 0x2001, 0x2002, 0x2003, 0x2004, 0x2005, 0x2006,
    0x2007, 0x2008, 0x2009, 0x200A, 0x202f, 0x205f, 0x3000,
];
/// `VSPACE_LIST` (`_pcre_vspace_list` in pcre_tables.c).
pub const VSPACE_LIST: [u32; 7] = [0x0a, 0x0b, 0x0c, 0x0d, 0x85, 0x2028, 0x2029];

// ---- Unicode property types and XCLASS items (only named, never compiled) ----

pub const PT_ANY: u32 = 0;
pub const PT_LAMP: u32 = 1;
pub const PT_GC: u32 = 2;
pub const PT_PC: u32 = 3;
pub const PT_SC: u32 = 4;
pub const PT_ALNUM: u32 = 5;
pub const PT_SPACE: u32 = 6;
pub const PT_PXSPACE: u32 = 7;
pub const PT_WORD: u32 = 8;
pub const PT_CLIST: u32 = 9;
pub const PT_UCNC: u32 = 10;
pub const PT_TABSIZE: u32 = 11;

pub const XCL_NOT: u8 = 0x01;
pub const XCL_MAP: u8 = 0x02;
pub const XCL_HASPROP: u8 = 0x04;
pub const XCL_END: u8 = 0;
pub const XCL_SINGLE: u8 = 1;
pub const XCL_RANGE: u8 = 2;
pub const XCL_PROP: u8 = 3;
pub const XCL_NOTPROP: u8 = 4;

// ---- Escapes (check_escape returns -ESC_LC_X); order matches the opcodes ----

pub const ESC_A: i32 = 1;
pub const ESC_G: i32 = 2;
pub const ESC_K: i32 = 3;
pub const ESC_B: i32 = 4;
pub const ESC_LC_B: i32 = 5;
pub const ESC_D: i32 = 6;
pub const ESC_LC_D: i32 = 7;
pub const ESC_S: i32 = 8;
pub const ESC_LC_S: i32 = 9;
pub const ESC_W: i32 = 10;
pub const ESC_LC_W: i32 = 11;
pub const ESC_N: i32 = 12;
pub const ESC_DUM: i32 = 13;
pub const ESC_C: i32 = 14;
pub const ESC_P: i32 = 15;
pub const ESC_LC_P: i32 = 16;
pub const ESC_R: i32 = 17;
pub const ESC_H: i32 = 18;
pub const ESC_LC_H: i32 = 19;
pub const ESC_V: i32 = 20;
pub const ESC_LC_V: i32 = 21;
pub const ESC_X: i32 = 22;
pub const ESC_Z: i32 = 23;
pub const ESC_LC_Z: i32 = 24;
pub const ESC_E: i32 = 25;
pub const ESC_Q: i32 = 26;
pub const ESC_LC_G: i32 = 27;
pub const ESC_LC_K: i32 = 28;
pub const ESC_DU: i32 = 29;
pub const ESC_LC_DU: i32 = 30;
pub const ESC_SU: i32 = 31;
pub const ESC_LC_SU: i32 = 32;
pub const ESC_WU: i32 = 33;
pub const ESC_LC_WU: i32 = 34;

/// Escapes that are just a data value.
pub const ESC_LC_A: i32 = 0x07;
pub const ESC_LC_E: i32 = 0x1b;
pub const ESC_LC_F: i32 = 0x0c;
pub const ESC_LC_N: i32 = 0x0a;
pub const ESC_LC_R: i32 = 0x0d;
pub const ESC_TEE: i32 = 0x09;

// ---- Opcodes ----

pub const FIRST_AUTOTAB_OP: u8 = OP_NOT_DIGIT;
pub const LAST_AUTOTAB_LEFT_OP: u8 = OP_EXTUNI;
pub const LAST_AUTOTAB_RIGHT_OP: u8 = OP_DOLLM;

pub const OP_END: u8 = 0;
pub const OP_SOD: u8 = 1;
pub const OP_SOM: u8 = 2;
pub const OP_SET_SOM: u8 = 3;
pub const OP_NOT_WORD_BOUNDARY: u8 = 4;
pub const OP_WORD_BOUNDARY: u8 = 5;
pub const OP_NOT_DIGIT: u8 = 6;
pub const OP_DIGIT: u8 = 7;
pub const OP_NOT_WHITESPACE: u8 = 8;
pub const OP_WHITESPACE: u8 = 9;
pub const OP_NOT_WORDCHAR: u8 = 10;
pub const OP_WORDCHAR: u8 = 11;
pub const OP_ANY: u8 = 12;
pub const OP_ALLANY: u8 = 13;
pub const OP_ANYBYTE: u8 = 14;
pub const OP_NOTPROP: u8 = 15;
pub const OP_PROP: u8 = 16;
pub const OP_ANYNL: u8 = 17;
pub const OP_NOT_HSPACE: u8 = 18;
pub const OP_HSPACE: u8 = 19;
pub const OP_NOT_VSPACE: u8 = 20;
pub const OP_VSPACE: u8 = 21;
pub const OP_EXTUNI: u8 = 22;
pub const OP_EODN: u8 = 23;
pub const OP_EOD: u8 = 24;
pub const OP_DOLL: u8 = 25;
pub const OP_DOLLM: u8 = 26;
pub const OP_CIRC: u8 = 27;
pub const OP_CIRCM: u8 = 28;
pub const OP_CHAR: u8 = 29;
pub const OP_CHARI: u8 = 30;
pub const OP_NOT: u8 = 31;
pub const OP_NOTI: u8 = 32;
pub const OP_STAR: u8 = 33;
pub const OP_MINSTAR: u8 = 34;
pub const OP_PLUS: u8 = 35;
pub const OP_MINPLUS: u8 = 36;
pub const OP_QUERY: u8 = 37;
pub const OP_MINQUERY: u8 = 38;
pub const OP_UPTO: u8 = 39;
pub const OP_MINUPTO: u8 = 40;
pub const OP_EXACT: u8 = 41;
pub const OP_POSSTAR: u8 = 42;
pub const OP_POSPLUS: u8 = 43;
pub const OP_POSQUERY: u8 = 44;
pub const OP_POSUPTO: u8 = 45;
pub const OP_STARI: u8 = 46;
pub const OP_MINSTARI: u8 = 47;
pub const OP_PLUSI: u8 = 48;
pub const OP_MINPLUSI: u8 = 49;
pub const OP_QUERYI: u8 = 50;
pub const OP_MINQUERYI: u8 = 51;
pub const OP_UPTOI: u8 = 52;
pub const OP_MINUPTOI: u8 = 53;
pub const OP_EXACTI: u8 = 54;
pub const OP_POSSTARI: u8 = 55;
pub const OP_POSPLUSI: u8 = 56;
pub const OP_POSQUERYI: u8 = 57;
pub const OP_POSUPTOI: u8 = 58;
pub const OP_NOTSTAR: u8 = 59;
pub const OP_NOTMINSTAR: u8 = 60;
pub const OP_NOTPLUS: u8 = 61;
pub const OP_NOTMINPLUS: u8 = 62;
pub const OP_NOTQUERY: u8 = 63;
pub const OP_NOTMINQUERY: u8 = 64;
pub const OP_NOTUPTO: u8 = 65;
pub const OP_NOTMINUPTO: u8 = 66;
pub const OP_NOTEXACT: u8 = 67;
pub const OP_NOTPOSSTAR: u8 = 68;
pub const OP_NOTPOSPLUS: u8 = 69;
pub const OP_NOTPOSQUERY: u8 = 70;
pub const OP_NOTPOSUPTO: u8 = 71;
pub const OP_NOTSTARI: u8 = 72;
pub const OP_NOTMINSTARI: u8 = 73;
pub const OP_NOTPLUSI: u8 = 74;
pub const OP_NOTMINPLUSI: u8 = 75;
pub const OP_NOTQUERYI: u8 = 76;
pub const OP_NOTMINQUERYI: u8 = 77;
pub const OP_NOTUPTOI: u8 = 78;
pub const OP_NOTMINUPTOI: u8 = 79;
pub const OP_NOTEXACTI: u8 = 80;
pub const OP_NOTPOSSTARI: u8 = 81;
pub const OP_NOTPOSPLUSI: u8 = 82;
pub const OP_NOTPOSQUERYI: u8 = 83;
pub const OP_NOTPOSUPTOI: u8 = 84;
pub const OP_TYPESTAR: u8 = 85;
pub const OP_TYPEMINSTAR: u8 = 86;
pub const OP_TYPEPLUS: u8 = 87;
pub const OP_TYPEMINPLUS: u8 = 88;
pub const OP_TYPEQUERY: u8 = 89;
pub const OP_TYPEMINQUERY: u8 = 90;
pub const OP_TYPEUPTO: u8 = 91;
pub const OP_TYPEMINUPTO: u8 = 92;
pub const OP_TYPEEXACT: u8 = 93;
pub const OP_TYPEPOSSTAR: u8 = 94;
pub const OP_TYPEPOSPLUS: u8 = 95;
pub const OP_TYPEPOSQUERY: u8 = 96;
pub const OP_TYPEPOSUPTO: u8 = 97;
pub const OP_CRSTAR: u8 = 98;
pub const OP_CRMINSTAR: u8 = 99;
pub const OP_CRPLUS: u8 = 100;
pub const OP_CRMINPLUS: u8 = 101;
pub const OP_CRQUERY: u8 = 102;
pub const OP_CRMINQUERY: u8 = 103;
pub const OP_CRRANGE: u8 = 104;
pub const OP_CRMINRANGE: u8 = 105;
pub const OP_CRPOSSTAR: u8 = 106;
pub const OP_CRPOSPLUS: u8 = 107;
pub const OP_CRPOSQUERY: u8 = 108;
pub const OP_CRPOSRANGE: u8 = 109;
pub const OP_CLASS: u8 = 110;
pub const OP_NCLASS: u8 = 111;
pub const OP_XCLASS: u8 = 112;
pub const OP_REF: u8 = 113;
pub const OP_REFI: u8 = 114;
pub const OP_DNREF: u8 = 115;
pub const OP_DNREFI: u8 = 116;
pub const OP_RECURSE: u8 = 117;
pub const OP_CALLOUT: u8 = 118;
pub const OP_ALT: u8 = 119;
pub const OP_KET: u8 = 120;
pub const OP_KETRMAX: u8 = 121;
pub const OP_KETRMIN: u8 = 122;
pub const OP_KETRPOS: u8 = 123;
pub const OP_REVERSE: u8 = 124;
pub const OP_ASSERT: u8 = 125;
pub const OP_ASSERT_NOT: u8 = 126;
pub const OP_ASSERTBACK: u8 = 127;
pub const OP_ASSERTBACK_NOT: u8 = 128;
pub const OP_ONCE: u8 = 129;
pub const OP_ONCE_NC: u8 = 130;
pub const OP_BRA: u8 = 131;
pub const OP_BRAPOS: u8 = 132;
pub const OP_CBRA: u8 = 133;
pub const OP_CBRAPOS: u8 = 134;
pub const OP_COND: u8 = 135;
pub const OP_SBRA: u8 = 136;
pub const OP_SBRAPOS: u8 = 137;
pub const OP_SCBRA: u8 = 138;
pub const OP_SCBRAPOS: u8 = 139;
pub const OP_SCOND: u8 = 140;
pub const OP_CREF: u8 = 141;
pub const OP_DNCREF: u8 = 142;
pub const OP_RREF: u8 = 143;
pub const OP_DNRREF: u8 = 144;
pub const OP_DEF: u8 = 145;
pub const OP_BRAZERO: u8 = 146;
pub const OP_BRAMINZERO: u8 = 147;
pub const OP_BRAPOSZERO: u8 = 148;
pub const OP_MARK: u8 = 149;
pub const OP_PRUNE: u8 = 150;
pub const OP_PRUNE_ARG: u8 = 151;
pub const OP_SKIP: u8 = 152;
pub const OP_SKIP_ARG: u8 = 153;
pub const OP_THEN: u8 = 154;
pub const OP_THEN_ARG: u8 = 155;
pub const OP_COMMIT: u8 = 156;
pub const OP_FAIL: u8 = 157;
pub const OP_ACCEPT: u8 = 158;
pub const OP_ASSERT_ACCEPT: u8 = 159;
pub const OP_CLOSE: u8 = 160;
pub const OP_SKIPZERO: u8 = 161;
pub const OP_TABLE_LENGTH: usize = 162;

/// `OP_LENGTHS` (`_pcre_OP_lengths` in pcre_tables.c) for 8-bit, LINK_SIZE 2.
#[rustfmt::skip]
pub const OP_LENGTHS: [u8; OP_TABLE_LENGTH] = {
    const L: u8 = LINK_SIZE as u8;
    const I: u8 = IMM2_SIZE as u8;
    [
        1,                          // End
        1, 1, 1, 1, 1,              // \A, \G, \K, \B, \b
        1, 1, 1, 1, 1, 1,           // \D, \d, \S, \s, \W, \w
        1, 1, 1,                    // Any, AllAny, Anybyte
        3, 3,                       // \P, \p
        1, 1, 1, 1, 1,              // \R, \H, \h, \V, \v
        1,                          // \X
        1, 1, 1, 1, 1, 1,           // \Z, \z, $, $M ^, ^M
        2, 2, 2, 2,                 // Char, Chari, not, noti
        2, 2, 2, 2, 2, 2,           // *, *?, +, +?, ?, ??
        2 + I, 2 + I, 2 + I,        // upto, minupto, exact
        2, 2, 2, 2 + I,             // *+, ++, ?+, upto+
        2, 2, 2, 2, 2, 2,           // caseless
        2 + I, 2 + I, 2 + I,
        2, 2, 2, 2 + I,
        2, 2, 2, 2, 2, 2,           // NOT
        2 + I, 2 + I, 2 + I,
        2, 2, 2, 2 + I,
        2, 2, 2, 2, 2, 2,           // NOT caseless
        2 + I, 2 + I, 2 + I,
        2, 2, 2, 2 + I,
        2, 2, 2, 2, 2, 2,           // Type
        2 + I, 2 + I, 2 + I,
        2, 2, 2, 2 + I,
        1, 1, 1, 1, 1, 1,           // class/ref *, *?, +, +?, ?, ??
        1 + 2 * I, 1 + 2 * I,       // CRRANGE, CRMINRANGE
        1, 1, 1, 1 + 2 * I,         // CRPOS*
        1 + 32, 1 + 32,             // CLASS, NCLASS
        0,                          // XCLASS - variable length
        1 + I, 1 + I,               // REF, REFI
        1 + 2 * I, 1 + 2 * I,       // DNREF, DNREFI
        1 + L,                      // RECURSE
        2 + 2 * L,                  // CALLOUT
        1 + L, 1 + L, 1 + L, 1 + L, 1 + L, // Alt, Ket, KetRmax, KetRmin, KetRpos
        1 + L,                      // Reverse
        1 + L, 1 + L, 1 + L, 1 + L, // Assert, Assert not, AssertB, AssertB not
        1 + L, 1 + L,               // ONCE, ONCE_NC
        1 + L, 1 + L,               // BRA, BRAPOS
        1 + L + I, 1 + L + I,       // CBRA, CBRAPOS
        1 + L,                      // COND
        1 + L, 1 + L,               // SBRA, SBRAPOS
        1 + L + I, 1 + L + I,       // SCBRA, SCBRAPOS
        1 + L,                      // SCOND
        1 + I, 1 + 2 * I,           // CREF, DNCREF
        1 + I, 1 + 2 * I,           // RREF, DNRREF
        1,                          // DEF
        1, 1, 1,                    // BRAZERO, BRAMINZERO, BRAPOSZERO
        3, 1, 3,                    // MARK, PRUNE, PRUNE_ARG
        1, 3,                       // SKIP, SKIP_ARG
        1, 3,                       // THEN, THEN_ARG
        1, 1, 1, 1,                 // COMMIT, FAIL, ACCEPT, ASSERT_ACCEPT
        1 + I, 1,                   // CLOSE, SKIPZERO
    ]
};

/// `RREF_ANY`: OP_RREF's "any recursion" condition.
pub const RREF_ANY: u32 = 0xffff;

// ---- Compile error codes (the ERRn enum) ----

pub const ERR0: i32 = 0;
pub const ERR1: i32 = 1;
pub const ERR2: i32 = 2;
pub const ERR3: i32 = 3;
pub const ERR4: i32 = 4;
pub const ERR5: i32 = 5;
pub const ERR6: i32 = 6;
pub const ERR7: i32 = 7;
pub const ERR8: i32 = 8;
pub const ERR9: i32 = 9;
pub const ERR10: i32 = 10;
pub const ERR11: i32 = 11;
pub const ERR12: i32 = 12;
pub const ERR13: i32 = 13;
pub const ERR14: i32 = 14;
pub const ERR15: i32 = 15;
pub const ERR16: i32 = 16;
pub const ERR17: i32 = 17;
pub const ERR18: i32 = 18;
pub const ERR19: i32 = 19;
pub const ERR20: i32 = 20;
pub const ERR21: i32 = 21;
pub const ERR22: i32 = 22;
pub const ERR23: i32 = 23;
pub const ERR24: i32 = 24;
pub const ERR25: i32 = 25;
pub const ERR26: i32 = 26;
pub const ERR27: i32 = 27;
pub const ERR28: i32 = 28;
pub const ERR29: i32 = 29;
pub const ERR30: i32 = 30;
pub const ERR31: i32 = 31;
pub const ERR32: i32 = 32;
pub const ERR33: i32 = 33;
pub const ERR34: i32 = 34;
pub const ERR35: i32 = 35;
pub const ERR36: i32 = 36;
pub const ERR37: i32 = 37;
pub const ERR38: i32 = 38;
pub const ERR39: i32 = 39;
pub const ERR40: i32 = 40;
pub const ERR41: i32 = 41;
pub const ERR42: i32 = 42;
pub const ERR43: i32 = 43;
pub const ERR44: i32 = 44;
pub const ERR45: i32 = 45;
pub const ERR46: i32 = 46;
pub const ERR47: i32 = 47;
pub const ERR48: i32 = 48;
pub const ERR49: i32 = 49;
pub const ERR50: i32 = 50;
pub const ERR51: i32 = 51;
pub const ERR52: i32 = 52;
pub const ERR53: i32 = 53;
pub const ERR54: i32 = 54;
pub const ERR55: i32 = 55;
pub const ERR56: i32 = 56;
pub const ERR57: i32 = 57;
pub const ERR58: i32 = 58;
pub const ERR59: i32 = 59;
pub const ERR60: i32 = 60;
pub const ERR61: i32 = 61;
pub const ERR62: i32 = 62;
pub const ERR63: i32 = 63;
pub const ERR64: i32 = 64;
pub const ERR65: i32 = 65;
pub const ERR66: i32 = 66;
pub const ERR67: i32 = 67;
pub const ERR68: i32 = 68;
pub const ERR69: i32 = 69;
pub const ERR70: i32 = 70;
pub const ERR71: i32 = 71;
pub const ERR72: i32 = 72;
pub const ERR73: i32 = 73;
pub const ERR74: i32 = 74;
pub const ERR75: i32 = 75;
pub const ERR76: i32 = 76;
pub const ERR77: i32 = 77;
pub const ERR78: i32 = 78;
pub const ERR79: i32 = 79;
pub const ERR80: i32 = 80;
pub const ERR81: i32 = 81;
pub const ERR82: i32 = 82;
pub const ERR83: i32 = 83;
pub const ERR84: i32 = 84;
pub const ERR85: i32 = 85;
pub const ERR86: i32 = 86;
pub const ERR87: i32 = 87;
pub const ERRCOUNT: i32 = 88;

// ---- Bits in ctypes and offsets in the tables ----

pub const CTYPE_SPACE: u8 = 0x01;
pub const CTYPE_LETTER: u8 = 0x02;
pub const CTYPE_DIGIT: u8 = 0x04;
pub const CTYPE_XDIGIT: u8 = 0x08;
pub const CTYPE_WORD: u8 = 0x10;
pub const CTYPE_META: u8 = 0x80;

pub const CBIT_SPACE: usize = 0;
pub const CBIT_XDIGIT: usize = 32;
pub const CBIT_DIGIT: usize = 64;
pub const CBIT_UPPER: usize = 96;
pub const CBIT_LOWER: usize = 128;
pub const CBIT_WORD: usize = 160;
pub const CBIT_GRAPH: usize = 192;
pub const CBIT_PRINT: usize = 224;
pub const CBIT_PUNCT: usize = 256;
pub const CBIT_CNTRL: usize = 288;
pub const CBIT_LENGTH: usize = 320;

pub const LCC_OFFSET: usize = 0;
pub const FCC_OFFSET: usize = 256;
pub const CBITS_OFFSET: usize = 512;
pub const CTYPES_OFFSET: usize = CBITS_OFFSET + CBIT_LENGTH;
pub const TABLES_LENGTH: usize = CTYPES_OFFSET + 256;

/// Character tables: C's `const unsigned char *tables`.
pub type Tables = [u8; TABLES_LENGTH];

// ---- The compiled pattern ----

/// `sizeof(REAL_PCRE)`: 24 bytes of u32, 24 of u16, then the `tables` and
/// `nullpad` pointers, as C lays it out on this host.
pub const REAL_PCRE_SIZE: usize = 48 + 2 * core::mem::size_of::<usize>();

/// `REAL_PCRE` and the block that follows it: C mallocs one block of
/// `size` bytes, the header, then `name_count` entries of
/// `name_entry_size` bytes, then the code. `data` is that block without
/// the header, so `name_table_offset` (`REAL_PCRE_SIZE`) maps to `data[0]`
/// and code offsets index [`Pcre::code`] as they index C's `codestart`.
/// Bytes past the final OP_END are C's over-estimate and are zero here.
#[derive(Clone, Debug)]
pub struct Pcre {
    pub magic_number: u32,
    pub size: u32,
    pub options: u32,
    pub flags: u32,
    pub limit_match: u32,
    pub limit_recursion: u32,
    pub first_char: u16,
    pub req_char: u16,
    pub max_lookbehind: u16,
    pub top_bracket: u16,
    pub top_backref: u16,
    pub name_table_offset: u16,
    pub name_entry_size: u16,
    pub name_count: u16,
    pub ref_count: u16,
    /// None for the default tables, as C stores NULL.
    pub tables: Option<&'static Tables>,
    pub data: Vec<u8>,
}

impl Pcre {
    /// `re->tables`, or the default tables when it is NULL.
    pub fn tables(&self) -> &'static Tables {
        self.tables.unwrap_or(&DEFAULT_TABLES)
    }

    /// `(pcre_uchar *)re + re->name_table_offset`.
    pub fn name_table(&self) -> &[u8] {
        &self.data[..self.name_count as usize * self.name_entry_size as usize]
    }

    /// `codestart`: everything after the name table.
    pub fn code(&self) -> &[u8] {
        &self.data[self.name_count as usize * self.name_entry_size as usize..]
    }
}
