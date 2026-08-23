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
//! know silently answered 0. This module is the whole engine — the directive
//! walk, the C99 conversions, and the narrowing table sCalc applies on top.

use super::error::CalcError;
use super::strtod::strtod;
use super::value::StackValue;

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
pub fn sscanf(input: &[u8], fmt: &[u8]) -> Result<StackValue, CalcError> {
    let cc = find_conversion_indicator(fmt).ok_or(CalcError::InvalidFormat)?;
    // C's `switch (*s)`: `default`, `p`, `w`, `n` and `$` all `return(-1)`
    // before any input is looked at.
    if !matches!(
        fmt[cc],
        b'd' | b'i'
            | b'o'
            | b'u'
            | b'x'
            | b'X'
            | b'e'
            | b'E'
            | b'f'
            | b'g'
            | b'G'
            | b'c'
            | b'['
            | b's'
    ) {
        return Err(CalcError::InvalidFormat);
    }

    let mut p = 0usize; // input cursor
    let mut i = 0usize; // format cursor

    while i < fmt.len() {
        let c = fmt[i];
        // A whitespace directive matches any run of whitespace, including none.
        if crate::runtime::stdlib::c_isspace(c as char) {
            p += leading_whitespace(&input[p..]);
            i += 1;
            continue;
        }
        if c != b'%' {
            if input.get(p) != Some(&c) {
                return Err(CalcError::InvalidFormat);
            }
            p += 1;
            i += 1;
            continue;
        }
        if fmt.get(i + 1) == Some(&b'%') {
            // glibc skips leading whitespace before the literal `%`; compiled
            // sCalc agrees (`SSCANF(' %42','%%%d')` is 42).
            p += leading_whitespace(&input[p..]);
            if input.get(p) != Some(&b'%') {
                return Err(CalcError::InvalidFormat);
            }
            p += 1;
            i += 2;
            continue;
        }

        let spec = Spec::parse(fmt, i).ok_or(CalcError::InvalidFormat)?;
        // A suppressed conversion still has to MATCH; failing one stops sscanf
        // with `i == 0`, which is C's -1.
        let value = spec.convert(input, &mut p)?;
        if !spec.suppress {
            if spec.conv_at != cc {
                // Only reachable through the greedy `%%` skip (`"%d %% %s"`),
                // where C hands its single output object to the WRONG directive
                // and writes an int through a `char *`. That is UB; refuse the
                // format instead of reproducing it.
                return Err(CalcError::InvalidFormat);
            }
            return Ok(value);
        }
        i = spec.end;
    }
    // The format ran out with nothing assigned: C's `i != 1`.
    Err(CalcError::InvalidFormat)
}

fn leading_whitespace(s: &[u8]) -> usize {
    s.iter()
        .take_while(|b| crate::runtime::stdlib::c_isspace(**b as char))
        .count()
}

/// One conversion specification: `%`, optional `*`, optional width, optional
/// length modifiers, then the conversion character.
struct Spec {
    suppress: bool,
    width: Option<usize>,
    /// C's `s[-1]`: the byte in front of the conversion character, whatever it
    /// is. `%2d` therefore has modifier `'2'`, i.e. no modifier at all.
    modifier: Option<u8>,
    conv: u8,
    conv_at: usize,
    set: Option<CharSet>,
    end: usize,
}

impl Spec {
    fn parse(f: &[u8], start: usize) -> Option<Self> {
        let mut j = start + 1;
        let suppress = f.get(j) == Some(&b'*');
        if suppress {
            j += 1;
        }
        let digits = j;
        while f.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
        let width = if j > digits {
            std::str::from_utf8(&f[digits..j]).ok()?.parse().ok()
        } else {
            None
        };
        while matches!(
            f.get(j),
            Some(b'h' | b'l' | b'L' | b'j' | b'z' | b't' | b'q')
        ) {
            j += 1;
        }
        let conv = *f.get(j)?;
        if !CONV.contains(&conv) {
            return None;
        }
        let (set, end) = if conv == b'[' {
            let e = skip_past_conversion(f, j)?;
            (Some(CharSet::parse(&f[j + 1..e - 1])), e)
        } else {
            (None, j + 1)
        };
        Some(Spec {
            suppress,
            width,
            modifier: f.get(j.wrapping_sub(1)).copied(),
            conv,
            conv_at: j,
            set,
            end,
        })
    }

