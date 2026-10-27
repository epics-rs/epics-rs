//! C `sscanf` as sCalc drives it (`sCalcPerform.c:105-190` and `:1635-1690`).
//!
//! sCalc does not implement a scanf *subset*: it hands the user's format
//! straight to the C library's `sscanf` together with exactly ONE output
//! object, whose type it picks from the conversion character and the byte in
//! front of it (`switch (*s)` / `s[-1]`). What makes one object enough is
//! `findConversionIndicator`: it locates the first conversion whose assignment
//! is not suppressed, and it *rejects the whole format* if a second live one
//! follows. So a sCalc SSCANF performs exactly one assignment, and
//! `if (i != 1) return(-1)` turns every matching failure into CALC_ALARM.
//!
//! Porting a subset of the conversions is therefore the defect, not a
//! shortcut: the format is the user's, and any conversion the subset does not
//! know silently answered 0. This module is sCalc's half — the indicator and
//! the object table; the directives and conversions are glibc's, in
//! [`crate::runtime::scanf`].

use std::cell::Cell;
use std::ffi::{c_long, c_ulong};

use super::error::CalcError;
use super::value::StackValue;
use crate::runtime::scanf::{self, ScanTarget, Scanned};

/// C's `strpbrk(s, "pwn$c[deEfgGiousxX")` set — every byte that can end a
/// conversion specification, including the four sCalc then refuses.
const CONV: &[u8] = b"pwn$c[deEfgGiousxX";

/// C `findConversionIndicator` (`sCalcPerform.c:105`): the offset of the one
/// conversion character that will be assigned, or `None` when the format has
/// none, has a second live one, or has a malformed character set.
///
/// Shared with BIN_READ (`sCalcPerform.c:1697`), which is why it lives here
/// rather than inside `sscanf`.
///
/// Two details drive most of its behaviour and neither is obvious:
///
/// * the `%%` skip is **greedy** — `strstr(s, "%%")` jumps past a `%%` found
///   anywhere ahead, skipping any real conversion in front of it. `"%d%%"`
///   therefore has NO conversion indicator at all and SSCANF returns -1.
/// * the second loop returns `NULL` on a second *non-suppressed* conversion,
///   so `"%d %d"` is rejected whatever the input, while `"%d %*d"` is fine.
pub fn find_conversion_indicator(f: &[u8]) -> Option<usize> {
    let mut cc: Option<usize> = None;
    let mut s = 0usize;

    while s < f.len() {
        if let Some(p) = find_sub(&f[s..], b"%%") {
            s += p + 2;
            continue;
        }
        let pct = find_byte(&f[s..], b'%')? + s;
        let c = f[pct..].iter().position(|b| CONV.contains(b))? + pct;
        // C never clears `cc` here, so a format ENDING in a suppressed
        // conversion leaves the suppressed one as the indicator. It still
        // returns -1 later, through `i != 1` — no assignment happened.
        cc = Some(c);
        match find_byte(&f[pct..], b'*') {
            Some(star) if star + pct < c => {
                s = skip_past_conversion(f, c)?;
                continue;
            }
            _ => break,
        }
    }

    let retval = cc?;

    let mut s = retval + 1;
    while s < f.len() {
        if let Some(p) = find_sub(&f[s..], b"%%") {
            s += p + 2;
            continue;
        }
        let Some(pct) = find_byte(&f[s..], b'%').map(|p| p + s) else {
            return Some(retval);
        };
        let Some(c) = f[pct..]
            .iter()
            .position(|b| CONV.contains(b))
            .map(|p| p + pct)
        else {
            return Some(retval);
        };
        match find_byte(&f[pct..], b'*') {
            Some(star) if star + pct < c => s = skip_past_conversion(f, c)?,
            // A second conversion that WOULD be assigned: one output object
            // cannot serve two, so C refuses the format.
            _ => return None,
        }
    }
    Some(retval)
}

