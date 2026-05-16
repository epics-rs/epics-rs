//! pvRequest builders.
//!
//! A pvRequest is sent inside an INIT operation to filter which fields the
//! server will return. Wire format: a single `0x80` (structure tag) byte
//! followed by an `encode_structure_desc` body for a structure shaped like
//!
//! ```text
//! structure
//!     structure field
//!         structure value      (empty)
//!         structure alarm      (empty)
//!         structure timeStamp  (empty)
//! ```
//!
//! Empty sub-structures carry no value bytes — only the descriptor — so the
//! caller need not append anything after the body.

use crate::proto::ByteOrder;
use crate::pvdata::encode::encode_type_desc;
use crate::pvdata::{FieldDesc, PvField};

/// Build a pvRequest selecting `fields` at the top level of "field(...)".
fn build(fields: &[&str], order: ByteOrder) -> Vec<u8> {
    let inner = FieldDesc::Structure {
        struct_id: String::new(),
        fields: fields
            .iter()
            .map(|name| {
                (
                    name.to_string(),
                    FieldDesc::Structure {
                        struct_id: String::new(),
                        fields: Vec::new(),
                    },
                )
            })
            .collect(),
    };
    let pv_request = FieldDesc::Structure {
        struct_id: String::new(),
        fields: vec![("field".to_string(), inner)],
    };
    // pvRequest wire format begins with the 0x80 type tag (the rest of the
    // structure body follows). encode_type_desc emits both the tag and the
    // body so the result is exactly what the wire expects.
    let mut out = Vec::new();
    encode_type_desc(&pv_request, order, &mut out);
    out
}

/// Build the standard pvRequest: `field(value,alarm,timeStamp)`.
pub fn build_pv_request(big_endian: bool) -> Vec<u8> {
    let order = if big_endian {
        ByteOrder::Big
    } else {
        ByteOrder::Little
    };
    build(&["value", "alarm", "timeStamp"], order)
}

/// Build a minimal pvRequest for PUT: `field(value)`.
pub fn build_pv_request_value_only(big_endian: bool) -> Vec<u8> {
    let order = if big_endian {
        ByteOrder::Big
    } else {
        ByteOrder::Little
    };
    build(&["value"], order)
}

/// Build a pvRequest selecting an arbitrary list of top-level fields,
/// equivalent to `field(<f1>,<f2>,...)`.
pub fn build_pv_request_fields(fields: &[&str], big_endian: bool) -> Vec<u8> {
    let order = if big_endian {
        ByteOrder::Big
    } else {
        ByteOrder::Little
    };
    build(fields, order)
}

/// Convert a pvRequest *structure* (rooted at `request_desc`) into a
/// `BitSet` over the fields of `value_desc`, using pvData spec §5.4
/// depth-first bit numbering. Mirrors pvxs `request2mask`.
///
/// Rules:
/// - The pvRequest has shape `structure { structure field { ... } }`.
///   Each direct child of `field` selects the matching top-level field
///   in `value_desc` and (recursively) its sub-fields named.
/// - An empty `field {}` (no children) selects *every* bit (root + all
///   descendants).
/// - Names in pvRequest that don't exist in `value_desc` are silently
///   skipped, *unless* no field at all matched — in which case
///   `Err(EmptyMask)` is returned.
/// - The root bit (bit 0) is always set when at least one descendant is
///   selected.
pub fn request_to_mask(
    value_desc: &crate::pvdata::FieldDesc,
    request_desc: &crate::pvdata::FieldDesc,
) -> Result<crate::proto::BitSet, RequestMaskError> {
    use crate::pvdata::FieldDesc;
    let mut mask = crate::proto::BitSet::new();

    // Find the top-level "field" sub-structure inside the pvRequest.
    let request_field = match request_desc {
        FieldDesc::Structure { fields, .. } => fields.iter().find(|(n, _)| n == "field"),
        _ => None,
    };
    let request_field = match request_field {
        Some((_, FieldDesc::Structure { fields, .. })) => fields,
        _ => {
            // No `field` sub-structure (e.g., the standard "empty
            // pvRequest" the Rust client sends as a 6-byte 0xFD-cached
            // empty struct). Per pvxs convention this means "send the
            // whole structure".
            let total = value_desc.total_bits();
            for i in 0..total {
                mask.set(i);
            }
            return Ok(mask);
        }
    };

    // Empty `field {}` → all fields set.
    if request_field.is_empty() {
        let total = value_desc.total_bits();
        for i in 0..total {
            mask.set(i);
        }
        return Ok(mask);
    }

    // Walk each requested top-level name and recursively select bits.
    let mut any_matched = false;
    if let FieldDesc::Structure { fields, .. } = value_desc {
        let mut child_bit = 1usize;
        for (name, child_desc) in fields {
            if let Some((_, sub_request)) = request_field.iter().find(|(n, _)| n == name) {
                any_matched = true;
                // Mark this field and recurse.
                mark_path(&mut mask, child_bit, child_desc, sub_request);
            }
            child_bit += child_desc.total_bits();
        }
    }

    if !any_matched {
        return Err(RequestMaskError::EmptyMask);
    }
    mask.set(0); // root
    Ok(mask)
}

