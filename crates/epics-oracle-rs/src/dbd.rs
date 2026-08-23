//! Parser for the *expanded* EPICS `.dbd` — the harness's denominator.
//!
//! The denominator must come from the spec, not from a hand-written list, so
//! every enumerated case traces back to a `field(...)` declaration in
//! `/home/stevek/work/epics-base/dbd/softIoc.dbd`. That file is the built,
//! macro-expanded form: `dbCommon` is already inlined into each `recordtype`
//! and every `menu(...)` it references is defined in the same file. The
//! unexpanded per-record `.dbd`s (`aiRecord.dbd` etc.) still carry
//! `include "dbCommon.dbd"` and would under-count the surface by 48
//! fields/record, so this parser deliberately targets the expanded file.
//!
//! Grammar actually present in the expanded file (nothing more is accepted):
//!
//! ```text
//! menu(menuScan) { choice(menuScanPassive, "Passive") ... }
//! recordtype(ai) {
//!     %  C escape lines -- ignored
//!     field(VAL, DBF_DOUBLE) {
//!         prompt("Current EGU Value")   asl(ASL0)   pp(TRUE)
//!         special(SPC_NOMOD)   menu(menuScan)   size(61)   initial("1")
//!     }
//! }
//! ```

use std::collections::BTreeMap;
use std::path::Path;

/// A `DBF_*` field type as declared in the `.dbd`.
///
/// `NoAccess` is retained rather than dropped at parse time: the harness needs
/// to *count* it to state honestly what fraction of the declared surface is
/// unreachable by a CA client (see [`crate::surface`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub enum DbfType {
    String,
    Char,
    UChar,
    Short,
    UShort,
    Long,
    ULong,
    Int64,
    UInt64,
    Float,
    Double,
    Enum,
    Menu,
    Device,
    InLink,
    OutLink,
    FwdLink,
    NoAccess,
}

impl DbfType {
    /// The `.dbd` spelling of this type, e.g. `DBF_DOUBLE`.
    ///
    /// The inverse of [`DbfType::from_dbd_name`]. Both are public because the
    /// allowlist's `dbf_types` scope constraint is written in `.dbd` spellings
    /// (`crates/epics-oracle-rs/allowlist/expected-deviations.toml`), so the
    /// row text and the measured field type have to meet on one vocabulary.
    pub fn as_dbd_name(self) -> &'static str {
        match self {
            Self::String => "DBF_STRING",
            Self::Char => "DBF_CHAR",
            Self::UChar => "DBF_UCHAR",
            Self::Short => "DBF_SHORT",
            Self::UShort => "DBF_USHORT",
            Self::Long => "DBF_LONG",
            Self::ULong => "DBF_ULONG",
            Self::Int64 => "DBF_INT64",
            Self::UInt64 => "DBF_UINT64",
            Self::Float => "DBF_FLOAT",
            Self::Double => "DBF_DOUBLE",
            Self::Enum => "DBF_ENUM",
            Self::Menu => "DBF_MENU",
            Self::Device => "DBF_DEVICE",
            Self::InLink => "DBF_INLINK",
            Self::OutLink => "DBF_OUTLINK",
            Self::FwdLink => "DBF_FWDLINK",
            Self::NoAccess => "DBF_NOACCESS",
        }
    }

    /// Parse a `.dbd` type spelling. `None` for anything not a `DBF_*` name.
    pub fn from_dbd_name(s: &str) -> Option<Self> {
        Some(match s {
            "DBF_STRING" => Self::String,
            "DBF_CHAR" => Self::Char,
            "DBF_UCHAR" => Self::UChar,
            "DBF_SHORT" => Self::Short,
            "DBF_USHORT" => Self::UShort,
            "DBF_LONG" => Self::Long,
            "DBF_ULONG" => Self::ULong,
            "DBF_INT64" => Self::Int64,
            "DBF_UINT64" => Self::UInt64,
            "DBF_FLOAT" => Self::Float,
            "DBF_DOUBLE" => Self::Double,
            "DBF_ENUM" => Self::Enum,
            "DBF_MENU" => Self::Menu,
            "DBF_DEVICE" => Self::Device,
            "DBF_INLINK" => Self::InLink,
            "DBF_OUTLINK" => Self::OutLink,
            "DBF_FWDLINK" => Self::FwdLink,
            "DBF_NOACCESS" => Self::NoAccess,
            _ => return None,
        })
    }

    /// Can a CA client see this field at all?
    ///
    /// `DBF_NOACCESS` fields are raw C pointers in the record struct (`BPTR`,
    /// `RPVT`, `DPVT`, ...). `dbNameToAddr` resolves them but CA refuses to
    /// serve them, so they are *not* part of the observable surface and must
    /// not be counted in the denominator — counting them would let the harness
    /// claim coverage of fields no client can ever reach.
    pub fn is_ca_observable(self) -> bool {
        self != Self::NoAccess
    }

    /// Is this field a link (`INP`, `OUT`, `FLNK`, `DOL`, ...)?
    ///
    /// Links are observable as strings but writing a boundary value to one
    /// rewires the record graph, so the value-boundary generator skips them;
    /// they are still probed read-only for type/value agreement.
    pub fn is_link(self) -> bool {
        matches!(self, Self::InLink | Self::OutLink | Self::FwdLink)
    }

    /// The enum-like classes, whose *strings* must be compared, not their
    /// ordinals — a port that agrees on `1` but reports `"On"` vs `"HIGH"` is
    /// observably different to a client.
    pub fn is_enumlike(self) -> bool {
        matches!(self, Self::Enum | Self::Menu | Self::Device)
    }
}