/// One past the conversion at `cc`, stepping over a `[` character set in all
/// three of C's spellings (`[..]`, `[]..]`, `[^]..]`). `None` is C's
/// "bad character-set syntax".
fn skip_past_conversion(f: &[u8], cc: usize) -> Option<usize> {
    if f[cc] != b'[' {
        return Some(cc + 1);
    }
    let mut s = cc + 1;
    if f.get(s) == Some(&b']') {
        s = cc + 2;
    } else if f.get(s) == Some(&b'^') && f.get(cc + 2) == Some(&b']') {
        s = cc + 3;
    }
    Some(find_byte(f.get(s..)?, b']')? + s + 1)
}

fn find_byte(h: &[u8], n: u8) -> Option<usize> {
    h.iter().position(|b| *b == n)
}

/// C `strstr`: the empty needle matches at the start of the haystack.
fn find_sub(h: &[u8], n: &[u8]) -> Option<usize> {
    if n.is_empty() {
        return Some(0);
    }
    if n.len() > h.len() {
        return None;
    }
    h.windows(n.len()).position(|w| w == n)
}

/// C `SSCANF` (`sCalcPerform.c:1635`). `Err` is C's `return(-1)`, which
/// sCalcoutRecord raises as CALC_ALARM; the port used to answer `0` and leave
/// the record healthy.
///
/// The format goes to glibc's `sscanf` whole, with the one object C picks
/// and, after it, the argument C did not pass: the greedy `%%` skip can leave
/// a second live conversion, which glibc counts.
pub fn sscanf(input: &[u8], fmt: &[u8]) -> Result<StackValue, CalcError> {
    let cc = find_conversion_indicator(fmt).ok_or(CalcError::InvalidFormat)?;
    // C's `switch (*s)` on `s[-1]`.
    let object = match (fmt[cc], fmt[cc - 1]) {
        (b'd' | b'i', b'h') => Object::Short,
        (b'd' | b'i', b'l') => Object::Long,
        (b'd' | b'i', _) => Object::Int,
        (b'o' | b'u' | b'x' | b'X', b'h') => Object::UShort,
        (b'o' | b'u' | b'x' | b'X', _) => Object::ULong,
        (b'e' | b'E' | b'f' | b'g' | b'G', b'l') => Object::Double,
        (b'e' | b'E' | b'f' | b'g' | b'G', _) => Object::Float,
        (b'c' | b'[' | b's', _) => Object::Text,
        // `default`, `p`, `w`, `n` and `$` all `return(-1)` before any input
        // is looked at.
        _ => return Err(CalcError::InvalidFormat),
    };
    let target = Target {
        object,
        value: Cell::new(None),
    };
    if scanf::sscanf(input, fmt, &[&target, &()]) != 1 {
        return Err(CalcError::InvalidFormat);
    }
    target.value.take().ok_or(CalcError::InvalidFormat)
}

/// sCalc's output object: `h`, `l`, `j`, `ui`, `ul`, `ps->d`, `f`, `tmpstr`.
#[derive(Clone, Copy)]
enum Object {
    Short,
    Long,
    Int,
    UShort,
    ULong,
    Double,
    Float,
    Text,
}

struct Target {
    object: Object,
    value: Cell<Option<StackValue>>,
}

impl ScanTarget for Target {
    fn store(&self, value: Scanned<'_>) {
        let value = match (self.object, value) {
            (Object::Short, Scanned::Int(v)) => StackValue::Double(v as i16 as f64),
            (Object::Long, Scanned::Int(v)) => StackValue::Double(v as c_long as f64),
            (Object::Int, Scanned::Int(v)) => StackValue::Double(v as i32 as f64),
            (Object::UShort, Scanned::UInt(v)) => StackValue::Double(v as u16 as f64),
            (Object::ULong, Scanned::UInt(v)) => StackValue::Double(v as c_ulong as f64),
            (Object::Double, Scanned::Double(v)) => StackValue::Double(v),
            (Object::Float, Scanned::Double(v)) => StackValue::Double(v as f32 as f64),
            // `strNcpy(ps->s, tmpstr, SCALC_STRING_SIZE-1)` (`:1684`).
            (Object::Text, Scanned::Str(b) | Scanned::Chars(b)) => StackValue::str_ncpy(b),
            // The greedy `%%` skip handed the object to a conversion of
            // another kind: C puns it through the vararg (UB), which is not
            // copied; the object stays unset and SSCANF fails.
            _ => return,
        };
        self.value.set(Some(value));
    }
}
