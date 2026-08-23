//! A parser for the subset of the EPICS `.dbd` grammar that declares record
//! types and menus.
//!
//! The grammar (`dbLexRoutines.c`, `dbYacc.y`) that matters here:
//!
//! ```text
//! menu(NAME) { choice(IDENT,"label") ... }
//! device(recordtype, link_type, dset_name, "choice string")
//! recordtype(NAME) {
//!     include "dbCommon.dbd"
//!     field(NAME,DBF_TYPE) {
//!         prompt("...")  promptgroup("...")  special(SPC_xxx)
//!         pp(TRUE|FALSE) asl(ASL0|ASL1)      size(N)
//!         menu(menuXxx)  initial("...")      interest(N)
//!         prop(YES|NO)   base(HEX|DECIMAL)   extra("C decl")
//!     }
//! }
//! ```
//!
//! Lines beginning with `%` are C passthrough for the generated header and
//! carry no declaration; `#` starts a comment. Both are dropped by the lexer.

use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Menu {
    /// Choice labels in `menu()` declaration order. The index is the stored
    /// `epicsEnum16` value and is wire-visible, so order is load-bearing.
    pub choices: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Field {
    pub name: String,
    /// The `DBF_*` token verbatim.
    pub dbf: String,
    pub prompt: Option<String>,
    pub promptgroup: Option<String>,
    /// The `SPC_*` token, or a bare number (a few records use `special(255)`).
    pub special: Option<String>,
    pub pp: bool,
    pub asl: Option<String>,
    pub size: Option<u32>,
    pub menu: Option<String>,
    pub initial: Option<String>,
    pub interest: Option<u32>,
    pub prop: bool,
    pub base: Option<String>,
    pub extra: Option<String>,
    /// True when the field came from `include "dbCommon.dbd"` rather than the
    /// record's own body. The port models dbCommon separately (`CommonFields`),
    /// so these are not emitted into per-record tables.
    pub from_common: bool,
}

#[derive(Debug, Clone)]
pub struct RecordType {
    pub name: String,
    pub fields: Vec<Field>,
    /// The `.dbd` basename this record was parsed from, for provenance.
    pub source: String,
}

/// One `device(rectype, link_type, dset, "name")` declaration.
///
/// C builds a `dbDeviceMenu` per record type out of these, in LOAD order, and
/// a record's `DTYP` field is an index into it (`dbConvert.c::getDeviceString`
/// renders the choice; an unset DTYP is index 0, the first device declared for
/// that record type). The order is therefore wire-visible and is preserved.
#[derive(Debug, Clone)]
pub struct Device {
    pub record: String,
    /// The `link_type` argument — `CONSTANT`, `INST_IO`, `VME_IO`, ... — as
    /// declared, upper-case. C stores it in `devSup::link_type` and
    /// `dbCanSetLink` (`dbStaticLib.c:2400-2419`) refuses to install a link
    /// whose parsed type is incompatible with it, so a `.db` that gives an
    /// `INST_IO` device a constant `INP` is rejected. Dropping it here is what
    /// let the port accept such a `.db`.
    pub link_type: String,
    pub name: String,
}

#[derive(Debug, Default)]
pub struct Dbd {
    pub menus: BTreeMap<String, Menu>,
    pub records: Vec<RecordType>,
    pub devices: Vec<Device>,
}

/// One lexical token. The `.dbd` grammar needs nothing richer: identifiers,
/// quoted strings, and the four punctuation marks.
#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    LParen,
    RParen,
    LBrace,
    RBrace,
    Comma,
}

fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let mut toks = Vec::new();
    let b = src.as_bytes();
    let mut i = 0;
    // Track column 0 so a `%` passthrough line is recognised only where the
    // grammar allows it (the `%` must be the first non-blank on the line).
    let mut at_line_start = true;
    while i < b.len() {
        let c = b[i];
        match c {
            b'\n' => {
                at_line_start = true;
                i += 1;
            }
            b' ' | b'\t' | b'\r' => i += 1,
            b'#' | b'%' if at_line_start || c == b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'(' => {
                toks.push(Tok::LParen);
                i += 1;
                at_line_start = false;
            }
            b')' => {
                toks.push(Tok::RParen);
                i += 1;
                at_line_start = false;
            }
            b'{' => {
                toks.push(Tok::LBrace);
                i += 1;
                at_line_start = false;
            }
            b'}' => {
                toks.push(Tok::RBrace);
                i += 1;
                at_line_start = false;
            }
            b',' => {
                toks.push(Tok::Comma);
                i += 1;
                at_line_start = false;
            }
            b'"' => {
                i += 1;
                let mut s = String::new();
                while i < b.len() && b[i] != b'"' {
                    // `.dbd` strings carry C escapes; the only ones that occur
                    // in the record declarations are `\"` and `\\`.
                    if b[i] == b'\\' && i + 1 < b.len() {
                        i += 1;
                    }
                    s.push(b[i] as char);
                    i += 1;
                }
                if i >= b.len() {
                    return Err("unterminated string".into());
                }
                i += 1;
                toks.push(Tok::Str(s));
                at_line_start = false;
            }
            _ => {
                let start = i;
                while i < b.len()
                    && !matches!(b[i], b'(' | b')' | b'{' | b'}' | b',' | b'"' | b'#')
                    && !b[i].is_ascii_whitespace()
                {
                    i += 1;
                }
                if i == start {
                    return Err(format!("unexpected byte {:?}", b[i] as char));
                }
                toks.push(Tok::Ident(src[start..i].to_string()));
                at_line_start = false;
            }
        }
    }
    Ok(toks)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        self.pos += 1;
        t
    }
    fn expect(&mut self, want: &Tok) -> Result<(), String> {
        match self.next() {
            Some(ref t) if t == want => Ok(()),
            other => Err(format!("expected {want:?}, found {other:?}")),
        }
    }
    /// The comma-separated arguments of `foo(a, b, c)`.
    fn paren_args(&mut self) -> Result<Vec<String>, String> {
        self.expect(&Tok::LParen)?;
        let mut args = Vec::new();
        loop {
            match self.next() {
                Some(Tok::Ident(s)) | Some(Tok::Str(s)) => args.push(s),
                Some(Tok::RParen) => return Ok(args),
                other => return Err(format!("expected argument, found {other:?}")),
            }
            match self.next() {
                Some(Tok::Comma) => {}
                Some(Tok::RParen) => return Ok(args),
                other => return Err(format!("expected , or ) found {other:?}")),
            }
        }
    }

    /// The single argument of `foo(arg)` — an identifier or a quoted string,
    /// both of which reduce to their text.
    fn paren_arg(&mut self) -> Result<String, String> {
        self.expect(&Tok::LParen)?;
        let s = match self.next() {
            Some(Tok::Ident(s)) | Some(Tok::Str(s)) => s,
            // `prompt()` with no argument occurs in a few module dbds.
            Some(Tok::RParen) => return Ok(String::new()),
            other => return Err(format!("expected argument, found {other:?}")),
        };
        self.expect(&Tok::RParen)?;
        Ok(s)
    }
}

/// Parse one `.dbd` file. `resolve_include` supplies the text of an
/// `include "x.dbd"` seen inside a `recordtype` body — the port only ever
/// needs `dbCommon.dbd`, whose fields are marked `from_common`.
pub fn parse_file(path: &Path, common: &[Field]) -> Result<Dbd, String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let source = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    parse_str(&src, &source, common).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn parse_str(src: &str, source: &str, common: &[Field]) -> Result<Dbd, String> {
    let mut p = Parser {
        toks: lex(src)?,
        pos: 0,
    };
    let mut dbd = Dbd::default();

    while let Some(tok) = p.peek().cloned() {
        match tok {
            Tok::Ident(kw) if kw == "menu" => {
                p.next();
                let name = p.paren_arg()?;
                let choices = parse_menu_body(&mut p)?;
                dbd.menus.insert(name, Menu { choices });
            }
            Tok::Ident(kw) if kw == "device" => {
                p.next();
                let args = p.paren_args()?;
                // `device(rectype, link_type, dset, "choice")`. Three of the
                // four are kept: the record type it extends, the link type it
                // demands, and the DTYP string a `.db` names it by. The `dset`
                // is a C symbol — the name of a `struct dset` the IOC links
                // against — and has no referent in the port, which dispatches
                // device support by DTYP string rather than by C symbol. It is
                // the one argument with no consumer, so it is the one dropped.
                let [record, link_type, _dset, name] = <[String; 4]>::try_from(args)
                    .map_err(|a| format!("device() takes 4 arguments, found {}", a.len()))?;
                dbd.devices.push(Device {
                    record,
                    link_type: link_type.to_ascii_uppercase(),
                    name,
                });
            }
            Tok::Ident(kw) if kw == "recordtype" => {
                p.next();
                let name = p.paren_arg()?;
                let fields = parse_recordtype_body(&mut p, common)?;
                dbd.records.push(RecordType {
                    name,
                    fields,
                    source: source.to_string(),
                });
            }
            // `include`, `driver`, `registrar`, `variable`,
            // `function`, `breaktable` — declarations this generator does not
            // model. Skip the statement and any brace body that follows.
            Tok::Ident(_) => {
                p.next();
                skip_statement(&mut p)?;
            }
            other => return Err(format!("unexpected top-level token {other:?}")),
        }
    }
    Ok(dbd)
}