/// One `field(NAME, DBF_TYPE) { ... }` declaration.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FieldDef {
    pub name: String,
    pub dbf: DbfType,
    /// `size(N)` — declared capacity of a `DBF_STRING`.
    pub size: Option<u32>,
    /// `special(SPC_*)` — dispatch code; `SPC_NOMOD` means the field rejects
    /// client writes, which is directly observable as a put rejection.
    pub special: Option<String>,
    /// `menu(nameOfMenu)` — the choice set backing a `DBF_MENU`.
    pub menu: Option<String>,
    /// `initial("...")` — the record's declared power-on value.
    pub initial: Option<String>,
    /// `pp(TRUE)` — a write to this field processes the record.
    pub pp: bool,
    /// `asl(ASL0|ASL1)` — access-security level.
    pub asl: Option<String>,
}

impl FieldDef {
    /// `special(SPC_NOMOD)` — the record refuses client modification. C's
    /// `dbPut` fails such a put with `S_db_noMod`; the port must too.
    pub fn is_nomod(&self) -> bool {
        self.special.as_deref() == Some("SPC_NOMOD")
    }

    /// `special(SPC_DBADDR)` — **the `.dbd` does not determine this field's
    /// type or element count.**
    ///
    /// `dbNameToAddr` fills the `DBADDR` from the `.dbd` and then hands it to
    /// the record type to rewrite, gated on exactly this token
    /// (`dbAccess.c:640-648`):
    ///
    /// ```text
    /// paddr->dbr_field_type = mapDBFToDBR[dbfType];   /* what the dbd says */
    /// if (paddr->special == SPC_DBADDR) {
    ///     const rset *prset = dbGetRset(paddr);
    ///     if (prset && prset->cvt_dbaddr)
    ///         return prset->cvt_dbaddr(paddr);        /* record gets the last word */
    /// }
    /// ```
    ///
    /// `cvt_dbaddr` is C the `.dbd` does not describe, and it may contradict the
    /// declared type outright — `mbbo`'s rewrites `DBF_ENUM` to `DBF_USHORT`
    /// whenever the record has no state strings (`mbboRecord.c:308-311`), a
    /// condition on a *runtime record value*, so no static reading of the `.dbd`
    /// could predict it even in principle. It may also change only the element
    /// count and leave the type alone: `asyn`'s makes the `DBF_CHAR` `BINP`/`BOUT`
    /// arrays of `imax`/`omax` (`asynRecord.c:944-955`).
    ///
    /// So this predicate marks the fields whose declared `DBF_*` type is a
    /// default that has been overridden by code, and any consumer deriving
    /// client-visible behaviour from the declared type MUST decline to predict
    /// here rather than assert the `.dbd`'s answer.
    pub fn rewrites_dbaddr(&self) -> bool {
        self.special.as_deref() == Some("SPC_DBADDR")
    }
}

/// One `recordtype(name) { ... }` declaration.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RecordType {
    pub name: String,
    pub fields: Vec<FieldDef>,
}

impl RecordType {
    pub fn field(&self, name: &str) -> Option<&FieldDef> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// Fields a CA client can actually reach (everything but `DBF_NOACCESS`).
    pub fn observable_fields(&self) -> impl Iterator<Item = &FieldDef> {
        self.fields.iter().filter(|f| f.dbf.is_ca_observable())
    }
}

/// A `menu(name) { choice(id, "Label") ... }` — the string set a `DBF_MENU`
/// field reports over CA.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Menu {
    pub name: String,
    pub choices: Vec<String>,
}

