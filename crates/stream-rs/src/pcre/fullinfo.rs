//! `pcre_fullinfo.c` for this build. StreamDevice never studies a pattern
//! and passes a NULL `pcre_extra`, so the study and JIT answers are C's
//! answers for a NULL extra.

use super::chartables::DEFAULT_TABLES;
use super::internal::*;

/// A `pcre_fullinfo` answer: C writes through `where` as the type the
/// request names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InfoValue<'a> {
    /// `unsigned long int`: PCRE_INFO_OPTIONS.
    ULong(u64),
    /// `size_t`: PCRE_INFO_SIZE, PCRE_INFO_STUDYSIZE, PCRE_INFO_JITSIZE.
    Size(usize),
    Int(i32),
    /// `pcre_uint32`: the *CHARACTER and *LIMIT requests.
    U32(u32),
    /// PCRE_INFO_FIRSTTABLE: NULL without study data.
    FirstTable(Option<&'a [u8; 32]>),
    /// PCRE_INFO_NAMETABLE: `(pcre_uchar *)re + re->name_table_offset`.
    NameTable(&'a [u8]),
    /// PCRE_INFO_DEFAULT_TABLES.
    Tables(&'static Tables),
}

/// `pcre_fullinfo(re, NULL, what, where)`: `Err` is C's negative return,
/// `re` None is C's NULL pattern.
pub fn fullinfo(re: Option<&Pcre>, what: i32) -> Result<InfoValue<'_>, i32> {
    use InfoValue::*;
    let Some(re) = re else {
        return Err(PCRE_ERROR_NULL);
    };
    if re.magic_number != MAGIC_NUMBER {
        return Err(if re.magic_number == REVERSED_MAGIC_NUMBER {
            PCRE_ERROR_BADENDIANNESS
        } else {
            PCRE_ERROR_BADMAGIC
        });
    }
    if re.flags & PCRE_MODE == 0 {
        return Err(PCRE_ERROR_BADMODE);
    }
    let flag = |f: u32| re.flags & f != 0;
    Ok(match what {
        PCRE_INFO_OPTIONS => ULong((re.options & PUBLIC_COMPILE_OPTIONS) as u64),
        PCRE_INFO_SIZE => Size(re.size as usize),
        PCRE_INFO_STUDYSIZE | PCRE_INFO_JITSIZE => Size(0),
        PCRE_INFO_CAPTURECOUNT => Int(re.top_bracket as i32),
        PCRE_INFO_BACKREFMAX => Int(re.top_backref as i32),
        PCRE_INFO_FIRSTBYTE => Int(if flag(PCRE_FIRSTSET) {
            re.first_char as i32
        } else if flag(PCRE_STARTLINE) {
            -1
        } else {
            -2
        }),
        PCRE_INFO_FIRSTCHARACTER => U32(if flag(PCRE_FIRSTSET) {
            re.first_char as u32
        } else {
            0
        }),
        PCRE_INFO_FIRSTCHARACTERFLAGS => Int(if flag(PCRE_FIRSTSET) {
            1
        } else if flag(PCRE_STARTLINE) {
            2
        } else {
            0
        }),
        PCRE_INFO_FIRSTTABLE => FirstTable(None),
        PCRE_INFO_MINLENGTH => Int(-1),
        PCRE_INFO_JIT => Int(0),
        PCRE_INFO_LASTLITERAL => Int(if flag(PCRE_REQCHSET) {
            re.req_char as i32
        } else {
            -1
        }),
        PCRE_INFO_REQUIREDCHAR => U32(if flag(PCRE_REQCHSET) {
            re.req_char as u32
        } else {
            0
        }),
        PCRE_INFO_REQUIREDCHARFLAGS => Int(flag(PCRE_REQCHSET) as i32),
        PCRE_INFO_NAMEENTRYSIZE => Int(re.name_entry_size as i32),
        PCRE_INFO_NAMECOUNT => Int(re.name_count as i32),
        PCRE_INFO_NAMETABLE => NameTable(re.name_table()),
        PCRE_INFO_DEFAULT_TABLES => Tables(&DEFAULT_TABLES),
        PCRE_INFO_OKPARTIAL => Int(!flag(PCRE_NOPARTIAL) as i32),
        PCRE_INFO_JCHANGED => Int(flag(PCRE_JCHANGED) as i32),
        PCRE_INFO_HASCRORLF => Int(flag(PCRE_HASCRORLF) as i32),
        PCRE_INFO_MAXLOOKBEHIND => Int(re.max_lookbehind as i32),
        PCRE_INFO_MATCHLIMIT => {
            if !flag(PCRE_MLSET) {
                return Err(PCRE_ERROR_UNSET);
            }
            U32(re.limit_match)
        }
        PCRE_INFO_RECURSIONLIMIT => {
            if !flag(PCRE_RLSET) {
                return Err(PCRE_ERROR_UNSET);
            }
            U32(re.limit_recursion)
        }
        PCRE_INFO_MATCH_EMPTY => Int(flag(PCRE_MATCH_EMPTY) as i32),
        _ => return Err(PCRE_ERROR_BADOPTION),
    })
}
