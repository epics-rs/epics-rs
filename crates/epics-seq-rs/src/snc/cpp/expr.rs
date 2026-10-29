//! `#if` expressions, after cpplib's `expr.cc` and the constant parts of
//! `charset.cc`: `intmax_t`/`uintmax_t` arithmetic, 64 bits on the build
//! host as on every EPICS host target.

use super::engine::Engine;
use super::lex::{Kind, P, Prefix, Tok};

#[derive(Clone, Copy)]
struct Num {
    v: u64,
    /// Of unsigned type.
    u: bool,
}

impl Num {
    fn bool(b: bool) -> Num {
        Num {
            v: b as u64,
            u: false,
        }
    }
}

struct Parser {
    peek: Option<Tok>,
    /// Tokens the operator loop read, for the `#if !defined X` guard.
    lex_count: u32,
    saw_leading_not: bool,
    /// Inside an operand that short-circuiting leaves unevaluated.
    skip_eval: u32,
}

fn prec(t: &Tok) -> Option<u8> {
    let Kind::Punct(p) = t.kind else {
        return None;
    };
    Some(match p {
        P::OrOr => 1,
        P::AndAnd => 2,
        P::Or => 3,
        P::Xor => 4,
        P::And => 5,
        P::EqEq | P::NotEq => 6,
        P::Less | P::Greater | P::LessEq | P::GreaterEq => 7,
        P::Lshift | P::Rshift => 8,
        P::Plus | P::Minus => 9,
        P::Mult | P::Div | P::Mod => 10,
        _ => return None,
    })
}

/// Tokens that start an operand.
fn starts_operand(t: &Tok) -> bool {
    matches!(
        t.kind,
        Kind::Number
            | Kind::Name
            | Kind::Char(_)
            | Kind::Punct(P::Hash | P::OpenParen | P::Not | P::Compl)
    )
}

impl Engine {
    /// `_cpp_parse_expr`: the value of an `#if`/`#elif` line.
    pub(super) fn parse_expr(&mut self, is_if: bool) -> bool {
        self.mi_ind_cmacro = None;
        let mut p = Parser {
            peek: None,
            lex_count: 0,
            saw_leading_not: false,
            skip_eval: 0,
        };
        let first = self.expr_peek(&mut p).clone();
        if first.kind == Kind::Eof {
            let d = if is_if { "#if" } else { "#elif" };
            self.error(first.loc, format!("{d} with no expression"));
            return false;
        }
        let Some(v) = self.comma(&mut p) else {
            return false;
        };
        let t = self.expr_next(&mut p);
        if t.kind != Kind::Eof {
            let msg = if t.is(P::CloseParen) {
                "missing '(' in expression".into()
            } else if t.is(P::Colon) {
                "':' without preceding '?'".into()
            } else if starts_operand(&t) {
                format!(
                    "missing binary operator before token \"{}\"",
                    t.text_lossy()
                )
            } else {
                format!(
                    "token \"{}\" is not valid in preprocessor expressions",
                    t.text_lossy()
                )
            };
            self.error(t.loc, msg);
            return false;
        }
        if self.mi_ind_cmacro.is_some() && !(p.saw_leading_not && p.lex_count == 3) {
            self.mi_ind_cmacro = None;
        }
        v.v != 0
    }

    fn expr_next(&mut self, p: &mut Parser) -> Tok {
        if let Some(t) = p.peek.take() {
            return t;
        }
        loop {
            let t = self.get_token();
            if t.is_padding() {
                continue;
            }
            p.lex_count += 1;
            if t.is(P::Not) {
                p.saw_leading_not = p.lex_count == 1;
            }
            return t;
        }
    }