/// Skip `(...)` and an optional `{...}` after an unhandled top-level keyword.
fn skip_statement(p: &mut Parser) -> Result<(), String> {
    if p.peek() == Some(&Tok::LParen) {
        let mut depth = 0;
        loop {
            match p.next() {
                Some(Tok::LParen) => depth += 1,
                Some(Tok::RParen) => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Some(_) => {}
                None => return Err("unterminated ( in skipped statement".into()),
            }
        }
    } else {
        // A bare `include "x.dbd"` — its argument is the next token.
        p.next();
    }
    if p.peek() == Some(&Tok::LBrace) {
        let mut depth = 0;
        loop {
            match p.next() {
                Some(Tok::LBrace) => depth += 1,
                Some(Tok::RBrace) => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Some(_) => {}
                None => return Err("unterminated { in skipped statement".into()),
            }
        }
    }
    Ok(())
}

fn parse_menu_body(p: &mut Parser) -> Result<Vec<String>, String> {
    p.expect(&Tok::LBrace)?;
    let mut choices = Vec::new();
    loop {
        match p.next() {
            Some(Tok::RBrace) => break,
            Some(Tok::Ident(kw)) if kw == "choice" => {
                p.expect(&Tok::LParen)?;
                // choice(IDENT,"label") — the identifier is the C enum name and
                // is not wire-visible; only the label is.
                match p.next() {
                    Some(Tok::Ident(_)) | Some(Tok::Str(_)) => {}
                    other => return Err(format!("choice: expected ident, found {other:?}")),
                }
                p.expect(&Tok::Comma)?;
                let label = match p.next() {
                    Some(Tok::Str(s)) | Some(Tok::Ident(s)) => s,
                    other => return Err(format!("choice: expected label, found {other:?}")),
                };
                p.expect(&Tok::RParen)?;
                choices.push(label);
            }
            other => return Err(format!("menu body: unexpected {other:?}")),
        }
    }
    Ok(choices)
}

fn parse_recordtype_body(p: &mut Parser, common: &[Field]) -> Result<Vec<Field>, String> {
    p.expect(&Tok::LBrace)?;
    let mut fields = Vec::new();
    loop {
        match p.next() {
            Some(Tok::RBrace) => break,
            Some(Tok::Ident(kw)) if kw == "include" => {
                let inc = match p.next() {
                    Some(Tok::Str(s)) | Some(Tok::Ident(s)) => s,
                    other => return Err(format!("include: expected path, found {other:?}")),
                };
                if inc != "dbCommon.dbd" {
                    return Err(format!("unsupported include inside recordtype: {inc}"));
                }
                fields.extend(common.iter().cloned());
            }
            Some(Tok::Ident(kw)) if kw == "field" => {
                fields.push(parse_field(p)?);
            }
            other => return Err(format!("recordtype body: unexpected {other:?}")),
        }
    }
    Ok(fields)
}