/// Recursively mark `value_desc`'s bit (at `bit_offset`) plus any
/// requested sub-fields as defined by `sub_request`.
fn mark_path(
    mask: &mut crate::proto::BitSet,
    bit_offset: usize,
    value_desc: &crate::pvdata::FieldDesc,
    sub_request: &crate::pvdata::FieldDesc,
) {
    use crate::pvdata::FieldDesc;
    mask.set(bit_offset);

    // Pick out the named sub-fields requested.
    let sub_fields = match sub_request {
        FieldDesc::Structure { fields, .. } => fields,
        _ => return,
    };
    if sub_fields.is_empty() {
        // Empty {} selects this entire sub-tree.
        let total = value_desc.total_bits();
        for i in 0..total {
            mask.set(bit_offset + i);
        }
        return;
    }

    if let FieldDesc::Structure { fields, .. } = value_desc {
        let mut child_bit = bit_offset + 1;
        for (name, child_desc) in fields {
            if let Some((_, sub2)) = sub_fields.iter().find(|(n, _)| n == name) {
                mark_path(mask, child_bit, child_desc, sub2);
            }
            child_bit += child_desc.total_bits();
        }
    }
}

/// Errors from [`request_to_mask`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RequestMaskError {
    /// The pvRequest selected no existing fields.
    #[error("pvRequest selected no existing fields")]
    EmptyMask,
}

// ── pvRequest expression parser (mirrors pvxs PVRParser) ─────────────────

/// Fluent builder for pvRequest expressions. Mirrors pvxs's
/// `Context::request()` (client.h:525) / `RequestBuilder` API:
///
/// ```ignore
/// let req = PvRequestBuilder::new()
///     .field("value")
///     .field("alarm.severity")
///     .record("pipeline", "true")
///     .build();
/// ```
///
/// Result is a fully-parsed [`PvRequestExpr`] you can `.encode()` to
/// wire bytes or `.to_field_desc()` for further composition.
#[derive(Debug, Clone, Default)]
pub struct PvRequestBuilder {
    expr: PvRequestExpr,
}

impl PvRequestBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a dotted field selector. Repeatable. pvxs `RequestBuilder::field`.
    pub fn field(mut self, path: impl Into<String>) -> Self {
        self.expr.fields.push(path.into());
        self
    }

    /// Set a record-level option (key=value). pvxs `RequestBuilder::record`.
    pub fn record(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.expr.record_options.push((key.into(), value.into()));
        self
    }

    /// Replace the builder state by parsing a pvRequest string in
    /// pvxs syntax (`field(a,b)record[pipeline=true]`). Mirrors
    /// pvxs `RequestBuilder::pvRequest(str)`.
    pub fn pv_request(mut self, expr: &str) -> Result<Self, PvRequestParseError> {
        self.expr = PvRequestExpr::parse(expr)?;
        Ok(self)
    }

    /// Replace the builder state with a hand-built [`PvRequestExpr`].
    /// Mirrors pvxs `RequestBuilder::rawRequest(Value)` — the escape
    /// hatch for callers who already constructed the request tree.
    pub fn raw_request(mut self, expr: PvRequestExpr) -> Self {
        self.expr = expr;
        self
    }

    /// Materialize the parsed expression. Equivalent to chaining
    /// `.encode(big_endian)` on the result.
    pub fn build(self) -> PvRequestExpr {
        self.expr
    }
}

