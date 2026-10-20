//! The C string handling the StreamDevice sources lean on: a `const char*`
//! cursor over a zero-tailed buffer, and the `<ctype.h>`, `strtoul`,
//! `strtol` and `sscanf` behaviour the parser's numbers depend on.

/// A `const char*` into a byte buffer.
///
/// A `StreamBuffer` always ends in 0x00 bytes, and the C code reads past the
/// last data byte (a token's line number after its terminator, the
/// terminator of the last string). Reading past the slice therefore yields
/// 0, as the buffer's zero tail does in C.
#[derive(Clone, Copy, Debug)]
pub struct CPtr<'a> {
    s: &'a [u8],
    p: usize,
}

impl<'a> CPtr<'a> {
    pub fn new(s: &'a [u8]) -> Self {
        Self { s, p: 0 }
    }

    pub fn at_offset(s: &'a [u8], p: usize) -> Self {
        Self { s, p }
    }

    /// The whole underlying buffer.
    pub fn base(&self) -> &'a [u8] {
        self.s
    }

    /// Offset of this pointer from the start of the buffer.
    pub fn offset(&self) -> usize {
        self.p
    }

    pub fn set_offset(&mut self, p: usize) {
        self.p = p;
    }

    /// `*p`
    pub fn peek(&self) -> u8 {
        self.at(0)
    }

    /// `p[i]`
    pub fn at(&self, i: usize) -> u8 {
        self.s.get(self.p + i).copied().unwrap_or(0)
    }

    /// `p += n`
    pub fn advance(&mut self, n: usize) {
        self.p += n;
    }

    /// `p + n`
    pub fn plus(&self, n: usize) -> Self {
        Self {
            s: self.s,
            p: self.p + n,
        }
    }

    /// `strlen(p)`
    pub fn strlen(&self) -> usize {
        let rest = self.s.get(self.p..).unwrap_or(&[]);
        rest.iter().position(|&b| b == 0).unwrap_or(rest.len())
    }

    /// The C string at `p`, without its terminator.
    pub fn cstr(&self) -> &'a [u8] {
        let n = self.strlen();
        self.s.get(self.p..self.p + n).unwrap_or(&[])
    }

    /// `getLineNumber(p)`: the native `int` stored after the terminator of
    /// the token at `p`.
    pub fn line_number(&self) -> i32 {
        let at = self.p + self.strlen() + 1;
        let mut b = [0u8; 4];
        for (i, x) in b.iter_mut().enumerate() {
            *x = self.s.get(at + i).copied().unwrap_or(0);
        }
        i32::from_ne_bytes(b)
    }

    /// `p += strlen(p) + 1 + sizeof(int)`: step over a token and its line.
    pub fn skip_token(&mut self) {
        self.p += self.strlen() + 1 + LINE_SIZE;
    }

    /// `strchr(p, c)` as an offset, `c == 0` finding the terminator.
    pub fn strchr(&self, c: u8) -> Option<usize> {
        let n = self.strlen();
        if c == 0 {
            return Some(self.p + n);
        }
        self.cstr().iter().position(|&b| b == c).map(|i| self.p + i)
    }
}

/// `sizeof(int)`, the line number stored after each token.
pub const LINE_SIZE: usize = 4;

/// `strchr(set, c) != NULL` for a C string `set`: the terminator matches 0.
pub fn c_strchr(set: &[u8], c: i32) -> bool {
    let c = c as u8;
    c == 0 || set.contains(&c)
}

/// `isspace` in the C locale.
pub fn c_isspace(c: i32) -> bool {
    matches!(c, 0x20 | 0x09..=0x0d)
}

/// `isalpha` in the C locale.
pub fn c_isalpha(c: i32) -> bool {
    (0..=255).contains(&c) && (c as u8).is_ascii_alphabetic()
}

/// `tolower` in the C locale.
pub fn c_tolower(c: i32) -> i32 {
    if (0..=255).contains(&c) {
        (c as u8).to_ascii_lowercase() as i32
    } else {
        c
    }
}

fn digit_value(c: u8) -> Option<u32> {
    (c as char).to_digit(36)
}

/// The bytes from `s` on; the zero tail past the slice stops a scan as the
/// slice end does.
fn rest<'a>(s: &CPtr<'a>) -> &'a [u8] {
    s.base().get(s.offset()..).unwrap_or(&[])
}

/// `strtoul(s, &end, base)` with a 64-bit `unsigned long` whatever the target's
/// is, i.e. `strtoull`: the value and the end offset, which stays at `s` when
/// nothing converted.
pub fn c_strtoul(s: CPtr<'_>, base: u32) -> (u64, usize) {
    let (v, end) = epics_base_rs::types::c_parse::strtoull(rest(&s), base);
    (v, s.offset() + end)
}

/// `strtol(s, &end, base)` with a 64-bit `long`, i.e. `strtoll`.
pub fn c_strtol(s: CPtr<'_>, base: u32) -> (i64, usize) {
    let (v, end) = epics_base_rs::types::c_parse::strtoll(rest(&s), base);
    (v, s.offset() + end)
}