fn parse_field(p: &mut Parser) -> Result<Field, String> {
    p.expect(&Tok::LParen)?;
    let name = match p.next() {
        Some(Tok::Ident(s)) | Some(Tok::Str(s)) => s,
        other => return Err(format!("field: expected name, found {other:?}")),
    };
    p.expect(&Tok::Comma)?;
    let dbf = match p.next() {
        Some(Tok::Ident(s)) | Some(Tok::Str(s)) => s,
        other => return Err(format!("field {name}: expected DBF type, found {other:?}")),
    };
    p.expect(&Tok::RParen)?;

    let mut f = Field {
        name,
        dbf,
        ..Default::default()
    };

    // The attribute block is optional in the grammar.
    if p.peek() != Some(&Tok::LBrace) {
        return Ok(f);
    }
    p.expect(&Tok::LBrace)?;
    loop {
        match p.next() {
            Some(Tok::RBrace) => break,
            Some(Tok::Ident(attr)) => {
                let arg = p.paren_arg()?;
                match attr.as_str() {
                    "prompt" => f.prompt = Some(arg),
                    "promptgroup" => f.promptgroup = Some(arg),
                    "special" => f.special = Some(arg),
                    // C `dbRecordtypeFieldItem` (`dbLexRoutines.c:637-645`):
                    // YES or TRUE sets `process_passive`, NO or FALSE clears
                    // it, and any other spelling is
                    // `yyerror("Invalid 'pp' value, ...")`, which sets
                    // `yyFailed` and so fails the whole `.dbd` load
                    // (`dbYacc.y:370-397`). The tests are `strcmp`, so they are
                    // case-sensitive: `pp(true)` is a load error in C, not a
                    // true. Defaulting an unrecognised value to `false` would
                    // give the flag two meanings — "the file said NO" and "we
                    // did not understand the file" — and only the first is a
                    // thing C can produce.
                    "pp" => {
                        f.pp = match arg.as_str() {
                            "YES" | "TRUE" => true,
                            "NO" | "FALSE" => false,
                            other => {
                                return Err(format!(
                                    "{}: Invalid 'pp' value '{other}', must be YES/NO/TRUE/FALSE",
                                    f.name
                                ));
                            }
                        }
                    }
                    "asl" => f.asl = Some(arg),
                    "size" => {
                        f.size = Some(
                            arg.parse()
                                .map_err(|_| format!("{}: bad size({arg})", f.name))?,
                        )
                    }
                    "menu" => f.menu = Some(arg),
                    "initial" => f.initial = Some(arg),
                    "interest" => {
                        f.interest = Some(
                            arg.parse()
                                .map_err(|_| format!("{}: bad interest({arg})", f.name))?,
                        )
                    }
                    // C (`dbLexRoutines.c:677-682`) is a bare
                    // `strcmp(value,"YES")`: exactly that spelling sets
                    // `prop`, every other one — `yes` included — leaves it 0,
                    // and there is no error arm. A field that advertises
                    // `DBE_PROPERTY` where C does not fires property monitors
                    // no C IOC sends.
                    "prop" => f.prop = arg == "YES",
                    "base" => f.base = Some(arg),
                    "extra" => f.extra = Some(arg),
                    other => return Err(format!("{}: unknown field attribute {other}", f.name)),
                }
            }
            other => return Err(format!("{}: unexpected {other:?} in body", f.name)),
        }
    }
    Ok(f)
}

