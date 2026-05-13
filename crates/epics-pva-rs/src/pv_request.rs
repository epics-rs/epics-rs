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
                    out.fields.push(s);
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

    fn parse_field_list(&mut self, out: &mut PvRequestExpr) -> Result<(), PvRequestParseError> {
        loop {
            let tok = self.lex()?;
            match tok {
                Token::RParen => return Ok(()),
                Token::Comma => continue,
                Token::Name(s) => {
                    out.fields.push(s);
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
}