/// Parsed pvRequest expression.
///
/// Captures the field selectors and record options as parsed from a
/// pvxs-style expression (e.g. `field(value,alarm.severity)record[pipeline=true]`).
/// Use [`PvRequestExpr::to_field_desc`] to materialize a wire-encodable
/// [`FieldDesc`] mirror, or [`PvRequestExpr::field_paths`] to extract just
/// the dotted field paths.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PvRequestExpr {
    /// Dotted field paths the caller is interested in. A `None` entry
    /// means "everything"; an empty list means "everything" too.
    pub fields: Vec<String>,
    /// Record-level options (`record[k=v,...]`).
    pub record_options: Vec<(String, String)>,
}

impl PvRequestExpr {
    /// Parse a pvRequest expression. Empty input yields an empty expr
    /// (which translates to `field()` = select-all in pvxs).
    pub fn parse(input: &str) -> Result<Self, PvRequestParseError> {
        let mut p = Parser::new(input);
        let mut out = PvRequestExpr::default();
        p.parse(&mut out)?;
        Ok(out)
    }

    /// True iff the expression selects a specific subset of fields.
    /// (Empty fields list = select-all.)
    pub fn has_field_selectors(&self) -> bool {
        !self.fields.is_empty()
    }

    /// Just the top-level field names (first dotted segment) — useful
    /// when callers want the simple `field(a,b,c)` form. Sub-structure
    /// selectors like `alarm.severity` are flattened to `alarm`.
    pub fn top_level_fields(&self) -> Vec<&str> {
        let mut out = Vec::new();
        for f in &self.fields {
            let head = f.split('.').next().unwrap_or(f);
            if !out.contains(&head) {
                out.push(head);
            }
        }
        out
    }

    /// Build a wire-encodable pvRequest [`FieldDesc`] tree from this
    /// parsed expression. The resulting structure is what callers feed
    /// to [`encode_type_desc`].
    pub fn to_field_desc(&self) -> FieldDesc {
        let inner = if self.fields.is_empty() {
            // empty `field {}` selects all
            FieldDesc::Structure {
                struct_id: String::new(),
                fields: Vec::new(),
            }
        } else {
            FieldDesc::Structure {
                struct_id: String::new(),
                fields: build_nested(&self.fields),
            }
        };
        let mut top_fields: Vec<(String, FieldDesc)> = vec![("field".to_string(), inner)];
        if !self.record_options.is_empty() {
            let opts: Vec<(String, FieldDesc)> = self
                .record_options
                .iter()
                .map(|(k, _v)| {
                    (
                        k.clone(),
                        FieldDesc::Scalar(crate::pvdata::ScalarType::String),
                    )
                })
                .collect();
            top_fields.push((
                "record".to_string(),
                FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: vec![(
                        "_options".to_string(),
                        FieldDesc::Structure {
                            struct_id: String::new(),
                            fields: opts,
                        },
                    )],
                },
            ));
        }
        FieldDesc::Structure {
            struct_id: String::new(),
            fields: top_fields,
        }
    }

    /// Encode this expression as a wire-format pvRequest body: the
    /// type descriptor followed by the full value. pvxs
    /// `clientget.cpp::to_wire_full(R, request)` semantics — the
    /// server decodes both halves so it can read `record._options`
    /// string values like `pipeline=true` or `_filter={...}`.
    ///
    /// Before this carried the value half, record-level options were
    /// silently dropped on the wire — the server saw the type
    /// descriptor (which said "there's a string here named pipeline")
    /// but no value, so `record._options.pipeline` ended up empty and
    /// every option-driven feature degraded to "off" / default.
    pub fn encode(&self, big_endian: bool) -> Vec<u8> {
        let order = if big_endian {
            ByteOrder::Big
        } else {
            ByteOrder::Little
        };
        let desc = self.to_field_desc();
        let value = self.to_pv_field();
        let mut out = Vec::new();
        encode_type_desc(&desc, order, &mut out);
        crate::pvdata::encode::encode_pv_field(&value, &desc, order, &mut out);
        out
    }

    /// Build a [`PvField`] tree matching [`Self::to_field_desc`],
    /// populated with the actual record-option string values. The
    /// `field` subtree carries no values (empty Structure) since
    /// pvRequest field selection is purely structural — only
    /// record-level options have data payload.
    pub fn to_pv_field(&self) -> PvField {
        use crate::pvdata::{PvField, PvStructure, ScalarValue};
        fn empty_nested(desc: &FieldDesc) -> PvField {
            match desc {
                FieldDesc::Structure { struct_id, fields } => {
                    let mut s = PvStructure::new(struct_id);
                    for (name, sub) in fields {
                        s.fields.push((name.clone(), empty_nested(sub)));
                    }
                    PvField::Structure(s)
                }
                _ => PvField::Scalar(ScalarValue::String(String::new())),
            }
        }
        let desc = self.to_field_desc();
        let FieldDesc::Structure {
            struct_id,
            fields: top_fields,
        } = desc
        else {
            // to_field_desc always returns a Structure; fall back to
            // an empty structure for safety.
            return PvField::Structure(PvStructure::new(""));
        };
        let mut top = PvStructure::new(&struct_id);
        for (name, sub_desc) in &top_fields {
            let sub_val = match name.as_str() {
                "field" => empty_nested(sub_desc),
                "record" => {
                    // record._options.{...} carries our string values.
                    let mut record_s = PvStructure::new("");
                    let mut options_s = PvStructure::new("");
                    for (k, v) in &self.record_options {
                        options_s
                            .fields
                            .push((k.clone(), PvField::Scalar(ScalarValue::String(v.clone()))));
                    }
                    record_s
                        .fields
                        .push(("_options".to_string(), PvField::Structure(options_s)));
                    PvField::Structure(record_s)
                }
                _ => empty_nested(sub_desc),
            };
            top.fields.push((name.clone(), sub_val));
        }
        PvField::Structure(top)
    }
}

