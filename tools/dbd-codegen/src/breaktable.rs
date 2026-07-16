//! `breaktable(...)` -> Rust, for the vendored `bpt*.dbd` files.
//!
//! C's `makeBpt` reads a raw sensor curve (`bptTypeKdegC.data`, thousands of
//! rows) and *fits* it: it walks the curve and emits the fewest breakpoints that
//! keep the piecewise-linear error under the accuracy the `.data` header asks
//! for. The result is checked into an EPICS install as `dbd/bptTypeKdegC.dbd`,
//! and an IOC that wants it says `dbLoadDatabase(".../bptTypeKdegC.dbd")`.
//!
//! The port vendors makeBpt's OUTPUT, not its input. Re-running the fit in Rust
//! would be a second implementation of a numeric approximation, and any
//! disagreement in it — one breakpoint placed a step earlier — moves every
//! converted value on that segment. The breakpoints are the contract; the fit is
//! not. So this module only transcribes.
//!
//! The grammar is the one `dbLexRoutines.c` accepts and
//! `db_loader::parse_db_with_breaktables` already implements at runtime:
//!
//! ```text
//! breaktable(NAME) {
//!     raw eng
//!     raw eng
//! }
//! ```

use std::path::Path;

/// One vendored `breaktable(...)`: its name and its `(raw, eng)` points in
/// declaration order.
pub struct BreakTable {
    pub name: String,
    pub points: Vec<(f64, f64)>,
    /// The `.dbd` this came from, for the generated doc comment.
    pub source: String,
}

/// Is this a breakpoint-table `.dbd` rather than a record/menu one? The record
/// parser must skip these — they declare no `recordtype`, `menu` or `device`.
pub fn is_breaktable_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("bpt"))
}

pub fn parse_file(path: &Path) -> Result<Vec<BreakTable>, String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let source = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?")
        .to_string();
    parse_str(&src, &source)
}

fn parse_str(src: &str, source: &str) -> Result<Vec<BreakTable>, String> {
    let mut out = Vec::new();
    let mut tokens = tokenize(src);
    tokens.reverse(); // pop() from the front

    while let Some(tok) = tokens.pop() {
        if tok != "breaktable" {
            return Err(format!("{source}: expected `breaktable`, got `{tok}`"));
        }
        expect(&mut tokens, "(", source)?;
        let name = tokens
            .pop()
            .ok_or_else(|| format!("{source}: breaktable name expected"))?;
        expect(&mut tokens, ")", source)?;
        expect(&mut tokens, "{", source)?;

        let mut points = Vec::new();
        loop {
            let tok = tokens
                .pop()
                .ok_or_else(|| format!("{source}: unexpected end of breaktable({name})"))?;
            if tok == "}" {
                break;
            }
            let raw: f64 = tok
                .parse()
                .map_err(|_| format!("{source}: breaktable({name}): non-numeric raw `{tok}`"))?;
            let tok = tokens.pop().ok_or_else(|| {
                format!("{source}: breaktable({name}): raw value missing its eng")
            })?;
            let eng: f64 = tok
                .parse()
                .map_err(|_| format!("{source}: breaktable({name}): non-numeric eng `{tok}`"))?;
            points.push((raw, eng));
        }
        if points.len() < 2 {
            return Err(format!(
                "{source}: breaktable({name}) has {} point(s); at least 2 are needed",
                points.len()
            ));
        }
        out.push(BreakTable {
            name,
            points,
            source: source.to_string(),
        });
    }
    Ok(out)
}

fn expect(tokens: &mut Vec<String>, want: &str, source: &str) -> Result<(), String> {
    match tokens.pop() {
        Some(t) if t == want => Ok(()),
        other => Err(format!("{source}: expected `{want}`, got {other:?}")),
    }
}

fn tokenize(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in src.lines() {
        let line = line.split('#').next().unwrap_or("");
        for ch in line.chars() {
            match ch {
                '(' | ')' | '{' | '}' | ',' => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                    out.push(ch.to_string());
                }
                c if c.is_whitespace() => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                c => cur.push(c),
            }
        }
        if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    out
}

/// Emit the seed table `epics-base-rs` compiles into `BreakTableRegistry::new`.
pub fn emit(tables: &[BreakTable]) -> String {
    let mut s = String::new();
    s.push_str(
        "//! Breakpoint tables, transcribed from the vendored `dbd/bpt*.dbd`.\n\
         //!\n\
         //! @generated by `tools/dbd-codegen` from `crates/epics-base-rs/dbd/bpt*.dbd`.\n\
         //! DO NOT EDIT. Re-run `cargo run -p dbd-codegen -- --write` after changing a\n\
         //! vendored `.dbd`; `cargo run -p dbd-codegen -- --check` fails on drift.\n\
         //!\n\
         //! These are `makeBpt`'s OUTPUT, byte-for-byte as an EPICS install ships them\n\
         //! in `dbd/`. C's `makeBpt` fits the raw sensor curve in `bpt*.data` down to a\n\
         //! handful of breakpoints; the breakpoints are the contract, and re-running the\n\
         //! fit in Rust could only move them.\n\
         \n",
    );
    s.push_str(
        "/// The `(raw, eng)` points of every vendored breakpoint table, keyed by the\n\
         /// `menuConvert` / `LINR` name that selects it.\n\
         pub static BREAK_TABLES: &[(&str, &[(f64, f64)])] = &[\n",
    );
    for t in tables {
        s.push_str(&format!("    // {}\n", t.source));
        s.push_str(&format!("    (\"{}\", &[\n", t.name));
        for (raw, eng) in &t.points {
            s.push_str(&format!("        ({raw:?}, {eng:?}),\n"));
        }
        s.push_str("    ]),\n");
    }
    s.push_str("];\n");
    s
}
