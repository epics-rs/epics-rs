//! No raw float placeholder with a precision or an exponent in production
//! source.
//!
//! # The rule
//!
//! In the production slice of every file under the `src` of each crate the
//! calling workspace names, a format placeholder that carries a precision
//! (`{:.3}`, `{:>8.*}`, `{:.p$}`) or the `e` / `E` type MUST format an
//! argument written as `CFloat(..)`.
//!
//! Those placeholders are how this workspace spells C's `%.Nf` / `%e`, and on
//! a raw `f64` Rust prints a NaN as `NaN` with its sign dropped where glibc
//! prints `nan` / `-nan`, and `{:e}` writes `1e5` where C writes `1e+05`.
//! `epics_libcom_rs::cfmt::CFloat` is the one owner of C's spelling; a raw
//! float reaching one of these placeholders is console or PV text that
//! differs from the C IOC's.
//!
//! # Why textual
//!
//! clippy cannot see a format argument's type, and a placeholder's type is the
//! only thing that tells a float apart from a string being truncated
//! (`{:.12}` of a `&str`). So the argument must *say* what it is: written
//! `CFloat(x)` it passes, written `x` it fails, and a captured identifier
//! (`{x:.3}`) fails because nothing at the call site says what `x` holds.
//! Placeholders whose argument is genuinely not a float are named in the
//! caller's exemption list with the reason; an exemption that matches nothing
//! fails the gate, so the list cannot outlive its sites. The crates and the
//! list are the workspace's own: this crate's `tests/cfloat_gate.rs` holds
//! epics-rs's, and epics-rs-iocs takes the crate as a git dev-dependency.
//!
//! Every `name!(...)` invocation is read, not a list of known format macros:
//! a format string is the first argument that is exactly one string literal.
//! A placeholder with no argument to bind to is reported as `<none>`.
//!
//! `cargo run -p cfloat-gate --bin cfloat-census` prints every site the gate
//! sees, exempted or not.

use std::path::Path;

/// One float placeholder whose argument is not written as `CFloat(..)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Site {
    /// Path from the workspace root.
    pub file: String,
    /// 1-based line of the format string.
    pub line: usize,
    /// The placeholder as written, braces included.
    pub placeholder: String,
    /// The argument's source text, or the captured identifier.
    pub arg: String,
}

/// Every float placeholder in `src` whose argument is not `CFloat(..)`, as
/// `(line, placeholder, argument)`. `src` is a production slice with comments
/// stripped.
pub fn sites(src: &str) -> Vec<(usize, String, String)> {
    let toks = lex(src);
    let mut out = Vec::new();
    for k in 0..toks.len() {
        let Some(args) = invocation_args(&toks, k) else {
            continue;
        };
        let Some(fmt_at) = args
            .iter()
            .position(|a| a.len() == 1 && matches!(a[0].tok, Tok::Str(Some(_))))
        else {
            continue;
        };
        let Tok::Str(Some(fmt)) = &args[fmt_at][0].tok else {
            unreachable!()
        };
        let line = 1 + src[..args[fmt_at][0].start].matches('\n').count();
        let mut positional = Vec::new();
        let mut named = Vec::new();
        for a in &args[fmt_at + 1..] {
            match named_arg(a) {
                Some(name) => named.push((name, arg_text(src, &a[2..]))),
                None => positional.push(arg_text(src, a)),
            }
        }
        let mut next = 0usize;
        for p in placeholders(fmt) {
            let mut take = || {
                next += 1;
                positional.get(next - 1).cloned()
            };
            if p.star {
                take();
            }
            let arg = match &p.arg {
                ArgRef::Next => take(),
                ArgRef::Index(i) => positional.get(*i).cloned(),
                ArgRef::Name(n) => Some(
                    named
                        .iter()
                        .find(|(name, _)| name == n)
                        .map_or_else(|| n.clone(), |(_, e)| e.clone()),
                ),
            };
            if !p.float {
                continue;
            }
            match arg {
                Some(e) if is_cfloat(&e) => {}
                Some(e) => out.push((line, p.text, e)),
                None => out.push((line, p.text, "<none>".to_string())),
            }
        }
    }
    out
}