    fn expr_peek<'p>(&mut self, p: &'p mut Parser) -> &'p Tok {
        if p.peek.is_none() {
            let t = self.expr_next(p);
            p.peek = Some(t);
        }
        p.peek.as_ref().expect("peeked")
    }

    fn comma(&mut self, p: &mut Parser) -> Option<Num> {
        let mut v = self.cond(p)?;
        while self.expr_peek(p).is(P::Comma) {
            self.expr_next(p);
            v = self.cond(p)?;
        }
        Some(v)
    }

    fn cond(&mut self, p: &mut Parser) -> Option<Num> {
        let c = self.binary(p, 1)?;
        if !self.expr_peek(p).is(P::Query) {
            return Some(c);
        }
        self.expr_next(p);
        let taken = c.v != 0;
        p.skip_eval += !taken as u32;
        let a = self.comma(p)?;
        p.skip_eval -= !taken as u32;
        let colon = self.expr_next(p);
        if !colon.is(P::Colon) {
            self.error(colon.loc, "'?' without following ':'".into());
            return None;
        }
        p.skip_eval += taken as u32;
        let b = self.cond(p)?;
        p.skip_eval -= taken as u32;
        Some(Num {
            v: if taken { a.v } else { b.v },
            u: a.u || b.u,
        })
    }

    fn binary(&mut self, p: &mut Parser, min: u8) -> Option<Num> {
        let mut lhs = self.unary(p)?;
        while let Some(pr) = prec(self.expr_peek(p)).filter(|&pr| pr >= min) {
            let op = self.expr_next(p);
            let skip = match op.kind {
                Kind::Punct(P::OrOr) => lhs.v != 0,
                Kind::Punct(P::AndAnd) => lhs.v == 0,
                _ => false,
            };
            p.skip_eval += skip as u32;
            let rhs = self.binary(p, pr + 1)?;
            p.skip_eval -= skip as u32;
            lhs = self.apply(p, &op, lhs, rhs);
        }
        Some(lhs)
    }

    fn unary(&mut self, p: &mut Parser) -> Option<Num> {
        let t = self.expr_next(p);
        match t.kind {
            Kind::Punct(P::Plus) => self.unary(p),
            Kind::Punct(P::Minus) => {
                let n = self.unary(p)?;
                if !n.u && n.v == i64::MIN as u64 && p.skip_eval == 0 {
                    // cpplib reduces once it has read the next operator.
                    self.expr_peek(p);
                    self.overflow_warning();
                }
                Some(Num {
                    v: n.v.wrapping_neg(),
                    u: n.u,
                })
            }
            Kind::Punct(P::Compl) => {
                let n = self.unary(p)?;
                Some(Num { v: !n.v, u: n.u })
            }
            Kind::Punct(P::Not) => {
                let n = self.unary(p)?;
                Some(Num::bool(n.v == 0))
            }
            Kind::Punct(P::OpenParen) => {
                if self.expr_peek(p).is(P::CloseParen) {
                    let c = self.expr_next(p);
                    self.error(c.loc, "missing expression between '(' and ')'".into());
                    return None;
                }
                let v = self.comma(p)?;
                let c = self.expr_next(p);
                if !c.is(P::CloseParen) {
                    self.error(c.loc, "missing ')' in expression".into());
                    return None;
                }
                Some(v)
            }
            Kind::Number => self.eval_number(&t),
            Kind::Char(prefix) => self.eval_char(&t, prefix),
            Kind::Name if &*t.text == b"defined" => Some(self.parse_defined()),
            Kind::Name => Some(Num { v: 0, u: false }),
            Kind::Punct(P::Hash) => {
                self.error(t.loc, "assertions are not supported".into());
                None
            }
            Kind::Eof => {
                self.error(t.loc, "operator has no right operand".into());
                None
            }
            _ if prec(&t).is_some() || t.is(P::Query) || t.is(P::Colon) || t.is(P::Comma) => {
                self.error(
                    t.loc,
                    format!("operator '{}' has no left operand", t.text_lossy()),
                );
                None
            }
            _ => {
                self.error(
                    t.loc,
                    format!(
                        "token \"{}\" is not valid in preprocessor expressions",
                        t.text_lossy()
                    ),
                );
                None
            }
        }
    }

    fn apply(&mut self, p: &Parser, op: &Tok, a: Num, b: Num) -> Num {
        let u = a.u || b.u;
        let (x, y) = (a.v as i64, b.v as i64);
        let Kind::Punct(o) = op.kind else {
            unreachable!("binary operator")
        };
        let mut overflowed = false;
        let r = match o {
            P::Plus => {
                overflowed = !u && x.checked_add(y).is_none();
                Num {
                    v: a.v.wrapping_add(b.v),
                    u,
                }
            }
            P::Minus => {
                overflowed = !u && x.checked_sub(y).is_none();
                Num {
                    v: a.v.wrapping_sub(b.v),
                    u,
                }
            }
            P::Mult => {
                overflowed = !u && x.checked_mul(y).is_none();
                Num {
                    v: a.v.wrapping_mul(b.v),
                    u,
                }
            }
            P::Div | P::Mod => {
                if b.v == 0 {
                    if p.skip_eval == 0 {
                        self.error(op.loc, "division by zero in #if".into());
                    }
                    return a;
                }
                let div = o == P::Div;
                let v = match (u, div) {
                    (true, true) => a.v / b.v,
                    (true, false) => a.v % b.v,
                    (false, true) => {
                        overflowed = x.checked_div(y).is_none();
                        x.wrapping_div(y) as u64
                    }
                    (false, false) => x.wrapping_rem(y) as u64,
                };
                Num { v, u }
            }
            P::Lshift | P::Rshift => {
                let (r, o) = shift(a, b, o == P::Lshift);
                overflowed = o;
                r
            }
            P::Less | P::Greater | P::LessEq | P::GreaterEq => {
                let ge = if u { a.v >= b.v } else { x >= y };
                Num::bool(match o {
                    P::GreaterEq => ge,
                    P::Less => !ge,
                    P::Greater => ge && a.v != b.v,
                    _ => !ge || a.v == b.v,
                })
            }
            P::EqEq => Num::bool(a.v == b.v),
            P::NotEq => Num::bool(a.v != b.v),
            P::And => Num { v: a.v & b.v, u },
            P::Or => Num { v: a.v | b.v, u },
            P::Xor => Num { v: a.v ^ b.v, u },
            P::AndAnd => Num::bool(a.v != 0 && b.v != 0),
            P::OrOr => Num::bool(a.v != 0 || b.v != 0),
            _ => unreachable!("binary operator"),
        };
        if overflowed && p.skip_eval == 0 {
            self.overflow_warning();
        }
        r
    }

    /// At the last token lexed, as cpplib's `cpp_error`; the parser has
    /// read the operator after the operands by then.
    fn overflow_warning(&mut self) {
        self.warning(
            self.last_loc,
            "integer overflow in preprocessor expression".into(),
        );
    }

    /// `parse_defined`: `defined X` or `defined ( X )`, unexpanded.
    fn parse_defined(&mut self) -> Num {
        self.prevent_expansion += 1;
        let mut t = self.get_token();
        let paren = t.is(P::OpenParen);
        if paren {
            t = self.get_token();
        }
        let mut node = None;
        if t.kind == Kind::Name {
            node = Some(t.text.clone());
            if paren && !self.get_token().is(P::CloseParen) {
                self.error(t.loc, "missing ')' after \"defined\"".into());
                node = None;
            }
        } else {
            self.error(t.loc, "operator \"defined\" requires an identifier".into());
        }
        self.prevent_expansion -= 1;
        let Some(node) = node else {
            return Num::bool(false);
        };
        let defined = self.macros.contains_key(&node);
        self.mi_ind_cmacro = Some(node);
        Num::bool(defined)
    }

    /// `cpp_classify_number` and `cpp_interpret_integer`.
    fn eval_number(&mut self, t: &Tok) -> Option<Num> {
        let s = &t.text[..];
        if s.len() == 1 {
            return Some(Num {
                v: (s[0] - b'0') as u64,
                u: false,
            });
        }
        let mut radix = 10u64;
        let mut i = 0;
        if s[0] == b'0' {
            radix = 8;
            i = 1;
            match (s.get(1), s.get(2)) {
                (Some(b'x' | b'X'), Some(&c)) if c == b'.' || c.is_ascii_hexdigit() => {
                    radix = 16;
                    i = 2;
                }
                (Some(b'b' | b'B'), Some(b'0' | b'1')) => {
                    radix = 2;
                    i = 2;
                }
                _ => {}
            }
        }
        let digits = i;
        let mut float = false;
        let mut max_digit = 0;
        while let Some(&c) = s.get(i) {
            if c.is_ascii_digit() || (radix == 16 && c.is_ascii_hexdigit()) {
                max_digit = max_digit.max(hex_value(c));
            } else if c == b'.'
                || (radix <= 10 && matches!(c, b'e' | b'E'))
                || (radix == 16 && matches!(c, b'p' | b'P'))
            {
                float = true;
                break;
            } else {
                break;
            }
            i += 1;
        }
        if float {
            self.error(t.loc, "floating constant in preprocessor expression".into());
            return None;
        }
        if max_digit >= radix {
            let kind = if radix == 2 { "binary" } else { "octal" };
            self.error(
                t.loc,
                format!(
                    "invalid digit \"{}\" in {kind} constant",
                    (b'0' + max_digit as u8) as char
                ),
            );
            return None;
        }
        let Some((mut u, imaginary)) = int_suffix(&s[i..]) else {
            self.error(
                t.loc,
                format!(
                    "invalid suffix \"{}\" on integer constant",
                    String::from_utf8_lossy(&s[i..])
                ),
            );
            return None;
        };
        if imaginary {
            self.error(t.loc, "imaginary number in preprocessor expression".into());
            return None;
        }
        let mut v = 0u64;
        let mut overflow = false;
        for &c in &s[digits..i] {
            let (m, o1) = v.overflowing_mul(radix);
            let (a, o2) = m.overflowing_add(hex_value(c));
            overflow |= o1 || o2;
            v = a;
        }
        if overflow {
            self.warning(t.loc, "integer constant is too large for its type".into());
        } else if !u && (v as i64) < 0 {
            if radix == 10 {
                self.warning(
                    t.loc,
                    "integer constant is so large that it is unsigned".into(),
                );
            }
            u = true;
        }
        Some(Num { v, u })
    }

    /// `cpp_interpret_charconst`, for an x86-64/ARM Linux target: `char`
    /// signed, `wchar_t` a signed 32-bit int.
    fn eval_char(&mut self, t: &Tok, prefix: Prefix) -> Option<Num> {
        let open = t.text.iter().position(|&c| c == b'\'').expect("quote");
        let body = &t.text[open + 1..t.text.len() - 1];
        if body.is_empty() {
            self.error(t.loc, "empty character constant".into());
            return None;
        }
        let width = match prefix {
            Prefix::None | Prefix::Utf8 => 8,
            Prefix::U16 => 16,
            Prefix::Wide | Prefix::U32 => 32,
        };
        let mut diags = Vec::new();
        let units = decode(body, width, prefix != Prefix::None, &mut diags);
        let mut failed = false;
        for (error, msg) in diags {
            failed |= error;
            if error {
                self.error(t.loc, msg);
            } else {
                self.warning(t.loc, msg);
            }
        }
        if failed {
            return None;
        }
        if prefix == Prefix::None {
            let mut r = 0u32;
            for &b in &units {
                r = (r << 8) | b;
            }
            let n = units.len();
            if n > 4 {
                self.warning(t.loc, "character constant too long for its type".into());
            } else if n > 1 {
                self.warning(
                    t.loc,
                    "multi-character character constant [-Wmultichar]".into(),
                );
            }
            let v = if n > 1 {
                r as i32
            } else {
                r as u8 as i8 as i32
            };
            return Some(Num {
                v: v as i64 as u64,
                u: false,
            });
        }
        if units.len() > 1 {
            self.warning(t.loc, "character constant too long for its type".into());
        }
        let last = *units.last().expect("unit");
        Some(match prefix {
            Prefix::Wide => Num {
                v: last as i32 as i64 as u64,
                u: false,
            },
            _ => Num {
                v: last as u64,
                u: true,
            },
        })
    }
}