/// Parse `dbCommon.dbd`, whose body is a bare field list with no enclosing
/// `recordtype`. Every field it yields is marked `from_common`.
pub fn parse_db_common(path: &Path) -> Result<Vec<Field>, String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut p = Parser {
        toks: lex(&src)?,
        pos: 0,
    };
    let mut fields = Vec::new();
    while let Some(tok) = p.next() {
        match tok {
            Tok::Ident(kw) if kw == "field" => {
                let mut f = parse_field(&mut p)?;
                f.from_common = true;
                fields.push(f);
            }
            other => return Err(format!("dbCommon.dbd: unexpected {other:?}")),
        }
    }
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_recordtype_with_every_attribute() {
        let src = r#"
# a comment
menu(menuSimm) {
    choice(menuSimmNO,"NO")
    choice(menuSimmYES,"YES")
}
recordtype(demo) {
    %#include "callback.h"
    field(VAL,DBF_DOUBLE) {
        prompt("Current Value")
        promptgroup("40 - Input")
        asl(ASL0)
        pp(TRUE)
        prop(YES)
    }
    field(NAME,DBF_STRING) {
        prompt("Record Name")
        special(SPC_NOMOD)
        size(61)
        interest(2)
        initial("x")
        menu(menuSimm)
        base(HEX)
        extra("void *p")
    }
}
"#;
        let dbd = parse_str(src, "demo.dbd", &[]).unwrap();
        assert_eq!(dbd.menus["menuSimm"].choices, vec!["NO", "YES"]);
        let rec = &dbd.records[0];
        assert_eq!(rec.name, "demo");
        assert_eq!(rec.fields.len(), 2);

        let val = &rec.fields[0];
        assert_eq!(val.dbf, "DBF_DOUBLE");
        assert!(val.pp);
        assert!(val.prop);
        assert_eq!(val.asl.as_deref(), Some("ASL0"));
        assert_eq!(val.special, None);

        let nm = &rec.fields[1];
        assert_eq!(nm.special.as_deref(), Some("SPC_NOMOD"));
        assert_eq!(nm.size, Some(61));
        assert_eq!(nm.interest, Some(2));
        assert_eq!(nm.initial.as_deref(), Some("x"));
        assert_eq!(nm.menu.as_deref(), Some("menuSimm"));
        assert_eq!(nm.base.as_deref(), Some("HEX"));
        assert_eq!(nm.extra.as_deref(), Some("void *p"));
        assert!(!nm.pp);
    }

    /// C's `pp` table is `YES|TRUE` -> TRUE, `NO|FALSE` -> FALSE, anything
    /// else -> `yyerror` (`dbLexRoutines.c:637-645`). All four spellings are
    /// C's, and `pp(YES)` mapping to `false` here would leave a `dbPutField`
    /// on that field failing to process a record a C IOC processes.
    #[test]
    fn pp_takes_every_spelling_c_accepts() {
        for (spelling, want) in [
            ("YES", true),
            ("TRUE", true),
            ("NO", false),
            ("FALSE", false),
        ] {
            let src = format!("recordtype(demo) {{ field(FOO,DBF_LONG) {{ pp({spelling}) }} }}");
            let dbd = parse_str(&src, "demo.dbd", &[])
                .unwrap_or_else(|e| panic!("pp({spelling}) must parse: {e}"));
            assert_eq!(
                dbd.records[0].fields[0].pp, want,
                "pp({spelling}) must map to {want}"
            );
        }
    }

    /// An unrecognised `pp` fails the whole load in C — `yyerror` sets
    /// `yyFailed` and `pvt_yy_parse` returns -1 (`dbYacc.y:370-397`). Silently
    /// emitting `pp: false` would ship a record type whose PP links are dead
    /// and say nothing. `strcmp` is case-sensitive, so `pp(true)` is one of
    /// the values C rejects.
    #[test]
    fn an_unrecognised_pp_value_fails_the_load_as_c_does() {
        for spelling in ["Ture", "true", "Yes", ""] {
            let src = format!("recordtype(demo) {{ field(FOO,DBF_LONG) {{ pp({spelling}) }} }}");
            let err = parse_str(&src, "demo.dbd", &[])
                .err()
                .unwrap_or_else(|| panic!("pp({spelling}) must be rejected"));
            assert!(
                err.contains("Invalid 'pp' value"),
                "pp({spelling}) must report C's message, got: {err}"
            );
        }
    }

    /// C's `prop` is a bare `strcmp(value,"YES")` with no error arm
    /// (`dbLexRoutines.c:677-682`), so `prop(yes)` leaves it 0. Accepting the
    /// lowercase spelling here would set `DBE_PROPERTY` on a field C does not,
    /// and the record would fire property monitors no C IOC sends.
    #[test]
    fn prop_is_the_exact_case_sensitive_yes_c_tests_for() {
        for (spelling, want) in [("YES", true), ("yes", false), ("Yes", false), ("NO", false)] {
            let src = format!("recordtype(demo) {{ field(FOO,DBF_LONG) {{ prop({spelling}) }} }}");
            let dbd = parse_str(&src, "demo.dbd", &[])
                .unwrap_or_else(|e| panic!("prop({spelling}) must parse: {e}"));
            assert_eq!(
                dbd.records[0].fields[0].prop, want,
                "prop({spelling}) must map to {want}"
            );
        }
    }

    #[test]
    fn include_dbcommon_splices_common_fields_tagged() {
        let common = parse_str(
            "recordtype(x) { field(NAME,DBF_STRING) { size(61) } }",
            "c.dbd",
            &[],
        )
        .unwrap()
        .records
        .remove(0)
        .fields
        .into_iter()
        .map(|mut f| {
            f.from_common = true;
            f
        })
        .collect::<Vec<_>>();

        let dbd = parse_str(
            r#"recordtype(demo) { include "dbCommon.dbd" field(VAL,DBF_LONG) {} }"#,
            "demo.dbd",
            &common,
        )
        .unwrap();
        let fields = &dbd.records[0].fields;
        assert_eq!(fields.len(), 2);
        assert!(fields[0].from_common);
        assert_eq!(fields[0].name, "NAME");
        assert!(!fields[1].from_common);
        assert_eq!(fields[1].name, "VAL");
    }

    /// A `%` passthrough line is C source for the generated header, not a
    /// declaration; a `#` comment can also sit at the end of an attribute line.
    #[test]
    fn passthrough_and_trailing_comments_are_dropped() {
        let dbd = parse_str(
            "recordtype(demo) {\n\
             %struct demoRecord;\n\
             field(A,DBF_LONG) {\n\
                 prop(YES)       # get_precision\n\
             }\n\
             }\n",
            "demo.dbd",
            &[],
        )
        .unwrap();
        assert_eq!(dbd.records[0].fields.len(), 1);
        assert!(dbd.records[0].fields[0].prop);
    }
}