/// The whole expanded `.dbd`: the spec the denominator is derived from.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Dbd {
    pub record_types: Vec<RecordType>,
    pub menus: BTreeMap<String, Menu>,
}

impl Dbd {
    pub fn record_type(&self, name: &str) -> Option<&RecordType> {
        self.record_types.iter().find(|r| r.name == name)
    }

    /// The choice strings a `DBF_MENU` field will report, resolved through its
    /// `menu(...)` reference.
    pub fn menu_choices(&self, field: &FieldDef) -> Option<&[String]> {
        let m = field.menu.as_ref()?;
        self.menus.get(m).map(|m| m.choices.as_slice())
    }

    pub fn parse_file(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read dbd {}: {e}", path.display()))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let mut p = Parser {
            lines: text.lines().collect(),
            i: 0,
        };
        p.run()
    }
}

struct Parser<'a> {
    lines: Vec<&'a str>,
    i: usize,
}

impl Parser<'_> {
    fn run(&mut self) -> Result<Dbd, String> {
        let mut record_types = Vec::new();
        let mut menus = BTreeMap::new();

        while self.i < self.lines.len() {
            let line = self.lines[self.i].trim();
            // `%` lines are verbatim C escapes; `#` is a comment. Both are
            // declaration-free and cannot contain a field(), so skipping them
            // outright is safe and keeps the brace tracking honest.
            if line.starts_with('%') || line.starts_with('#') || line.is_empty() {
                self.i += 1;
                continue;
            }
            if let Some(name) = block_head(line, "recordtype") {
                self.i += 1;
                let fields = self.parse_recordtype_body()?;
                record_types.push(RecordType { name, fields });
            } else if let Some(name) = block_head(line, "menu") {
                self.i += 1;
                let choices = self.parse_menu_body()?;
                menus.insert(name.clone(), Menu { name, choices });
            } else {
                self.i += 1;
            }
        }

        if record_types.is_empty() {
            return Err("dbd parsed to zero recordtypes — wrong file? the harness \
                        needs the EXPANDED dbd (softIoc.dbd), not aiRecord.dbd"
                .into());
        }
        Ok(Dbd {
            record_types,
            menus,
        })
    }

    /// Consume `field(...) {...}` declarations until the recordtype's closing
    /// brace. Nested braces inside a field body are tracked so a stray `}` in
    /// a `%`-escape or prompt string cannot end the record early.
    fn parse_recordtype_body(&mut self) -> Result<Vec<FieldDef>, String> {
        let mut fields = Vec::new();
        while self.i < self.lines.len() {
            let line = self.lines[self.i].trim();
            if line.starts_with('%') || line.starts_with('#') || line.is_empty() {
                self.i += 1;
                continue;
            }
            if line == "}" {
                self.i += 1;
                return Ok(fields);
            }
            if let Some((name, ty)) = field_head(line) {
                let Some(dbf) = DbfType::from_dbd_name(&ty) else {
                    return Err(format!("unknown DBF type `{ty}` on field `{name}`"));
                };
                self.i += 1;
                let body = self.collect_braced_body(line);
                fields.push(build_field(name, dbf, &body));
                continue;
            }
            self.i += 1;
        }
        Err("unterminated recordtype block".into())
    }

    /// Gather the lines of a `{ ... }` body that opened on `head`, returning
    /// them joined. If the body opened and closed on the same line, nothing is
    /// consumed.
    fn collect_braced_body(&mut self, head: &str) -> String {
        let mut depth = head.matches('{').count() as i32 - head.matches('}').count() as i32;
        if depth <= 0 {
            return head.to_string();
        }
        let mut body = String::new();
        while self.i < self.lines.len() && depth > 0 {
            let line = self.lines[self.i];
            self.i += 1;
            let t = line.trim();
            if t.starts_with('%') || t.starts_with('#') {
                continue;
            }
            depth += t.matches('{').count() as i32;
            depth -= t.matches('}').count() as i32;
            body.push_str(t);
            body.push('\n');
        }
        body
    }

    fn parse_menu_body(&mut self) -> Result<Vec<String>, String> {
        let mut choices = Vec::new();
        while self.i < self.lines.len() {
            let line = self.lines[self.i].trim();
            self.i += 1;
            if line == "}" {
                return Ok(choices);
            }
            // choice(menuScanPassive, "Passive")
            if line.starts_with("choice(")
                && let Some(label) = quoted_after_comma(line)
            {
                choices.push(label);
            }
        }
        Err("unterminated menu block".into())
    }
}