/// `interpret_int_suffix` in C mode: `u`, `l`/`ll`, and GNU `i`/`j`.
fn int_suffix(s: &[u8]) -> Option<(bool, bool)> {
    let (mut u, mut l, mut i) = (0, 0, 0);
    for k in (0..s.len()).rev() {
        match s[k] {
            b'u' | b'U' => u += 1,
            b'i' | b'I' | b'j' | b'J' => i += 1,
            b'l' | b'L' => {
                l += 1;
                if l == 2 && s.get(k + 1) != Some(&s[k]) {
                    return None;
                }
            }
            _ => return None,
        }
    }
    (l <= 2 && u <= 1 && i <= 1).then_some((u == 1, i == 1))
}

fn hex_value(c: u8) -> u64 {
    (c as char).to_digit(16).expect("hex digit") as u64
}

/// `num_lshift` / `num_rshift`, with a negative signed count shifting the
/// other way; also whether a signed left shift lost bits.
fn shift(a: Num, b: Num, mut left: bool) -> (Num, bool) {
    let mut n = b.v;
    if !b.u && (b.v as i64) < 0 {
        left = !left;
        n = (b.v as i64).wrapping_neg() as u64;
    }
    let negative = !a.u && (a.v as i64) < 0;
    if left {
        if n >= 64 {
            return (Num { v: 0, u: a.u }, !a.u && a.v != 0);
        }
        let v = a.v << n;
        let lost = !a.u && ((v as i64) >> n) as u64 != a.v;
        (Num { v, u: a.u }, lost)
    } else {
        let v = if n >= 64 {
            if negative { u64::MAX } else { 0 }
        } else if negative {
            ((a.v as i64) >> n) as u64
        } else {
            a.v >> n
        };
        (Num { v, u: a.u }, false)
    }
}