/// Build a nested-empty-struct tree for a list of dotted field paths.
fn build_nested(paths: &[String]) -> Vec<(String, FieldDesc)> {
    use std::collections::BTreeMap;
    // Group by first segment, recurse on tails. Preserve first-seen order
    // by tracking order separately.
    let mut order: Vec<String> = Vec::new();
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for path in paths {
        let mut split = path.splitn(2, '.');
        let head = split.next().unwrap_or("").to_string();
        let tail = split.next().unwrap_or("").to_string();
        if !groups.contains_key(&head) {
            order.push(head.clone());
        }
        let entry = groups.entry(head).or_default();
        if !tail.is_empty() {
            entry.push(tail);
        }
    }
    let mut out: Vec<(String, FieldDesc)> = Vec::with_capacity(order.len());
    for head in order {
        let tails = groups.remove(&head).unwrap_or_default();
        let child = if tails.is_empty() {
            FieldDesc::Structure {
                struct_id: String::new(),
                fields: Vec::new(),
            }
        } else {
            FieldDesc::Structure {
                struct_id: String::new(),
                fields: build_nested(&tails),
            }
        };
        out.push((head, child));
    }
    out
}

/// Errors from [`PvRequestExpr::parse`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PvRequestParseError {
    #[error("unexpected character at position {pos}: {chr}")]
    UnexpectedChar { pos: usize, chr: String },
    #[error("expected '{want}' at position {pos}, got '{got}'")]
    Expected {
        pos: usize,
        want: String,
        got: String,
    },
    #[error("invalid identifier at position {pos}")]
    InvalidIdent { pos: usize },
    #[error("unterminated bracket at position {pos}")]
    Unterminated { pos: usize },
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Comma,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Equal,
    Field,
    Record,
    Name(String),
    Eof,
}