/// `sscanf(s, "%<width>x%n", &c, &n)` as glibc reads it: `None` when no
/// conversion was made, else the 32-bit value and the bytes consumed,
/// skipped leading whitespace included.
pub fn c_sscanf_hex(s: CPtr<'_>, width: usize) -> Option<(u32, usize)> {
    let mut i = 0;
    while c_isspace(s.at(i) as i32) {
        i += 1;
    }
    let mut w = width;
    let mut neg = false;
    if w > 0 && (s.at(i) == b'+' || s.at(i) == b'-') {
        neg = s.at(i) == b'-';
        i += 1;
        w -= 1;
    }
    let mut value: u32 = 0;
    let mut any = false;
    if w > 0 && s.at(i) == b'0' {
        // glibc takes "0x" as a prefix inside the field width.
        any = true;
        i += 1;
        w -= 1;
        if w > 0 && (s.at(i) | 0x20) == b'x' {
            i += 1;
            w -= 1;
        }
    }
    while w > 0 {
        let Some(d) = digit_value(s.at(i)).filter(|&d| d < 16) else {
            break;
        };
        any = true;
        value = value.wrapping_mul(16).wrapping_add(d);
        i += 1;
        w -= 1;
    }
    if !any {
        return None;
    }
    Some((if neg { value.wrapping_neg() } else { value }, i))
}

/// `sscanf(s, "%<width>o%n")` or `"%<width>u%n"` on a string that starts with
/// a digit of the base, as the parser only calls it: up to `width` digits.
pub fn c_sscanf_digits(s: CPtr<'_>, width: usize, base: u32) -> (u32, usize) {
    let mut value: u32 = 0;
    let mut i = 0;
    while i < width {
        let Some(d) = digit_value(s.at(i)).filter(|&d| d < base) else {
            break;
        };
        value = value * base + d;
        i += 1;
    }
    (value, i)
}

/// printf `%#o`.
pub fn fmt_alt_octal(v: u32) -> String {
    if v == 0 {
        "0".to_string()
    } else {
        format!("0{v:o}")
    }
}

/// printf `%#04x`.
pub fn fmt_alt_hex04(v: u32) -> String {
    if v == 0 {
        "0000".to_string()
    } else {
        format!("{:#04x}", v)
    }
}

/// `sscanf(s, "%d%n")` as glibc reads it: `(int)strtol(s, &end, 10)`.
/// `None` when no conversion was made, else the value and the bytes
/// consumed, skipped leading whitespace included.
pub fn c_sscanf_int(s: CPtr<'_>) -> Option<(i32, usize)> {
    let (v, end) = c_strtol(s, 10);
    (end != s.offset()).then(|| (v as i32, end - s.offset()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ul(s: &[u8], base: u32) -> (u64, usize) {
        c_strtoul(CPtr::new(s), base)
    }

    #[test]
    fn strtoul_matches_glibc_on_prefixes_signs_and_overflow() {
        assert_eq!(ul(b"0x1F", 0), (31, 4));
        assert_eq!(ul(b"017", 0), (15, 3));
        assert_eq!(ul(b"0x", 0), (0, 1));
        assert_eq!(ul(b"0xg", 0), (0, 1));
        assert_eq!(ul(b"  12z", 10), (12, 4));
        assert_eq!(ul(b"-5", 10), (u64::MAX - 4, 2));
        assert_eq!(ul(b"", 0), (0, 0));
        assert_eq!(ul(b"  -", 0), (0, 0));
        assert_eq!(ul(b"99999999999999999999999", 10), (u64::MAX, 23));
        assert_eq!(ul(b"08", 0), (0, 1));
    }

    #[test]
    fn strtol_clamps_at_the_long_limits() {
        assert_eq!(c_strtol(CPtr::new(b"-0x80"), 0), (-128, 5));
        assert_eq!(
            c_strtol(CPtr::new(b"99999999999999999999"), 0),
            (i64::MAX, 20)
        );
        assert_eq!(
            c_strtol(CPtr::new(b"-99999999999999999999"), 0),
            (i64::MIN, 21)
        );
    }

    #[test]
    fn strtol_reads_0b_as_binary_like_glibc_with_gnu_source() {
        // g++ defines _GNU_SOURCE, so glibc 2.39's C23 strtol takes `0b`.
        assert_eq!(ul(b"0b101", 0), (5, 5));
        assert_eq!(c_strtol(CPtr::new(b"-0b11"), 2), (-3, 5));
        assert_eq!(ul(b"0b2", 0), (0, 1));
        assert_eq!(
            c_strtol(CPtr::at_offset(b"x -99999999999999999999", 1), 10),
            (i64::MIN, 23)
        );
        assert_eq!(ul(b"x", 0), (0, 0));
        assert_eq!(c_strtoul(CPtr::at_offset(b"ab", 1), 16), (11, 2));
    }

    #[test]
    fn sscanf_hex_takes_whitespace_sign_and_prefix_like_glibc() {
        let h = |s: &[u8]| c_sscanf_hex(CPtr::new(s), 2);
        assert_eq!(h(b"41\""), Some((0x41, 2)));
        assert_eq!(h(b"4g"), Some((4, 1)));
        assert_eq!(h(b" 5"), Some((5, 2)));
        assert_eq!(h(b"-1"), Some((u32::MAX, 2)));
        assert_eq!(h(b"0x5"), Some((0, 2)));
        assert_eq!(h(b"g"), None);
        assert_eq!(h(b"-"), None);
    }

    #[test]
    fn line_number_past_the_end_reads_the_zero_tail() {
        let mut b = b"ab\0".to_vec();
        b.extend_from_slice(&7i32.to_ne_bytes());
        assert_eq!(CPtr::new(&b).line_number(), 7);
        assert_eq!(CPtr::at_offset(&b, b.len()).line_number(), 0);
    }

    #[test]
    fn alternate_forms_match_printf() {
        assert_eq!(fmt_alt_octal(0o777), "0777");
        assert_eq!(fmt_alt_octal(0), "0");
        assert_eq!(fmt_alt_hex04(9), "0x09");
        assert_eq!(fmt_alt_hex04(0x85), "0x85");
        assert_eq!(fmt_alt_hex04(0), "0000");
    }
}