/// Every directory `<group>/<name>` under `root` that has a `src`, relative
/// to `root` and sorted.
pub fn crates_in(root: &Path, groups: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for group in groups {
        let mut crates: Vec<_> = std::fs::read_dir(root.join(group))
            .unwrap_or_else(|e| panic!("cfloat-gate: cannot read {group}: {e}"))
            .map(|e| e.expect("dir entry").path())
            .filter(|p| p.join("src").is_dir())
            .map(|p| {
                p.strip_prefix(root)
                    .expect("below root")
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        crates.sort();
        out.extend(crates);
    }
    out
}

/// Every [`Site`] in `crates` (directories relative to `root`), exemptions
/// not applied.
pub fn census(root: &Path, crates: &[String]) -> Vec<Site> {
    let mut out = Vec::new();
    for krate in crates {
        let files = source_guard::rust_sources(root.join(krate).join("src"));
        let test_only = test_only_files(&files);
        for (rel, text) in files {
            if test_only
                .iter()
                .any(|t| rel == t || rel.starts_with(&format!("{t}/")))
            {
                continue;
            }
            let slice = source_guard::production_str(text, source_guard::Comments::Strip);
            for (line, placeholder, arg) in sites(&slice) {
                out.push(Site {
                    file: format!("{krate}/src/{rel}"),
                    line,
                    placeholder,
                    arg,
                });
            }
        }
    }
    out
}

/// The gate: panics naming every unexempted site in `crates` and every stale
/// entry of `exempt`, whose `(file, placeholder, argument, reason)` entries
/// name sites whose argument is not a float and are matched exactly.
pub fn check(root: &Path, crates: &[String], exempt: &[(&str, &str, &str, &str)]) {
    assert!(
        !crates.is_empty(),
        "cfloat-gate: name at least one crate; an empty list proves nothing"
    );
    let all = census(root, crates);
    let exempted = |s: &Site| {
        exempt
            .iter()
            .any(|(f, p, a, _)| s.file == *f && s.placeholder == *p && s.arg == *a)
    };
    let raw: Vec<String> = all
        .iter()
        .filter(|s| !exempted(s))
        .map(|s| format!("{}:{}: {} <- {}", s.file, s.line, s.placeholder, s.arg))
        .collect();
    let stale: Vec<String> = exempt
        .iter()
        .filter(|(f, p, a, _)| {
            !all.iter()
                .any(|s| s.file == *f && s.placeholder == *p && s.arg == *a)
        })
        .map(|(f, p, a, _)| format!("{f}: {p} <- {a}"))
        .collect();
    assert!(
        raw.is_empty() && stale.is_empty(),
        "float placeholders not formatting a CFloat (wrap the argument in \
         epics_libcom_rs::cfmt::CFloat, or name a non-float in the exemption \
         list):\n{}\n\
         stale exemptions:\n{}",
        raw.join("\n"),
        stale.join("\n")
    );
}

/// Files a parent module declares only under a test-only `cfg`, as paths
/// relative to the crate's `src`; a directory entry covers its subtree.
fn test_only_files(files: &[(&str, &str)]) -> Vec<String> {
    let mut out = Vec::new();
    for (rel, text) in files {
        let prod = source_guard::production_str(text, source_guard::Comments::Keep);
        let (dir, file) = rel.rsplit_once('/').unwrap_or(("", rel));
        let stem = file.trim_end_matches(".rs");
        let join = |a: &str, b: &str| {
            if a.is_empty() {
                b.to_string()
            } else {
                format!("{a}/{b}")
            }
        };
        let child_dir = if matches!(file, "mod.rs" | "lib.rs" | "main.rs") || dir == "bin" {
            dir.to_string()
        } else {
            join(dir, stem)
        };
        let raw: Vec<&str> = text.lines().collect();
        // `split`, not `lines`: blanked lines at the end of the file count.
        for (i, kept) in prod.split('\n').enumerate().take(raw.len()) {
            let line = raw[i].trim();
            let Some(name) = line
                .strip_suffix(';')
                .and_then(|l| l.rsplit_once("mod "))
                .map(|(_, n)| n.trim())
            else {
                continue;
            };
            if !kept.trim().is_empty() || name.contains(|c: char| !c.is_alphanumeric() && c != '_')
            {
                continue;
            }
            let path = (0..i)
                .rev()
                .map(|j| raw[j].trim())
                .take_while(|l| l.starts_with("#["))
                .find_map(|l| {
                    l.strip_prefix("#[path = \"")
                        .and_then(|p| p.strip_suffix("\"]"))
                });
            match path {
                Some(p) => out.push(join(dir, p)),
                None => {
                    out.push(join(&child_dir, &format!("{name}.rs")));
                    out.push(join(&child_dir, name));
                }
            }
        }
    }
    out
}

fn is_cfloat(expr: &str) -> bool {
    let mut e = expr.trim_start_matches(|c: char| c == '&' || c.is_whitespace());
    if let Some(rest) = e.strip_prefix("mut ") {
        e = rest.trim_start();
    }
    loop {
        if e.starts_with("CFloat(") {
            return true;
        }
        let ident = e
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(e.len());
        match e[ident..].strip_prefix("::") {
            Some(rest) if ident > 0 => e = rest,
            _ => return false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Punct(char),
    /// A string literal's value; `None` for byte and C strings.
    Str(Option<String>),
    /// Numbers, char literals, lifetimes.
    Other,
}

#[derive(Clone, Debug)]
struct Token {
    tok: Tok,
    start: usize,
    end: usize,
}

fn lex(src: &str) -> Vec<Token> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let c = b[i];
        let tok = if c.is_ascii_whitespace() {
            i += 1;
            continue;
        } else if c == b'"' {
            let (end, value) = cooked_string(src, i + 1);
            i = end;
            Tok::Str(Some(value))
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            let word = &src[i..j];
            if matches!(word, "r" | "br" | "cr") && raw_string_at(b, j) {
                i = raw_string_end(b, j);
                let hashes = src[j..].find('"').expect("raw string quote");
                Tok::Str((word == "r").then(|| src[j + hashes + 1..i - hashes - 1].to_string()))
            } else if matches!(word, "b" | "c") && b.get(j) == Some(&b'"') {
                i = cooked_string(src, j + 1).0;
                Tok::Str(None)
            } else if word == "b" && b.get(j) == Some(&b'\'') {
                i = char_or_lifetime_end(src, j);
                Tok::Other
            } else if word == "r"
                && b.get(j) == Some(&b'#')
                && b.get(j + 1)
                    .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
            {
                let mut k = j + 1;
                while k < b.len() && (b[k].is_ascii_alphanumeric() || b[k] == b'_') {
                    k += 1;
                }
                i = k;
                Tok::Ident(src[j + 1..k].to_string())
            } else {
                i = j;
                Tok::Ident(word.to_string())
            }
        } else if c.is_ascii_digit() {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            Tok::Other
        } else if c == b'\'' {
            i = char_or_lifetime_end(src, i);
            Tok::Other
        } else if c >= 0x80 {
            i += src[i..].chars().next().expect("char").len_utf8();
            Tok::Other
        } else {
            i += 1;
            Tok::Punct(c as char)
        };
        out.push(Token { tok, start, end: i });
    }
    out
}

/// End of a `"..."` body starting at `i` (past the quote), and its value.
fn cooked_string(src: &str, mut i: usize) -> (usize, String) {
    let b = src.as_bytes();
    let mut value = String::new();
    while i < b.len() && b[i] != b'"' {
        if b[i] != b'\\' {
            let ch = src[i..].chars().next().expect("char");
            value.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let esc = b.get(i + 1).copied().unwrap_or(b'\\');
        i += 2;
        match esc {
            b'n' => value.push('\n'),
            b't' => value.push('\t'),
            b'r' => value.push('\r'),
            b'0' => value.push('\0'),
            b'x' => {
                let hex = src.get(i..i + 2).unwrap_or("0");
                value.push(char::from(u8::from_str_radix(hex, 16).unwrap_or(0)));
                i += 2;
            }
            b'u' => {
                let close = src[i..].find('}').map_or(src.len(), |c| i + c);
                let hex = src[i..close].trim_start_matches('{');
                value.extend(u32::from_str_radix(hex, 16).ok().and_then(char::from_u32));
                i = close + 1;
            }
            b'\n' | b'\r' => {
                while i < b.len() && b[i].is_ascii_whitespace() {
                    i += 1;
                }
            }
            other => value.push(other as char),
        }
    }
    (i + 1, value)
}

fn raw_string_at(b: &[u8], mut j: usize) -> bool {
    while b.get(j) == Some(&b'#') {
        j += 1;
    }
    b.get(j) == Some(&b'"')
}

/// End of a raw string whose `#`s or quote start at `j`.
fn raw_string_end(b: &[u8], mut j: usize) -> usize {
    let mut hashes = 0;
    while b[j] == b'#' {
        hashes += 1;
        j += 1;
    }
    j += 1;
    while j < b.len() {
        if b[j] == b'"'
            && b[j + 1..]
                .iter()
                .take(hashes)
                .filter(|c| **c == b'#')
                .count()
                == hashes
        {
            return j + 1 + hashes;
        }
        j += 1;
    }
    b.len()
}

/// End of a char literal or a lifetime starting with the `'` at `i`.
fn char_or_lifetime_end(src: &str, i: usize) -> usize {
    let b = src.as_bytes();
    if b.get(i + 1) == Some(&b'\\') {
        let close = src[i + 3..].find('\'').map_or(src.len(), |c| i + 3 + c);
        return close + 1;
    }
    let Some(ch) = src[i + 1..].chars().next() else {
        return b.len();
    };
    let after = i + 1 + ch.len_utf8();
    if b.get(after) == Some(&b'\'') {
        return after + 1;
    }
    let mut j = i + 1;
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
        j += 1;
    }
    j
}

/// The comma-separated arguments of the macro invocation whose name is token
/// `k`, if it is one: `name!(`, `name![` or `name!{`, with no space before the
/// `!` so that `if !(x)` is not read as one.
fn invocation_args(toks: &[Token], k: usize) -> Option<Vec<&[Token]>> {
    let (Tok::Ident(_), Some(bang), Some(open)) = (&toks[k].tok, toks.get(k + 1), toks.get(k + 2))
    else {
        return None;
    };
    if bang.tok != Tok::Punct('!') || bang.start != toks[k].end {
        return None;
    }
    if !matches!(open.tok, Tok::Punct('(' | '[' | '{')) {
        return None;
    }
    let mut depth = 0usize;
    let mut args = Vec::new();
    let mut from = k + 3;
    for j in k + 2..toks.len() {
        match toks[j].tok {
            Tok::Punct('(' | '[' | '{') => depth += 1,
            Tok::Punct(')' | ']' | '}') => {
                depth -= 1;
                if depth == 0 {
                    if from < j {
                        args.push(&toks[from..j]);
                    }
                    return Some(args);
                }
            }
            Tok::Punct(',') if depth == 1 => {
                args.push(&toks[from..j]);
                from = j + 1;
            }
            _ => {}
        }
    }
    None
}

/// `name = expr` as a format argument: the name.
fn named_arg(arg: &[Token]) -> Option<String> {
    match arg {
        [
            Token {
                tok: Tok::Ident(name),
                ..
            },
            eq,
            rest,
            ..,
        ] if eq.tok == Tok::Punct('=')
            && !(matches!(rest.tok, Tok::Punct('=' | '>')) && rest.start == eq.end) =>
        {
            Some(name.clone())
        }
        _ => None,
    }
}

fn arg_text(src: &str, arg: &[Token]) -> String {
    match (arg.first(), arg.last()) {
        (Some(a), Some(z)) => src[a.start..z.end]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

#[derive(Debug, PartialEq)]
enum ArgRef {
    Next,
    Index(usize),
    Name(String),
}

#[derive(Debug)]
struct Placeholder {
    text: String,
    arg: ArgRef,
    /// `.*`: the precision is the next positional argument.
    star: bool,
    /// A precision, or the `e` / `E` type.
    float: bool,
}

/// The placeholders of a format string, in order.
fn placeholders(fmt: &str) -> Vec<Placeholder> {
    let mut out = Vec::new();
    let mut rest = fmt;
    while let Some(open) = rest.find(['{', '}']) {
        let tail = &rest[open..];
        if tail.starts_with("{{") || tail.starts_with("}}") || tail.starts_with('}') {
            rest = &tail[if tail.len() > 1 && tail.as_bytes()[1] == tail.as_bytes()[0] {
                2
            } else {
                1
            }..];
            continue;
        }
        let Some(close) = tail.find('}') else {
            break;
        };
        let body = &tail[1..close];
        let (arg, spec) = body.split_once(':').unwrap_or((body, ""));
        let arg = arg.trim();
        let arg = if arg.is_empty() {
            ArgRef::Next
        } else if let Ok(i) = arg.parse() {
            ArgRef::Index(i)
        } else {
            ArgRef::Name(arg.to_string())
        };
        let (star, precision, ty) = parse_spec(spec);
        out.push(Placeholder {
            text: tail[..=close].to_string(),
            arg,
            star,
            float: !ty.contains('?') && (precision || ty == "e" || ty == "E"),
        });
        rest = &tail[close + 1..];
    }
    out
}

/// `(star, has precision, type)` of a format spec:
/// `[[fill]align][sign]['#']['0'][width]['.' precision][type]`.
fn parse_spec(spec: &str) -> (bool, bool, &str) {
    let mut s = spec;
    let mut chars = s.chars();
    let first = chars.next();
    let second = chars.next();
    if second.is_some_and(|c| "<^>".contains(c)) {
        s = &s[first.map_or(0, char::len_utf8) + 1..];
    } else if first.is_some_and(|c| "<^>".contains(c)) {
        s = &s[1..];
    }
    s = s.strip_prefix(['+', '-']).unwrap_or(s);
    s = s.strip_prefix('#').unwrap_or(s);
    if s.starts_with('0') && !s[1..].starts_with('$') {
        s = &s[1..];
    }
    s = skip_count(s);
    let Some(after_dot) = s.strip_prefix('.') else {
        return (false, false, s);
    };
    if let Some(ty) = after_dot.strip_prefix('*') {
        return (true, true, ty);
    }
    (false, true, skip_count(after_dot))
}

/// Past a width or precision: an integer, or an argument followed by `$`.
fn skip_count(s: &str) -> &str {
    let digits = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if digits > 0 {
        let after = &s[digits..];
        return after.strip_prefix('$').unwrap_or(after);
    }
    let ident = s
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(s.len());
    match s[ident..].strip_prefix('$') {
        Some(after) if ident > 0 => after,
        _ => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(src: &str) -> Vec<String> {
        sites(src).into_iter().map(|(_, _, a)| a).collect()
    }

    #[test]
    fn a_precision_or_exponent_needs_a_cfloat_argument() {
        assert_eq!(args(r#"format!("{:.3}", x)"#), ["x"]);
        assert_eq!(args(r#"format!("{:e}", x)"#), ["x"]);
        assert_eq!(args(r#"format!("{:E}", x)"#), ["x"]);
        assert_eq!(args(r#"format!("{:>8.2e}", x)"#), ["x"]);
        assert!(args(r#"format!("{:.3}", CFloat(x))"#).is_empty());
        assert!(args(r#"format!("{:.3}", &CFloat(x))"#).is_empty());
        assert!(args(r#"format!("{:.3}", cfmt::CFloat(x))"#).is_empty());
        assert!(args(r#"format!("{:.3}", epics_libcom_rs::cfmt::CFloat(x).g())"#).is_empty());
        assert_eq!(args(r#"format!("{:.3}", NotCFloat(x))"#), ["NotCFloat(x)"]);
    }

    #[test]
    fn placeholders_without_precision_or_exponent_are_not_float_sites() {
        for f in [
            "{}", "{:>8}", "{:08}", "{:?}", "{:.3?}", "{:x}", "{:#x}", "{:.>8}", "{{:.3}}",
            "{:-^20}",
        ] {
            assert!(args(&format!("format!(\"{f}\", x)")).is_empty(), "{f}");
        }
        assert_eq!(args(r#"format!("{:.<8.2}", x)"#), ["x"]);
        assert_eq!(args(r#"format!("{:+08.3}", x)"#), ["x"]);
    }

    #[test]
    fn arguments_bind_as_rust_binds_them() {
        assert_eq!(args(r#"format!("{} {:.3}", a, b)"#), ["b"]);
        assert_eq!(args(r#"format!("{1:.3} {}", a, b)"#), ["b"]);
        assert!(args(r#"format!("{:.*}", p, CFloat(x))"#).is_empty());
        assert_eq!(args(r#"format!("{:.*} {}", p, x, y)"#), ["x"]);
        assert_eq!(args(r#"format!("{:1$.3} {}", x, w)"#), ["x"]);
        assert_eq!(args(r#"format!("{:.p$}", x, p = 3)"#), ["x"]);
        assert_eq!(args(r#"format!("{v:.3}", v = x)"#), ["x"]);
        assert!(args(r#"format!("{v:.3}", v = CFloat(x))"#).is_empty());
        assert_eq!(args(r#"format!("{v:.3}")"#), ["v"]);
        assert_eq!(args(r#"format!("{:.3}")"#), ["<none>"]);
    }

    #[test]
    fn every_macro_shape_is_read() {
        assert_eq!(args(r#"write!(f, "{:.3}", x)"#), ["x"]);
        assert_eq!(args(r#"assert!(a == b, "{:.3}", x)"#), ["x"]);
        assert_eq!(
            args(r#"tracing::warn!(target: "t", k = 1, "{:.3}", x)"#),
            ["x"]
        );
        assert_eq!(args(r#"my_log![r"{:.3}", x]"#), ["x"]);
        assert_eq!(args(r###"my_log!{r#"{:.3} "q""#, x}"###), ["x"]);
        assert_eq!(args(r#"outer!("{}", inner!("{:.1}", y))"#), ["y"]);
        assert!(args(r#"if !(x) { "{:.3}" }"#).is_empty());
        assert!(args(r#"a!=("{:.3}")"#).is_empty());
        assert!(args(r#"f(b"{:.3}")"#).is_empty());
    }

    #[test]
    fn literals_do_not_end_an_argument_list() {
        assert_eq!(args(r#"format!("{} {:.3}", '"', x)"#), ["x"]);
        assert_eq!(args(r#"format!("{} {:.3}", '{', x)"#), ["x"]);
        assert_eq!(args(r#"format!("{} {:.3}", '\'', x)"#), ["x"]);
        assert_eq!(args(r#"format!("\"{}\" {:.3}", s, x)"#), ["x"]);
        assert_eq!(args(r#"format!("{} {:.3}", f::<'a>(","), x)"#), ["x"]);
        assert_eq!(args(r#"format!("{} {:.3}", (a, b), x)"#), ["x"]);
        assert_eq!(args("format!(\"a \\\n   {:.3}\", x)"), ["x"]);
    }

    #[test]
    fn a_site_is_reported_on_its_format_string_line() {
        let s = sites("fn f() {\n    println!(\n        \"{:.1}\",\n        x\n    );\n}\n");
        assert_eq!(s, [(3, "{:.1}".to_string(), "x".to_string())]);
    }

    #[test]
    fn a_module_declared_under_cfg_test_is_not_production() {
        let files = [
            (
                "lib.rs",
                "#[cfg(test)]\nmod t;\nmod p;\n#[cfg(all(test, unix))]\n#[path = \"x_tests.rs\"]\nmod x;\n",
            ),
            ("a/b.rs", "#[cfg(test)]\nmod c;\n"),
            ("a/mod.rs", "#[cfg(not(test))]\nmod d;\n"),
        ];
        let got = test_only_files(&files);
        assert_eq!(got, ["t.rs", "t", "x_tests.rs", "a/b/c.rs", "a/b/c"]);
    }
}