struct Parser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn peek_char(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    fn advance(&mut self, n: usize) {
        self.pos += n;
    }

    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek_char() {
            if c.is_whitespace() {
                self.advance(c.len_utf8());
            } else {
                break;
            }
        }
    }

    fn lex(&mut self) -> Result<Token, PvRequestParseError> {
        self.skip_whitespace();
        let Some(c) = self.peek_char() else {
            return Ok(Token::Eof);
        };
        match c {
            ',' => {
                self.advance(1);
                Ok(Token::Comma)
            }
            '(' => {
                self.advance(1);
                Ok(Token::LParen)
            }
            ')' => {
                self.advance(1);
                Ok(Token::RParen)
            }
            '[' => {
                self.advance(1);
                Ok(Token::LBracket)
            }
            ']' => {
                self.advance(1);
                Ok(Token::RBracket)
            }
            '{' => {
                self.advance(1);
                Ok(Token::LBrace)
            }
            '}' => {
                self.advance(1);
                Ok(Token::RBrace)
            }
            '=' => {
                self.advance(1);
                Ok(Token::Equal)
            }
            _ if is_ident_start(c) => {
                let start = self.pos;
                while let Some(c) = self.peek_char() {
                    if is_ident(c) {
                        self.advance(c.len_utf8());
                    } else {
                        break;
                    }
                }
                let s = &self.input[start..self.pos];
                Ok(match s {
                    "field" => Token::Field,
                    "record" => Token::Record,
                    other => Token::Name(other.to_string()),
                })
            }
            _ => Err(PvRequestParseError::UnexpectedChar {
                pos: self.pos,
                chr: c.to_string(),
            }),
        }
    }

    fn parse(&mut self, out: &mut PvRequestExpr) -> Result<(), PvRequestParseError> {
        loop {
            let tok = self.lex()?;
            match tok {
                Token::Eof => break,
                Token::Field => {
                    self.expect(Token::LParen)?;
                    self.parse_field_list(out)?;
                    // parse_field_list consumed up through RParen
                }
                Token::Record => {
                    self.expect(Token::LBracket)?;
                    self.parse_options(out)?;
                }
                Token::Name(s) => {
                    // Bare-name short-hand for `field(name)`. pvDataCPP also
                    // allows a brace group to follow a bare name here, e.g.
                    // `value{a,b}` — treat it as `field(value{a,b})`.
                    self.parse_field_entry(s, "", out)?;
                }
                other => {
                    return Err(PvRequestParseError::UnexpectedChar {
                        pos: self.pos,
                        chr: format!("{other:?}"),
                    });
                }
            }
        }
        Ok(())
    }

    /// Parse the comma-separated entry list of a top-level `field(...)`,
    /// terminated by `RParen`.
    ///
    /// Each entry is a dotted/brace path. Every leaf path pushed into
    /// `out.fields` is the entry's fully-joined dotted form, so the brace
    /// dialect `field(v{a,b})` and the dotted dialect `field(v.a,v.b)`
    /// both yield the identical flat path list `["v.a", "v.b"]`.
    /// An immediately-closing `field()` is the valid select-all form.
    fn parse_field_list(&mut self, out: &mut PvRequestExpr) -> Result<(), PvRequestParseError> {
        loop {
            let tok = self.lex()?;
            match tok {
                Token::RParen => return Ok(()),
                Token::Comma => {
                    // A leading or doubled comma is tolerated (matches the
                    // lenient pvxs `parse_fields` loop).
                    continue;
                }
                Token::Name(s) => {
                    self.parse_field_entry(s, "", out)?;
                }
                Token::Eof => {
                    return Err(PvRequestParseError::Unterminated { pos: self.pos });
                }
                other => {
                    return Err(PvRequestParseError::UnexpectedChar {
                        pos: self.pos,
                        chr: format!("{other:?}"),
                    });
                }
            }
        }
    }

    /// Parse a single field entry whose leading name has already been
    /// lexed as `name`. `prefix` is the dotted ancestry above it.
    ///
    /// An entry is `name` optionally followed by a brace member group
    /// `{ sub-entry-list }`. The name itself may be a dotted path
    /// (`alarm.severity`) since the lexer treats `.` as part of a name;
    /// such a dotted name simply becomes part of the joined prefix.
    ///
    /// - Bare `name` (no brace)         → push `prefix + name`.
    /// - `name{a,b}`                    → recurse with prefix `name`.
    /// - `name{a{c},b}`                 → fully nested recursion.
    ///
    /// After the entry (and its optional brace group) is consumed, the
    /// caller's list loop resumes at the following `,` / terminator.
    fn parse_field_entry(
        &mut self,
        name: String,
        prefix: &str,
        out: &mut PvRequestExpr,
    ) -> Result<(), PvRequestParseError> {
        let joined = join_path(prefix, &name);
        // Look ahead: a `{` immediately after the name opens a member
        // group; anything else means this entry is a leaf path.
        let save = self.pos;
        let tok = self.lex()?;
        if tok == Token::LBrace {
            // Nested member group. Recurse, parsing the sub-list with the
            // current joined path as the new prefix.
            self.parse_brace_group(&joined, out)?;
        } else {
            // Not a brace group — this entry is a complete leaf path.
            // Rewind so the caller's list loop sees the terminator/comma.
            self.pos = save;
            out.fields.push(joined);
        }
        Ok(())
    }

    /// Parse the body of a `{ ... }` member group whose opening `{` has
    /// already been consumed. `prefix` is the dotted ancestry that every
    /// sub-entry hangs off of.
    fn parse_brace_group(
        &mut self,
        prefix: &str,
        out: &mut PvRequestExpr,
    ) -> Result<(), PvRequestParseError> {
        let mut first = true;
        loop {
            let tok = self.lex()?;
            match tok {
                Token::RBrace => {
                    if first {
                        return Err(PvRequestParseError::UnexpectedChar {
                            pos: self.pos,
                            chr: "empty {}".to_string(),
                        });
                    }
                    return Ok(());
                }
                Token::Comma => continue,
                Token::Name(s) => {
                    first = false;
                    self.parse_field_entry(s, prefix, out)?;
                }
                Token::Eof => {
                    return Err(PvRequestParseError::Unterminated { pos: self.pos });
                }
                other => {
                    return Err(PvRequestParseError::UnexpectedChar {
                        pos: self.pos,
                        chr: format!("{other:?}"),
                    });
                }
            }
        }
    }

    fn parse_options(&mut self, out: &mut PvRequestExpr) -> Result<(), PvRequestParseError> {
        loop {
            let tok = self.lex()?;
            match tok {
                Token::RBracket => return Ok(()),
                Token::Comma => continue,
                Token::Eof => {
                    return Err(PvRequestParseError::Unterminated { pos: self.pos });
                }
                Token::Name(key) => {
                    self.expect(Token::Equal)?;
                    let val_tok = self.lex()?;
                    let val = match val_tok {
                        Token::Name(v) => v,
                        other => {
                            return Err(PvRequestParseError::Expected {
                                pos: self.pos,
                                want: "value".into(),
                                got: format!("{other:?}"),
                            });
                        }
                    };
                    out.record_options.push((key, val));
                }
                other => {
                    return Err(PvRequestParseError::UnexpectedChar {
                        pos: self.pos,
                        chr: format!("{other:?}"),
                    });
                }
            }
        }
    }

    fn expect(&mut self, expected: Token) -> Result<(), PvRequestParseError> {
        let pos = self.pos;
        let tok = self.lex()?;
        if std::mem::discriminant(&tok) == std::mem::discriminant(&expected) {
            Ok(())
        } else {
            Err(PvRequestParseError::Expected {
                pos,
                want: format!("{expected:?}"),
                got: format!("{tok:?}"),
            })
        }
    }
}