    /// Match this directive against `input[*p..]`, advancing `p`. `Err` is a
    /// matching failure or input exhaustion — both leave C's `i` short of 1.
    ///
    /// A string result is bounded at 38 bytes, not 39: C scans into `tmpstr` and
    /// then copies with `strNcpy(ps->s, tmpstr, SCALC_STRING_SIZE-1)` (`:1684`).
    fn convert(&self, input: &[u8], p: &mut usize) -> Result<StackValue, CalcError> {
        match self.conv {
            // `%c` and `%[` do NOT skip leading whitespace.
            b'c' => {
                let n = self.width.unwrap_or(1);
                let end = p.checked_add(n).filter(|e| *e <= input.len());
                let end = end.ok_or(CalcError::InvalidFormat)?;
                let text = input[*p..end].to_vec();
                *p = end;
                Ok(StackValue::str_ncpy(text))
            }
            b'[' => {
                let set = self.set.as_ref().ok_or(CalcError::InvalidFormat)?;
                let max = self.width.unwrap_or(usize::MAX);
                let mut n = 0;
                while n < max && input.get(*p + n).is_some_and(|c| set.contains(*c)) {
                    n += 1;
                }
                if n == 0 {
                    return Err(CalcError::InvalidFormat);
                }
                let text = input[*p..*p + n].to_vec();
                *p += n;
                Ok(StackValue::str_ncpy(text))
            }
            b's' => {
                *p += leading_whitespace(&input[*p..]);
                let max = self.width.unwrap_or(usize::MAX);
                let mut n = 0;
                while n < max
                    && input
                        .get(*p + n)
                        .is_some_and(|c| !crate::runtime::stdlib::c_isspace(*c as char))
                {
                    n += 1;
                }
                if n == 0 {
                    return Err(CalcError::InvalidFormat);
                }
                let text = input[*p..*p + n].to_vec();
                *p += n;
                Ok(StackValue::str_ncpy(text))
            }
            b'e' | b'E' | b'f' | b'g' | b'G' => {
                *p += leading_whitespace(&input[*p..]);
                let limit = self
                    .width
                    .map_or(input.len(), |w| (*p + w).min(input.len()));
                let r = strtod(&input[*p..limit]);
                if r.len == 0 {
                    return Err(CalcError::InvalidFormat);
                }
                *p += r.len;
                // C: `%lf` writes into a `double`, bare `%f` into a `float`
                // that is then widened — `SSCANF('0.1','%f')` really is
                // 0.10000000149011612.
                Ok(StackValue::Double(if self.modifier == Some(b'l') {
                    r.value
                } else {
                    r.value as f32 as f64
                }))
            }
            b'd' | b'i' | b'o' | b'u' | b'x' | b'X' => self.integer(input, p),
            // `p`, `w`, `n`, `$` are refused by the caller before we get here.
            _ => Err(CalcError::InvalidFormat),
        }
    }

    fn integer(&self, input: &[u8], p: &mut usize) -> Result<StackValue, CalcError> {
        *p += leading_whitespace(&input[*p..]);
        let limit = self
            .width
            .map_or(input.len(), |w| (*p + w).min(input.len()));
        let s = &input[*p..limit];

        let mut k = 0;
        let negative = match s.first() {
            Some(b'-') => {
                k = 1;
                true
            }
            Some(b'+') => {
                k = 1;
                false
            }
            _ => false,
        };
        let hex_prefix = s.len() > k + 2
            && s[k] == b'0'
            && (s[k + 1] | 0x20) == b'x'
            && s[k + 2].is_ascii_hexdigit();
        let base = match self.conv {
            b'x' | b'X' => {
                if hex_prefix {
                    k += 2;
                }
                16
            }
            b'o' => 8,
            b'i' => {
                if hex_prefix {
                    k += 2;
                    16
                } else if s.get(k) == Some(&b'0') {
                    8
                } else {
                    10
                }
            }
            _ => 10,
        };

        let first_digit = k;
        let mut acc: i128 = 0;
        while let Some(d) = s.get(k).and_then(|c| (*c as char).to_digit(base)) {
            acc = acc.wrapping_mul(base as i128).wrapping_add(d as i128);
            k += 1;
        }
        if k == first_digit {
            return Err(CalcError::InvalidFormat);
        }
        *p += k;

        let v = if negative { -acc } else { acc };
        // sCalc's narrowing table: the output object's type, and hence the
        // width the C library's sscanf writes through it.
        let d = match self.conv {
            // `short h` / `long l` / `int j`.
            b'd' | b'i' => match self.modifier {
                Some(b'h') => v as i16 as f64,
                Some(b'l') => v as i64 as f64,
                _ => v as i32 as f64,
            },
            // `unsigned short ui`, else `unsigned long ul = 0UL` — into which a
            // bare `%u`/`%x` still writes only 4 bytes, so the value is a u32.
            _ => match self.modifier {
                Some(b'h') => v as u16 as f64,
                Some(b'l') => v as u64 as f64,
                _ => v as u32 as f64,
            },
        };
        Ok(StackValue::Double(d))
    }
}

/// A `%[...]` scanset.
struct CharSet {
    negated: bool,
    ranges: Vec<(u8, u8)>,
}

impl CharSet {
    /// `body` is what lies between the brackets, so a leading `]` is already a
    /// plain member and needs no special case.
    fn parse(mut body: &[u8]) -> Self {
        let negated = body.first() == Some(&b'^');
        if negated {
            body = &body[1..];
        }
        let mut ranges = Vec::new();
        let mut i = 0;
        while i < body.len() {
            if i + 2 < body.len() && body[i + 1] == b'-' {
                ranges.push((body[i], body[i + 2]));
                i += 3;
            } else {
                ranges.push((body[i], body[i]));
                i += 1;
            }
        }
        CharSet { negated, ranges }
    }

    fn contains(&self, c: u8) -> bool {
        self.ranges.iter().any(|(lo, hi)| c >= *lo && c <= *hi) != self.negated
    }
}