/// The code units of a literal body: bytes for a narrow literal, UTF-16
/// or UTF-32 units for a wide one, escapes resolved as cpplib's
/// `convert_escape` does.
fn decode(body: &[u8], width: u32, wide: bool, diags: &mut Vec<(bool, String)>) -> Vec<u32> {
    let mask = if width == 32 {
        u32::MAX
    } else {
        (1u32 << width) - 1
    };
    let mut out = Vec::new();
    let push_char = |out: &mut Vec<u32>, cp: u32| {
        if !wide {
            let mut b = [0u8; 4];
            let s = char::from_u32(cp).unwrap_or('\u{FFFD}').encode_utf8(&mut b);
            out.extend(s.bytes().map(u32::from));
        } else if width == 16 && cp > 0xFFFF {
            let c = cp - 0x10000;
            out.push(0xD800 | (c >> 10));
            out.push(0xDC00 | (c & 0x3FF));
        } else {
            out.push(cp);
        }
    };
    let mut i = 0;
    while i < body.len() {
        let c = body[i];
        if c != b'\\' {
            if wide && c >= 0x80 {
                let len = match c {
                    0xF0.. => 4,
                    0xE0.. => 3,
                    _ => 2,
                };
                let end = (i + len).min(body.len());
                let cp = std::str::from_utf8(&body[i..end])
                    .ok()
                    .and_then(|s| s.chars().next())
                    .map_or(c as u32, u32::from);
                push_char(&mut out, cp);
                i = end;
            } else {
                out.push(c as u32);
                i += 1;
            }
            continue;
        }
        i += 1;
        let Some(&e) = body.get(i) else {
            break;
        };
        i += 1;
        let v = match e {
            b'\\' | b'\'' | b'"' | b'?' | b'(' | b'{' | b'[' | b'%' => e as u32,
            b'a' => 7,
            b'b' => 8,
            b'f' => 12,
            b'n' => 10,
            b'r' => 13,
            b't' => 9,
            b'v' => 11,
            b'e' | b'E' => 27,
            b'0'..=b'7' => {
                let mut v = (e - b'0') as u32;
                let mut k = 0;
                while k < 2 && matches!(body.get(i), Some(b'0'..=b'7')) {
                    v = v * 8 + (body[i] - b'0') as u32;
                    i += 1;
                    k += 1;
                }
                if v & !mask != 0 {
                    diags.push((false, "octal escape sequence out of range".into()));
                }
                v & mask
            }
            b'x' => {
                let start = i;
                let mut v = 0u64;
                while body.get(i).is_some_and(u8::is_ascii_hexdigit) {
                    v = (v << 4) | hex_value(body[i]);
                    i += 1;
                }
                if i == start {
                    diags.push((true, "\\x used with no following hex digits".into()));
                }
                if v > mask as u64 || i - start > 16 {
                    diags.push((false, "hex escape sequence out of range".into()));
                }
                v as u32 & mask
            }
            b'u' | b'U' => {
                let n = if e == b'u' { 4 } else { 8 };
                let hex = body
                    .get(i..i + n)
                    .filter(|h| h.iter().all(u8::is_ascii_hexdigit));
                let Some(hex) = hex else {
                    diags.push((true, "incomplete universal character name".into()));
                    break;
                };
                i += n;
                let cp = hex
                    .iter()
                    .fold(0u32, |a, &h| (a << 4) | hex_value(h) as u32);
                if (cp < 0xA0 && !matches!(cp, 0x24 | 0x40 | 0x60))
                    || (0xD800..0xE000).contains(&cp)
                    || cp > 0x10FFFF
                {
                    diags.push((
                        true,
                        format!("\\u{cp:04x} is not a valid universal character"),
                    ));
                    continue;
                }
                push_char(&mut out, cp);
                continue;
            }
            _ => {
                diags.push((false, format!("unknown escape sequence: '\\{}'", e as char)));
                e as u32
            }
        };
        out.push(v);
    }
    out
}

/// A narrow string literal's body with escapes resolved, for `#line`.
pub(super) fn unescape(body: &[u8]) -> Vec<u8> {
    decode(body, 8, false, &mut Vec::new())
        .into_iter()
        .map(|b| b as u8)
        .collect()
}