/// Join a dotted `prefix` with a `segment`, both of which may themselves
/// be dotted paths. An empty prefix yields the segment unchanged so the
/// brace and dotted dialects produce identical flat paths:
/// `field(v{a,b})` → `v.a`,`v.b` ; `field(v.a,v.b)` → `v.a`,`v.b`.
fn join_path(prefix: &str, segment: &str) -> String {
    if prefix.is_empty() {
        segment.to_string()
    } else if segment.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}.{segment}")
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c.is_ascii_digit()
}

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '.'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pv_request_starts_with_structure_tag() {
        let bytes = build_pv_request(false);
        assert_eq!(bytes[0], 0x80);
    }

    #[test]
    fn value_only_request_is_shorter() {
        let full = build_pv_request(false);
        let value_only = build_pv_request_value_only(false);
        assert!(value_only.len() < full.len());
    }

    // ── pvRequest expression parser ──────────────────────────────────

    fn fields(expr: &str) -> Vec<String> {
        PvRequestExpr::parse(expr).expect("parse ok").fields
    }

    #[test]
    fn parses_dotted_dialect() {
        assert_eq!(fields("field(value,alarm.severity)"), ["value", "alarm.severity"]);
    }

    #[test]
    fn parses_brace_member_group() {
        // pvDataCPP dialect: `value{a,b}` == `value.a, value.b`.
        assert_eq!(
            fields("field(value{LMT_l,LTM_h})"),
            ["value.LMT_l", "value.LTM_h"]
        );
    }

    #[test]
    fn brace_and_dotted_dialects_are_equivalent() {
        // The pvxs issue #156 core requirement: the two dialects must
        // produce the same field selection.
        let dotted = PvRequestExpr::parse("field(value.LMT_l,value.LTM_h)").unwrap();
        let brace = PvRequestExpr::parse("field(value{LMT_l,LTM_h})").unwrap();
        assert_eq!(dotted, brace);
        assert_eq!(dotted.to_field_desc(), brace.to_field_desc());
    }

    #[test]
    fn nested_brace_groups() {
        // `a{b{c,d},e}` == `a.b.c, a.b.d, a.e`.
        assert_eq!(
            fields("field(a{b{c,d},e})"),
            ["a.b.c", "a.b.d", "a.e"]
        );
    }

    #[test]
    fn nested_brace_equivalent_to_dotted() {
        let brace = PvRequestExpr::parse("field(a{b{c,d},e})").unwrap();
        let dotted = PvRequestExpr::parse("field(a.b.c,a.b.d,a.e)").unwrap();
        assert_eq!(brace, dotted);
        assert_eq!(brace.to_field_desc(), dotted.to_field_desc());
    }

    #[test]
    fn dotted_name_with_trailing_brace_group() {
        // A dotted prefix segment may itself carry a brace group.
        assert_eq!(
            fields("field(a.b{c,d})"),
            ["a.b.c", "a.b.d"]
        );
        // …and that equals the fully-dotted spelling.
        assert_eq!(
            PvRequestExpr::parse("field(a.b{c,d})").unwrap(),
            PvRequestExpr::parse("field(a.b.c,a.b.d)").unwrap()
        );
    }

    #[test]
    fn brace_group_inside_dotted_prefix_then_more() {
        // Mixed dialect: a brace group followed by a sibling at the
        // top level.
        assert_eq!(
            fields("field(value{a,b},timeStamp)"),
            ["value.a", "value.b", "timeStamp"]
        );
    }

    #[test]
    fn deeply_nested_braces() {
        assert_eq!(
            fields("field(a{b{c{d,e},f},g})"),
            ["a.b.c.d", "a.b.c.e", "a.b.f", "a.g"]
        );
    }

    #[test]
    fn brace_group_on_bare_name_shorthand() {
        // pvDataCPP allows the brace group without a `field(...)` wrapper.
        assert_eq!(fields("value{a,b}"), ["value.a", "value.b"]);
        assert_eq!(
            PvRequestExpr::parse("value{a,b}").unwrap(),
            PvRequestExpr::parse("field(value.a,value.b)").unwrap()
        );
    }

    #[test]
    fn brace_groups_coexist_with_record_options() {
        let expr =
            PvRequestExpr::parse("field(value{a,b})record[pipeline=true]").unwrap();
        assert_eq!(expr.fields, ["value.a", "value.b"]);
        assert_eq!(
            expr.record_options,
            [("pipeline".to_string(), "true".to_string())]
        );
    }

    #[test]
    fn whitespace_inside_brace_group() {
        assert_eq!(
            fields("field( value { a , b } )"),
            ["value.a", "value.b"]
        );
    }

    #[test]
    fn empty_brace_group_is_rejected() {
        // pvDataCPP rejects `{}` ("empty {}").
        assert!(PvRequestExpr::parse("field(value{})").is_err());
    }

    #[test]
    fn empty_nested_brace_group_is_rejected() {
        assert!(PvRequestExpr::parse("field(a{b{}})").is_err());
    }

    #[test]
    fn unterminated_brace_group_is_error() {
        assert!(PvRequestExpr::parse("field(value{a,b").is_err());
    }

    #[test]
    fn brace_dialect_encodes_same_field_desc_as_dotted() {
        // End-to-end: the wire FieldDesc tree must be byte-identical for
        // equivalent dotted and brace expressions.
        let dotted = PvRequestExpr::parse("field(a.b,a.c,d)").unwrap();
        let brace = PvRequestExpr::parse("field(a{b,c},d)").unwrap();
        assert_eq!(dotted.to_field_desc(), brace.to_field_desc());
        assert_eq!(dotted.encode(false), brace.encode(false));
        assert_eq!(dotted.encode(true), brace.encode(true));
    }

    #[test]
    fn brace_group_repeated_subfield_dedups_in_nested_tree() {
        // `a{b,b.c}` → paths a.b and a.b.c; build_nested must merge them
        // under a single `a` → `b` node, same as the dotted spelling.
        let brace = PvRequestExpr::parse("field(a{b,b.c})").unwrap();
        let dotted = PvRequestExpr::parse("field(a.b,a.b.c)").unwrap();
        assert_eq!(brace.fields, ["a.b", "a.b.c"]);
        assert_eq!(brace.to_field_desc(), dotted.to_field_desc());
    }

    #[test]
    fn builder_pv_request_accepts_brace_dialect() {
        let built = PvRequestBuilder::new()
            .pv_request("field(value{LMT_l,LTM_h})")
            .expect("parse ok")
            .build();
        assert_eq!(built.fields, ["value.LMT_l", "value.LTM_h"]);
    }
}
