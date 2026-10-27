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

use std::cell::{Cell, RefCell};
use std::ffi::{c_int, c_long, c_short, c_ulong, c_ushort};

use super::error::CalcError;
use super::value::{SCALC_STRING_SIZE, StackValue};
use crate::runtime::scanf::{self, ScanNum};

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
/// a second live conversion, which glibc counts. Whichever conversion is
/// assigned writes its own type's bytes into the object, as in C.
pub fn sscanf(input: &[u8], fmt: &[u8]) -> Result<StackValue, CalcError> {
    let cc = find_conversion_indicator(fmt).ok_or(CalcError::InvalidFormat)?;
    let num = |v: f64| Ok(StackValue::Double(v));
    // C's `switch (*s)` on `s[-1]`, into `h`, `l`, `j`, `ui`, `ul`, `ps->d`,
    // `f` and `tmpstr`.
    match (fmt[cc], fmt[cc - 1]) {
        (b'd' | b'i', b'h') => num(scanned::<c_short>(input, fmt)?.into()),
        (b'd' | b'i', b'l') => num(scanned::<c_long>(input, fmt)? as f64),
        (b'd' | b'i', _) => num(scanned::<c_int>(input, fmt)?.into()),
        (b'o' | b'u' | b'x' | b'X', b'h') => num(scanned::<c_ushort>(input, fmt)?.into()),
        (b'o' | b'u' | b'x' | b'X', _) => num(scanned::<c_ulong>(input, fmt)? as f64),
        (b'e' | b'E' | b'f' | b'g' | b'G', b'l') => num(scanned::<f64>(input, fmt)?),
        (b'e' | b'E' | b'f' | b'g' | b'G', _) => num(scanned::<f32>(input, fmt)?.into()),
        (b'c' | b'[' | b's', _) => {
            // C's is `char[1000]`; `strNcpy(ps->s, tmpstr, SCALC_STRING_SIZE-1)`
            // (`:1684`) reads no further than this.
            let tmpstr = RefCell::new([0; SCALC_STRING_SIZE]);
            if scanf::sscanf(input, fmt, &[&tmpstr, &()]) != 1 {
                return Err(CalcError::InvalidFormat);
            }
            Ok(StackValue::str_ncpy(tmpstr.into_inner()))
        }
        // `default`, `p`, `w`, `n` and `$` all `return(-1)` before any input
        // is looked at.
        _ => Err(CalcError::InvalidFormat),
    }
}

/// The object scanned from zero. C zeroes `h`, `l` and `j` per SSCANF but
/// leaves `ui`, `f` and `tmpstr` stale and `ul` holding an earlier op's
/// bytes, which a narrower conversion leaves in place; that is not copied.
fn scanned<T: ScanNum + Default>(input: &[u8], fmt: &[u8]) -> Result<T, CalcError> {
    let object = Cell::new(T::default());
    if scanf::sscanf(input, fmt, &[&object, &()]) != 1 {
        return Err(CalcError::InvalidFormat);
    }
    Ok(object.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn num(v: f64) -> Option<StackValue> {
        Some(StackValue::Double(v))
    }

    fn text(s: &[u8]) -> Option<StackValue> {
        Some(StackValue::str_ncpy(s))
    }

    /// Against glibc 2.43 running sCalc's SSCANF case verbatim on zeroed
    /// objects: (input, format, result), `None` being CALC_ALARM.
    fn check(rows: &[(&[u8], &str, Option<StackValue>)]) {
        let failed: Vec<String> = rows
            .iter()
            .filter_map(|(input, fmt, want)| {
                let got = sscanf(input, fmt.as_bytes()).ok();
                (&got != want)
                    .then(|| format!("{fmt} \"{}\": C {want:?}, rs {got:?}", input.escape_ascii()))
            })
            .collect();
        assert!(failed.is_empty(), "{}", failed.join("\n"));
    }

    #[cfg(target_endian = "little")]
    #[test]
    fn a_conversion_stores_its_own_width_into_cs_object() {
        check(&[
            (b"200", "%hhd", num(200.0)),
            (b"128", "%hhd", num(128.0)),
            (b"-1", "%hhd", num(255.0)),
            (b"300", "%hhd", num(44.0)),
            (b"-1", "%hhi", num(255.0)),
            (b"0x1ff", "%hhi", num(255.0)),
            (b"-1", "%hd", num(-1.0)),
            (b"-1", "%hhu", num(255.0)),
            (b"1ff", "%hhx", num(255.0)),
            (b"-1", "%hho", num(255.0)),
            (b"-1", "%hu", num(65535.0)),
            (b"-1", "%d", num(-1.0)),
            (b"-1", "%ld", num(-1.0)),
            (b"-1", "%lld", num(-1.0)),
            (b"-1", "%jd", num(-1.0)),
            (b"-1", "%x", num(4294967295.0)),
            (b"-1", "%lx", num(c_ulong::MAX as f64)),
            (b"-1", "%llx", num(c_ulong::MAX as f64)),
            (b"1.5", "%f", num(1.5)),
            (b"1.5", "%lf", num(1.5)),
            (b"abc", "%c", text(b"a")),
            (b"abc", "%3c", text(b"abc")),
            (b"abc", "%s", text(b"abc")),
            (b"abc1", "%[a-z]", text(b"abc")),
            // The greedy `%%` skip hands the object to a conversion of
            // another type, whose bytes glibc stores into it.
            (b"1.5", "%f%%%d", num(1.5f32.to_bits() as f64)),
            (b"7", "%d%%%f", num(f32::from_bits(7) as f64)),
            (b"ab", "%s%%%d", num(i32::from_le_bytes(*b"ab\0\0") as f64)),
            (b"7", "%d%%%s", text(b"\x07")),
        ]);
    }

    /// `%lc`, `%ls` and `%l[` store `wchar_t`s into `tmpstr`, so `strNcpy`
    /// keeps the first character. In the C locale a byte at or above 0x80 is
    /// a conversion error for `%lc` and `%ls`, and `%l[` leaves its
    /// `wchar_t` unwritten.
    #[cfg(target_endian = "little")]
    #[test]
    fn a_wide_conversion_stores_wchar_t_into_tmpstr() {
        check(&[
            (b"abc", "%lc", text(b"a")),
            (b"abc", "%ls", text(b"a")),
            (b"abc", "%l[a-z]", text(b"a")),
            (b"abc", "%3lc", text(b"a")),
            (b"\xe9", "%lc", None),
            (b"a\xe9b", "%ls", None),
            (b"a\xe9", "%l[a-z]", text(b"a")),
            (b"a\xe9b,", "%l[^,]", text(b"a")),
            (b"\xe9", "%l[^,]", text(b"")),
            (b"a", "%lc%%%d", num(97.0)),
        ]);
    }
}