/// `recordtype(ai) {` -> `Some("ai")`; also matches `menu(menuScan) {`.
fn block_head(line: &str, kw: &str) -> Option<String> {
    let rest = line.strip_prefix(kw)?.trim_start();
    let rest = rest.strip_prefix('(')?;
    let (name, _) = rest.split_once(')')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    Some(name.to_string())
}

/// `field(VAL, DBF_DOUBLE) {` -> `Some(("VAL", "DBF_DOUBLE"))`.
fn field_head(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("field")?.trim_start();
    let rest = rest.strip_prefix('(')?;
    let (args, _) = rest.split_once(')')?;
    let (name, ty) = args.split_once(',')?;
    Some((name.trim().to_string(), ty.trim().to_string()))
}

/// The quoted label in `choice(id, "Label")` / `initial("1")`.
fn quoted_after_comma(s: &str) -> Option<String> {
    let start = s.find('"')?;
    let rest = &s[start + 1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Pull `directive(arg)` out of a field body, unquoting if quoted.
///
/// Scans the whole body rather than only line starts, because a field body may
/// be written on one line (`field(NAME, DBF_STRING) { size(61) }`) as readily as
/// on several. A line-start-only match silently returned `None` for the
/// single-line form, which would have dropped `size(...)` and with it every
/// string-capacity boundary — a missing case that no test on the *generator*
/// would have caught.
///
/// The keyword must sit on a token boundary so that a directive is never
/// matched inside a longer identifier.
fn directive(body: &str, kw: &str) -> Option<String> {
    let mut from = 0;
    while let Some(hit) = body[from..].find(kw) {
        let at = from + hit;
        let after = at + kw.len();
        from = after;

        // Token boundary on the left: the previous char must not be part of an
        // identifier, or `menu` would match inside `promptgroup_menu`.
        let left_ok = at == 0
            || !body[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
        if !left_ok {
            continue;
        }
        // ...and the keyword must be immediately followed by its argument list.
        let Some(rest) = body[after..].strip_prefix('(') else {
            continue;
        };
        let Some((arg, _)) = rest.split_once(')') else {
            continue;
        };
        let arg = arg.trim();
        let arg = arg.strip_prefix('"').unwrap_or(arg);
        let arg = arg.strip_suffix('"').unwrap_or(arg);
        return Some(arg.to_string());
    }
    None
}

fn build_field(name: String, dbf: DbfType, body: &str) -> FieldDef {
    FieldDef {
        name,
        dbf,
        size: directive(body, "size").and_then(|s| s.parse().ok()),
        special: directive(body, "special"),
        menu: directive(body, "menu"),
        initial: directive(body, "initial"),
        pp: directive(body, "pp").as_deref() == Some("TRUE"),
        asl: directive(body, "asl"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
menu(menuScan) {
    choice(menuScanPassive, "Passive")
    choice(menuScanEvent, "Event")
}
recordtype(ai) {
    %#include "epicsTypes.h"
    %struct aiRecord;
    field(NAME, DBF_STRING) {
        size(61)
        prompt("Record Name")
        special(SPC_NOMOD)
    }
    field(VAL, DBF_DOUBLE) {
        prompt("Current EGU Value")
        asl(ASL0)
        pp(TRUE)
    }
    field(SCAN, DBF_MENU) {
        prompt("Scan Mechanism")
        menu(menuScan)
        special(SPC_SCAN)
    }
    field(RPVT, DBF_NOACCESS) {
        prompt("Record Private")
        extra("void *rpvt")
    }
    field(STAT, DBF_MENU) {
        initial("UDF")
        menu(menuAlarmStat)
    }
}
"#;

    fn dbd() -> Dbd {
        Dbd::parse(SAMPLE).expect("parse")
    }

    #[test]
    fn parses_recordtype_and_all_its_fields() {
        let d = dbd();
        let ai = d.record_type("ai").expect("ai recordtype");
        assert_eq!(ai.fields.len(), 5, "every field() must be captured");
    }

    #[test]
    fn parses_field_directives() {
        let d = dbd();
        let ai = d.record_type("ai").unwrap();

        let name = ai.field("NAME").unwrap();
        assert_eq!(name.dbf, DbfType::String);
        assert_eq!(name.size, Some(61));
        assert!(name.is_nomod(), "NAME is special(SPC_NOMOD)");

        let val = ai.field("VAL").unwrap();
        assert_eq!(val.dbf, DbfType::Double);
        assert!(val.pp, "VAL is pp(TRUE)");
        assert_eq!(val.asl.as_deref(), Some("ASL0"));
        assert!(!val.is_nomod());

        let stat = ai.field("STAT").unwrap();
        assert_eq!(stat.initial.as_deref(), Some("UDF"));
    }

    /// `SPC_DBADDR` and `SPC_NOMOD` are both `special(...)`, and they say
    /// entirely different things — one that the field rejects writes, one that
    /// the field's declared type is not its real type. Neither may answer for
    /// the other.
    #[test]
    fn the_two_special_codes_are_told_apart() {
        let d = Dbd::parse(
            r#"
recordtype(mbbo) {
    field(VAL, DBF_ENUM) { pp(TRUE) special(SPC_DBADDR) prompt("Desired Value") }
    field(NAME, DBF_STRING) { size(61) special(SPC_NOMOD) }
    field(HIGH, DBF_DOUBLE) { prompt("Alarm Deadband") }
}
"#,
        )
        .unwrap();
        let mbbo = d.record_type("mbbo").unwrap();

        let val = mbbo.field("VAL").unwrap();
        assert!(val.rewrites_dbaddr(), "mbbo.VAL is special(SPC_DBADDR)");
        assert!(!val.is_nomod(), "SPC_DBADDR says nothing about writability");

        let name = mbbo.field("NAME").unwrap();
        assert!(name.is_nomod());
        assert!(
            !name.rewrites_dbaddr(),
            "SPC_NOMOD leaves the declared type authoritative"
        );

        let high = mbbo.field("HIGH").unwrap();
        assert!(!high.rewrites_dbaddr(), "no special() at all");
    }

    #[test]
    fn resolves_menu_choices_to_strings() {
        let d = dbd();
        let scan = d.record_type("ai").unwrap().field("SCAN").unwrap();
        assert_eq!(scan.menu.as_deref(), Some("menuScan"));
        assert_eq!(
            d.menu_choices(scan).unwrap(),
            ["Passive".to_string(), "Event".to_string()]
        );
    }

    #[test]
    fn noaccess_is_excluded_from_the_observable_surface() {
        let d = dbd();
        let ai = d.record_type("ai").unwrap();
        assert!(!DbfType::NoAccess.is_ca_observable());
        let obs: Vec<_> = ai.observable_fields().map(|f| f.name.as_str()).collect();
        assert_eq!(obs, ["NAME", "VAL", "SCAN", "STAT"], "RPVT must be dropped");
    }

    #[test]
    fn c_escape_lines_never_leak_into_the_field_list() {
        let d = dbd();
        let ai = d.record_type("ai").unwrap();
        assert!(ai.fields.iter().all(|f| !f.name.contains("include")));
    }

    /// A field body written on ONE line must yield the same directives as the
    /// multi-line form. The line-start-only matcher returned `size = None` here,
    /// which silently dropped every string-capacity boundary case downstream.
    #[test]
    fn single_line_field_body_yields_the_same_directives_as_multiline() {
        let d = Dbd::parse(
            r#"
recordtype(ai) {
    field(NAME, DBF_STRING) { size(61) prompt("Record Name") special(SPC_NOMOD) }
    field(SCAN, DBF_MENU) { menu(menuScan) pp(TRUE) }
}
"#,
        )
        .unwrap();
        let ai = d.record_type("ai").unwrap();
        let name = ai.field("NAME").unwrap();
        assert_eq!(name.size, Some(61), "size() on a one-line body");
        assert!(name.is_nomod());
        let scan = ai.field("SCAN").unwrap();
        assert_eq!(scan.menu.as_deref(), Some("menuScan"));
        assert!(scan.pp);
    }

    /// A directive name must not be matched inside a longer identifier.
    #[test]
    fn directive_matching_respects_token_boundaries() {
        let d = Dbd::parse(
            r#"
recordtype(ai) {
    field(VAL, DBF_DOUBLE) { promptgroup("40 - Input") prompt("Value") }
}
"#,
        )
        .unwrap();
        let val = d.record_type("ai").unwrap().field("VAL").unwrap();
        // `promptgroup(...)` must not be read as a `prompt(...)`-style hit for
        // `pp`, `menu`, or `size`.
        assert_eq!(val.size, None);
        assert_eq!(val.menu, None);
        assert!(!val.pp);
    }

    /// The unexpanded per-record dbds still carry `include "dbCommon.dbd"`, so
    /// parsing one would silently under-count the surface. Refuse loudly.
    #[test]
    fn empty_dbd_is_an_error_not_an_empty_denominator() {
        let err = Dbd::parse("# nothing here\n").unwrap_err();
        assert!(err.contains("zero recordtypes"), "got: {err}");
    }
}
