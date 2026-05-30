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
use crate::pvdata::{FieldDesc, PvField, ScalarValue};

/// Build a pvRequest selecting `fields` at the top level of "field(...)".
fn build(fields: &[&str], order: ByteOrder) -> Vec<u8> {
    // Split dotted paths (`value.index`) into nested sub-structures via
    // `build_nested` so the server's `request_to_mask` resolves them
    // field-by-field. A flat member literally named `"value.index"`
    // matches no top-level field, and a strict server rejects
    // that as an empty mask ("pvRequest selected no existing fields")
    // instead of falling back to all-fields.
    let owned: Vec<String> = fields.iter().map(|s| s.to_string()).collect();
    let inner = FieldDesc::Structure {
        struct_id: String::new(),
        fields: build_nested(&owned),
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

/// build a pvRequest that includes
/// `record._options.pipeline = "true"` and
/// `record._options.queueSize = "<queue_size>"` alongside the
/// requested `fields`. pvxs `servermon.cpp:523-552` only enables the
/// server-side credit window when these options are present in the
/// pvRequest. Pre-fix Rust set `pipeline_size` on the client
/// context but never injected the options, so the server ran in
/// default no-window mode and a Rust↔Rust monitor was effectively
/// un-flow-controlled. The body and the `descriptor + value` shape
/// match what pvxs `clientreq.cpp` emits when the caller does
/// `.record("pipeline", true).record("queueSize", N)`.
pub fn build_pv_request_pipeline(fields: &[&str], queue_size: u32, big_endian: bool) -> Vec<u8> {
    let order = if big_endian {
        ByteOrder::Big
    } else {
        ByteOrder::Little
    };
    build_with_pipeline(fields, queue_size, order)
}

fn build_with_pipeline(fields: &[&str], queue_size: u32, order: ByteOrder) -> Vec<u8> {
    use crate::pvdata::PvStructure;
    use crate::pvdata::ScalarValue;
    use crate::pvdata::encode::encode_pv_field;

    // pvRequest type:
    //   structure {
    //     structure field { ...<fields>... }
    //     structure record {
    //       structure _options {
    //         string pipeline
    //         string queueSize
    //       }
    //     }
    //   }
    // Dotted paths must nest — see `build()`.
    let owned: Vec<String> = fields.iter().map(|s| s.to_string()).collect();
    let field_struct = FieldDesc::Structure {
        struct_id: String::new(),
        fields: build_nested(&owned),
    };
    // Typed options, matching pvxs `.record("pipeline", true)` /
    // `.record("queueSize", N)` (clientreq.cpp:312-323): `bool pipeline`
    // and `uint queueSize`, not the string form a parsed `record[...]`
    // would yield.
    let options_struct = FieldDesc::Structure {
        struct_id: String::new(),
        fields: vec![
            (
                "pipeline".to_string(),
                FieldDesc::Scalar(crate::pvdata::ScalarType::Boolean),
            ),
            (
                "queueSize".to_string(),
                FieldDesc::Scalar(crate::pvdata::ScalarType::UInt),
            ),
        ],
    };
    let record_struct = FieldDesc::Structure {
        struct_id: String::new(),
        fields: vec![("_options".to_string(), options_struct.clone())],
    };
    let pv_request_desc = FieldDesc::Structure {
        struct_id: String::new(),
        fields: vec![
            ("field".to_string(), field_struct.clone()),
            ("record".to_string(), record_struct),
        ],
    };

    // pvRequest value (descriptor + full value per
    // `from_wire_type_value`). Mirror `field_struct` so a nested dotted
    // path produces a matching nested value body.
    let field_value = empty_struct_value(&field_struct);
    let options_value = PvField::Structure(PvStructure {
        struct_id: String::new(),
        fields: vec![
            (
                "pipeline".to_string(),
                PvField::Scalar(ScalarValue::Boolean(true)),
            ),
            (
                "queueSize".to_string(),
                PvField::Scalar(ScalarValue::UInt(queue_size)),
            ),
        ],
    });
    let record_value = PvField::Structure(PvStructure {
        struct_id: String::new(),
        fields: vec![("_options".to_string(), options_value)],
    });
    let pv_request_value = PvField::Structure(PvStructure {
        struct_id: String::new(),
        fields: vec![
            ("field".to_string(), field_value),
            ("record".to_string(), record_value),
        ],
    });

    let mut out = Vec::new();
    encode_type_desc(&pv_request_desc, order, &mut out);
    encode_pv_field(&pv_request_value, &pv_request_desc, order, &mut out);
    out
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
        // `field` is a sub-structure → walk its children below. pvxs
        // `request2mask` (pvrequest.cpp): the `fields.type()==Struct`
        // branch.
        Some((_, FieldDesc::Structure { fields, .. })) => fields,
        // No `field` entry at all (e.g. the standard "empty pvRequest"
        // the Rust client sends as a 6-byte 0xFD-cached empty struct).
        // pvxs `else if(!fields.valid()) foundrequested = true` →
        // wildcard: select the whole structure.
        None => {
            let total = value_desc.total_bits();
            for i in 0..total {
                mask.set(i);
            }
            return Ok(mask);
        }
        // `field` present but NOT a sub-structure (e.g. a scalar). pvxs's
        // trailing `else` leaves `foundrequested == false`, so it throws
        // "pvRequest must select at least one field". Mirror that as an
        // error rather than silently widening to a wildcard.
        Some(_) => return Err(RequestMaskError::EmptyMask),
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
    mask.set(0); // root — pvxs `testpvreq.cpp` request/selection-mask parity
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
///     .record("pipeline", true)
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

    /// Set a record-level option with a *typed* scalar value. Generic
    /// over [`Into<ScalarValue>`] (bool, the integer types, f32/f64,
    /// `&str`/`String`) so the wire descriptor carries the caller's real
    /// type — `record("pipeline", true)` emits `bool pipeline = true`,
    /// `record("queueSize", 8u32)` emits `uint queueSize = 8`. pvxs
    /// `CommonBuilder::record<T>` (client.h:661-675). Distinct from the
    /// string-parser path ([`pv_request`](Self::pv_request)), where every
    /// option value is a string.
    pub fn record(mut self, key: impl Into<String>, value: impl Into<ScalarValue>) -> Self {
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

    /// Materialize the parsed expression. Equivalent to chaining
    /// `.encode(big_endian)` on the result.
    pub fn build(self) -> PvRequestExpr {
        self.expr
    }
}

/// A hand-assembled raw pvRequest: an explicit descriptor + value tree
/// that bypasses the [`PvRequestExpr`] field/option assembly. This is
/// the true pvxs `rawRequest(Value)` escape hatch (client.h:683) — for
/// requests `PvRequestExpr` cannot express (arbitrary typed value trees,
/// nested non-option structures). The former `PvRequestBuilder::raw_request`
/// only re-seated a parsed `PvRequestExpr`, so it could never carry a
/// hand-built typed `Value`; `PvRequestExpr` is now one construction path
/// rather than the only raw representation.
#[derive(Debug, Clone, PartialEq)]
pub struct RawPvRequest {
    /// The pvRequest type descriptor (root structure).
    pub desc: FieldDesc,
    /// The pvRequest value tree, structurally matching `desc`.
    pub value: PvField,
}

impl RawPvRequest {
    /// Wrap a caller-built descriptor + value pair.
    pub fn new(desc: FieldDesc, value: PvField) -> Self {
        Self { desc, value }
    }

    /// Encode to wire-format pvRequest bytes: the type descriptor
    /// followed by the full value, exactly as [`PvRequestExpr::encode`]
    /// does for the assembled path.
    pub fn encode(&self, big_endian: bool) -> Vec<u8> {
        let order = if big_endian {
            ByteOrder::Big
        } else {
            ByteOrder::Little
        };
        let mut out = Vec::new();
        encode_type_desc(&self.desc, order, &mut out);
        crate::pvdata::encode::encode_pv_field(&self.value, &self.desc, order, &mut out);
        out
    }
}

/// Parsed pvRequest expression.
///
/// Captures the field selectors and record options as parsed from a
/// pvxs-style expression (e.g. `field(value,alarm.severity)record[pipeline=true]`).
/// Use [`PvRequestExpr::to_field_desc`] to materialize a wire-encodable
/// [`FieldDesc`] mirror, or read [`PvRequestExpr::fields`] for just the
/// dotted field paths.
///
/// `Eq` is intentionally not derived: `record_options` now holds typed
/// [`ScalarValue`]s, which include `f32`/`f64` and so are only
/// `PartialEq`. `assert_eq!`-style comparisons (`PartialEq`) are
/// unaffected.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PvRequestExpr {
    /// Dotted field paths the caller is interested in. A `None` entry
    /// means "everything"; an empty list means "everything" too.
    pub fields: Vec<String>,
    /// Record-level options (`record[k=v,...]`), each carrying a *typed*
    /// scalar value. pvxs `CommonBuilder::record<T>` preserves the
    /// caller's scalar type (`bool pipeline = true`, `uint queueSize = 8`)
    /// via `Value::Helper::build` (client.h:661-675, clientreq.cpp:85-90);
    /// the string-parser path (`record[pipeline=true]`) instead yields
    /// `String("true")` so the typed-builder and parsed-text wire shapes
    /// stay distinct, exactly as pvxs `testpvreq.cpp:172-256` requires.
    pub record_options: Vec<(String, ScalarValue)>,
    /// Per-field option brackets, mirroring pvDataCPP `createRequest`'s
    /// `field(value[deadband=abs:1.0])` / `field(value[array=1:3])`
    /// syntax. Each entry pairs a dotted field path with its
    /// `(key, value)` options; in the wire structure the options land
    /// under that field's `_options` sub-structure. Option *values*
    /// may contain non-identifier characters (`:`, `.`, digits) so
    /// `abs:1.0` and `1:3` round-trip verbatim.
    pub field_options: Vec<(String, Vec<(String, String)>)>,
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

    /// Parse using the strict pvxs `PVRParser` grammar
    /// (`/Users/stevek/codes/epics-modules/pvxs/src/clientreq.cpp:137-283`).
    ///
    /// This rejects the lenient pvDataCPP extensions that [`parse`] accepts
    /// — brace member groups (`field(v{a,b})`), per-field option brackets
    /// (`field(v[deadband=abs:1.0])`), quoted option values, and option
    /// values containing `:`/`-`/`+` — so a request string accepted here is
    /// also accepted by pvxs `RequestBuilder::pvRequest()`. Use it on call
    /// sites that advertise pvxs request-builder compatibility; use
    /// [`parse`] where the extended pvData filter syntax is intended.
    ///
    /// [`parse`]: Self::parse
    pub fn parse_pvxs_compat(input: &str) -> Result<Self, PvRequestParseError> {
        let mut p = Parser::new_strict(input);
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
        // A field may carry per-field options without otherwise being
        // a selected path (`field(value[deadband=abs:1.0])` selects
        // `value` *and* options it). Fold every field-option path into
        // the selector set so the nested tree always has a node to
        // hang the `_options` sub-structure off of.
        let inner = if self.fields.is_empty() && self.field_options.is_empty() {
            // empty `field {}` selects all
            FieldDesc::Structure {
                struct_id: String::new(),
                fields: Vec::new(),
            }
        } else {
            let mut paths = self.fields.clone();
            for (path, _) in &self.field_options {
                if !paths.contains(path) {
                    paths.push(path.clone());
                }
            }
            let mut tree = build_nested(&paths);
            for (path, opts) in &self.field_options {
                attach_field_options(&mut tree, path, opts);
            }
            FieldDesc::Structure {
                struct_id: String::new(),
                fields: tree,
            }
        };
        let mut top_fields: Vec<(String, FieldDesc)> = vec![("field".to_string(), inner)];
        if !self.record_options.is_empty() {
            // Each option's descriptor carries its *own* scalar type
            // (pvxs `TypeDef(pair.second).as(pair.first)`,
            // clientreq.cpp:312-316), so a typed `bool pipeline` stays a
            // bool on the wire rather than collapsing to `string`.
            let opts: Vec<(String, FieldDesc)> = self
                .record_options
                .iter()
                .map(|(k, v)| (k.clone(), FieldDesc::Scalar(v.scalar_type())))
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
    /// populated with the actual option string values. Field selection
    /// itself is purely structural (empty sub-structures), but both
    /// record-level options and per-field option brackets carry string
    /// payload — `field(value[deadband=abs:1.0])` puts `"abs:1.0"`
    /// under `field.value._options.deadband`.
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
        // Fill the `field` subtree: empty structs everywhere, except an
        // `_options` member carries the per-field option values. `path`
        // is the dotted ancestry of `desc` below the `field` root, used
        // to look the field's options up.
        fn fill_field(desc: &FieldDesc, path: &str, expr: &PvRequestExpr) -> PvField {
            match desc {
                FieldDesc::Structure { struct_id, fields } => {
                    let mut s = PvStructure::new(struct_id);
                    for (name, sub) in fields {
                        if name == "_options" {
                            // The options belong to the *enclosing*
                            // field, identified by `path`.
                            let opts = expr
                                .field_options
                                .iter()
                                .find(|(p, _)| p == path)
                                .map(|(_, o)| o.as_slice())
                                .unwrap_or(&[]);
                            let mut opt_s = PvStructure::new("");
                            for (k, v) in opts {
                                opt_s.fields.push((
                                    k.clone(),
                                    PvField::Scalar(ScalarValue::String(v.clone())),
                                ));
                            }
                            s.fields
                                .push(("_options".to_string(), PvField::Structure(opt_s)));
                        } else {
                            let child_path = if path.is_empty() {
                                name.clone()
                            } else {
                                format!("{path}.{name}")
                            };
                            s.fields
                                .push((name.clone(), fill_field(sub, &child_path, expr)));
                        }
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
                "field" => fill_field(sub_desc, "", self),
                "record" => {
                    // record._options.{...} carries our string values.
                    let mut record_s = PvStructure::new("");
                    let mut options_s = PvStructure::new("");
                    for (k, v) in &self.record_options {
                        // The typed value goes on the wire verbatim,
                        // matching the per-option descriptor emitted by
                        // `to_field_desc` (pvxs `opt[name].assign(value)`,
                        // clientreq.cpp:320-322).
                        options_s
                            .fields
                            .push((k.clone(), PvField::Scalar(v.clone())));
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

/// Mirror a [`FieldDesc`] into an all-empty value [`PvField`]: every
/// `Structure` becomes an empty-membered `PvStructure` (recursively),
/// every scalar an empty string. Used to build the pvRequest value body
/// that must structurally match a generated descriptor.
fn empty_struct_value(desc: &FieldDesc) -> crate::pvdata::PvField {
    use crate::pvdata::{PvField, PvStructure, ScalarValue};
    match desc {
        FieldDesc::Structure { struct_id, fields } => {
            let mut s = PvStructure::new(struct_id);
            for (name, sub) in fields {
                s.fields.push((name.clone(), empty_struct_value(sub)));
            }
            PvField::Structure(s)
        }
        _ => PvField::Scalar(ScalarValue::String(String::new())),
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

/// Attach a per-field `_options` sub-structure to the node at the
/// dotted `path` inside an already-built nested field tree.
///
/// Mirrors pvDataCPP `createRequest`: `field(value[deadband=abs:1.0])`
/// places the option under `field.value._options.deadband`. The node
/// for `path` is guaranteed to exist because `to_field_desc` folds
/// every field-option path into the selector set before building the
/// tree. Each option is emitted as a `string` member carrying its
/// (possibly non-identifier) value verbatim. An empty option list is
/// a no-op.
fn attach_field_options(tree: &mut [(String, FieldDesc)], path: &str, opts: &[(String, String)]) {
    if opts.is_empty() {
        return;
    }
    let mut split = path.splitn(2, '.');
    let head = split.next().unwrap_or("");
    let tail = split.next();

    let Some((_, child)) = tree.iter_mut().find(|(n, _)| n == head) else {
        // Unreachable in practice (the node was just built), but stay
        // total rather than panic on a malformed call.
        return;
    };
    let FieldDesc::Structure { fields, .. } = child else {
        return;
    };
    match tail {
        Some(rest) => attach_field_options(fields, rest, opts),
        None => {
            let opt_fields: Vec<(String, FieldDesc)> = opts
                .iter()
                .map(|(k, _)| {
                    (
                        k.clone(),
                        FieldDesc::Scalar(crate::pvdata::ScalarType::String),
                    )
                })
                .collect();
            // Replace an existing `_options` (repeated bracket on the
            // same field) rather than appending a duplicate member.
            fields.retain(|(n, _)| n != "_options");
            fields.push((
                "_options".to_string(),
                FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: opt_fields,
                },
            ));
        }
    }
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
    /// When `true`, restrict the accepted grammar to pvxs `PVRParser`
    /// (`/Users/stevek/codes/epics-modules/pvxs/src/clientreq.cpp:137-283`):
    /// names are `[A-Za-z0-9._]+`, the only syntax tokens are `[](),=`,
    /// `field(name,...)` and `record[name=name,...]` are the only shapes,
    /// and option *values* are themselves `name` tokens. That parser has
    /// no brace member groups, no per-field option brackets (its
    /// `parse_fields` accepts only `name`/`,` until `)`), and no quoted or
    /// `:`/`-`/`+`-bearing option values.
    ///
    /// Default (`false`) keeps the lenient pvDataCPP `createRequest`
    /// superset — brace groups, per-field `[...]` option brackets, and
    /// `:`-bearing / quoted option values — because the server-side field
    /// filter and `record[_filter="{...}"]` JSON payloads depend on it.
    strict: bool,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            pos: 0,
            strict: false,
        }
    }

    /// Parser restricted to the pvxs `PVRParser` grammar (see
    /// [`Parser::strict`]).
    fn new_strict(input: &'a str) -> Self {
        Self {
            input,
            pos: 0,
            strict: true,
        }
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
    /// An entry is `name` optionally followed by a member group
    /// `{ sub-entry-list }` and/or a per-field option bracket
    /// `[key=val,...]`. The name itself may be a dotted path
    /// (`alarm.severity`) since the lexer treats `.` as part of a name.
    ///
    /// - Bare `name` (no suffix)        → push `prefix + name`.
    /// - `name{a,b}`                    → recurse with prefix `name`.
    /// - `name{a{c},b}`                 → fully nested recursion.
    /// - `name[deadband=abs:1.0]`       → per-field options on `name`
    ///   (pvDataCPP `createRequest`); the field path is still selected.
    /// - `name{a,b}[opt=v]`             → brace group *and* options.
    ///
    /// After the entry (and any suffix) is consumed, the caller's list
    /// loop resumes at the following `,` / terminator.
    fn parse_field_entry(
        &mut self,
        name: String,
        prefix: &str,
        out: &mut PvRequestExpr,
    ) -> Result<(), PvRequestParseError> {
        let joined = join_path(prefix, &name);
        // Look ahead: a `{` opens a member group, a `[` opens a
        // per-field option bracket; anything else ends the entry.
        let save = self.pos;
        let tok = self.lex()?;
        if tok == Token::LBrace {
            // pvxs `PVRParser::parse_fields` (clientreq.cpp:230-243) accepts
            // only `name`/`,` until `)` — a `{` is outside its grammar.
            if self.strict {
                return Err(PvRequestParseError::UnexpectedChar {
                    pos: save,
                    chr: "{ (brace member group not in pvxs grammar)".to_string(),
                });
            }
            // Nested member group. Recurse with the joined path as the
            // new prefix. The leaf paths are pushed by the recursion.
            self.parse_brace_group(&joined, out)?;
        } else if tok == Token::LBracket {
            // Per-field option brackets are likewise outside pvxs
            // `parse_fields`; pvxs allows options only on `record[...]`.
            if self.strict {
                return Err(PvRequestParseError::UnexpectedChar {
                    pos: save,
                    chr: "[ (per-field option bracket not in pvxs grammar)".to_string(),
                });
            }
            // `name[opt=val,...]` — the field is selected itself, and
            // its options hang under `joined._options`.
            out.fields.push(joined.clone());
            self.parse_field_options(&joined, out)?;
            return Ok(());
        } else {
            // Plain leaf path — rewind so the caller sees the comma /
            // terminator, then check separately for a `[...]` suffix.
            self.pos = save;
            out.fields.push(joined.clone());
        }
        // A `[...]` option bracket may follow a brace group too
        // (`name{a,b}[opt=v]`). It never follows another bracket.
        if tok == Token::LBrace {
            let save2 = self.pos;
            if self.lex()? == Token::LBracket {
                self.parse_field_options(&joined, out)?;
            } else {
                self.pos = save2;
            }
        }
        Ok(())
    }

    /// Parse the body of a per-field option bracket `[key=val,...]`
    /// whose opening `[` has already been consumed, terminated by `]`.
    /// `field_path` is the dotted path the options attach to.
    ///
    /// Option *values* may contain non-identifier characters (`:`,
    /// `.`, digits) — pvDataCPP allows `deadband=abs:1.0`, `array=1:3`
    /// — so values are lexed with the permissive [`Self::lex_value`]
    /// rather than the identifier lexer.
    fn parse_field_options(
        &mut self,
        field_path: &str,
        out: &mut PvRequestExpr,
    ) -> Result<(), PvRequestParseError> {
        let mut opts: Vec<(String, String)> = Vec::new();
        loop {
            let tok = self.lex()?;
            match tok {
                Token::RBracket => break,
                Token::Comma => continue,
                Token::Eof => {
                    return Err(PvRequestParseError::Unterminated { pos: self.pos });
                }
                Token::Name(key) => {
                    self.expect(Token::Equal)?;
                    let val = self.lex_value()?;
                    opts.push((key, val));
                }
                other => {
                    return Err(PvRequestParseError::UnexpectedChar {
                        pos: self.pos,
                        chr: format!("{other:?}"),
                    });
                }
            }
        }
        // Merge into any existing option set for the same field path
        // (`field(value[a=1],value[b=2])` is equivalent to one bracket).
        if let Some((_, existing)) = out.field_options.iter_mut().find(|(p, _)| p == field_path) {
            existing.extend(opts);
        } else if !opts.is_empty() {
            out.field_options.push((field_path.to_string(), opts));
        }
        Ok(())
    }

    /// Lex a per-field / record option *value*.
    ///
    /// Two forms are accepted:
    ///
    /// 1. **Bare run** — alphanumerics, `_`, `.`, `:`, `-`, `+`. An
    ///    option value may contain `:` (`abs:1.0`, `1:3`) and signed
    ///    numbers carry `-`/`+`. The run ends at the first delimiter
    ///    (`,`, `]`, `)`, `}`, whitespace). Used for the common
    ///    `deadband=abs:1.0` / `pipeline=true` shapes.
    ///
    /// 2. **Quoted string** — when the value starts with `"`, scan to
    ///    the matching closing `"`, with `\"`, `\\`, `\n`, `\t`, `\r`
    ///    escapes. This is what lets a record option carry a JSON
    ///    payload such as `record[_filter="{\"dbnd\":{\"d\":0.5}}"]` —
    ///    the bare run rejects `{`, `}`, `,`, and `"` before the JSON
    ///    can be read. The returned string is the unescaped content
    ///    between the quotes.
    ///
    /// An empty bare run is an error — `key=` with no value is
    /// malformed. An empty quoted string (`key=""`) is valid.
    fn lex_value(&mut self) -> Result<String, PvRequestParseError> {
        self.skip_whitespace();
        if self.strict {
            // pvxs `parse_options` (clientreq.cpp:255-268) requires the value
            // to be a `name` token (`[A-Za-z0-9._]+`); quoted strings and
            // `:`/`-`/`+`-bearing runs are outside its grammar.
            return self.lex_value_strict();
        }
        if self.peek_char() == Some('"') {
            return self.lex_quoted_value();
        }
        let start = self.pos;
        while let Some(c) = self.peek_char() {
            if is_value_char(c) {
                self.advance(c.len_utf8());
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(PvRequestParseError::Expected {
                pos: self.pos,
                want: "option value".into(),
                got: self
                    .peek_char()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "EOF".into()),
            });
        }
        Ok(self.input[start..self.pos].to_string())
    }

    /// Lex an option value under the strict pvxs grammar: a single `name`
    /// token (`[A-Za-z0-9._]+`). Rejects quoted strings and any run that
    /// stops on a non-`name` character (`:`, `-`, `+`), matching pvxs
    /// `parse_options` requiring `lextok==name` for the value
    /// (clientreq.cpp:262-263).
    fn lex_value_strict(&mut self) -> Result<String, PvRequestParseError> {
        let start = self.pos;
        while let Some(c) = self.peek_char() {
            if is_ident(c) {
                self.advance(c.len_utf8());
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(PvRequestParseError::Expected {
                pos: self.pos,
                want: "name option value (pvxs grammar)".into(),
                got: self
                    .peek_char()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "EOF".into()),
            });
        }
        Ok(self.input[start..self.pos].to_string())
    }

    /// Lex a double-quoted option value. The opening `"` is expected
    /// at the current position. Recognises `\"`, `\\`, `\n`, `\t`,
    /// `\r` escapes; any other `\x` is a parse error. An unterminated
    /// quote (EOF before the closing `"`) is an error.
    fn lex_quoted_value(&mut self) -> Result<String, PvRequestParseError> {
        // Consume the opening quote.
        debug_assert_eq!(self.peek_char(), Some('"'));
        self.advance('"'.len_utf8());
        let mut out = String::new();
        loop {
            match self.peek_char() {
                None => {
                    return Err(PvRequestParseError::Unterminated { pos: self.pos });
                }
                Some('"') => {
                    self.advance('"'.len_utf8());
                    return Ok(out);
                }
                Some('\\') => {
                    self.advance('\\'.len_utf8());
                    match self.peek_char() {
                        Some(esc @ ('"' | '\\')) => {
                            out.push(esc);
                            self.advance(esc.len_utf8());
                        }
                        Some('n') => {
                            out.push('\n');
                            self.advance(1);
                        }
                        Some('t') => {
                            out.push('\t');
                            self.advance(1);
                        }
                        Some('r') => {
                            out.push('\r');
                            self.advance(1);
                        }
                        Some(other) => {
                            return Err(PvRequestParseError::UnexpectedChar {
                                pos: self.pos,
                                chr: format!("invalid escape \\{other}"),
                            });
                        }
                        None => {
                            return Err(PvRequestParseError::Unterminated { pos: self.pos });
                        }
                    }
                }
                Some(c) => {
                    out.push(c);
                    self.advance(c.len_utf8());
                }
            }
        }
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
                    // Record option values, like per-field option
                    // values, may contain `:` (`record[deadband=abs:1.0]`),
                    // so lex them with the permissive value scanner. The
                    // parsed-text path is always string-typed (pvxs
                    // PVRParser stores `string pipeline = "true"`,
                    // testpvreq.cpp:232-256) — only the typed builder
                    // carries bool/int.
                    let val = self.lex_value()?;
                    out.record_options.push((key, ScalarValue::String(val)));
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

/// Characters allowed inside an option *value* (`field(v[k=val])` /
/// `record[k=val]`). Broader than an identifier: option values like
/// pvDataCPP's `abs:1.0` and `1:3` carry `:`, and signed numbers carry
/// `-`/`+`. The value run ends at the first delimiter (`,`, `]`, `)`,
/// `}`, `=`, whitespace).
fn is_value_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-' | '+')
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

    /// A parsed-text record option: the string parser is always
    /// string-typed (pvxs PVRParser `string k = "v"`), so test
    /// expectations wrap the value in `ScalarValue::String`.
    fn rec_s(key: &str, val: &str) -> (String, ScalarValue) {
        (key.to_string(), ScalarValue::String(val.to_string()))
    }

    /// The `record._options.<name>` member *types* a built pvRequest
    /// carries on the wire. This is where the typed-builder vs
    /// parsed-text distinction is observable: the typed builder emits
    /// `bool`/`uint`, the string parser emits `string`.
    fn option_descs(expr: &PvRequestExpr) -> Vec<(String, crate::pvdata::ScalarType)> {
        let FieldDesc::Structure { fields: top, .. } = expr.to_field_desc() else {
            panic!("pvRequest root is a structure");
        };
        let (_, record) = top
            .iter()
            .find(|(n, _)| n == "record")
            .expect("record sub-structure present");
        let FieldDesc::Structure {
            fields: rfields, ..
        } = record
        else {
            panic!("record is a structure");
        };
        let (_, options) = rfields
            .iter()
            .find(|(n, _)| n == "_options")
            .expect("_options sub-structure present");
        let FieldDesc::Structure {
            fields: ofields, ..
        } = options
        else {
            panic!("_options is a structure");
        };
        ofields
            .iter()
            .map(|(k, d)| {
                let FieldDesc::Scalar(t) = d else {
                    panic!("option {k} must be a scalar descriptor");
                };
                (k.clone(), *t)
            })
            .collect()
    }

    /// The `record._options.<name>` member *values* a built pvRequest
    /// puts on the wire, twin of [`option_descs`].
    fn option_values(expr: &PvRequestExpr) -> Vec<(String, ScalarValue)> {
        let PvField::Structure(top) = expr.to_pv_field() else {
            panic!("pvRequest root value is a structure");
        };
        let (_, record) = top
            .fields
            .iter()
            .find(|(n, _)| n == "record")
            .expect("record value present");
        let PvField::Structure(rfields) = record else {
            panic!("record value is a structure");
        };
        let (_, options) = rfields
            .fields
            .iter()
            .find(|(n, _)| n == "_options")
            .expect("_options value present");
        let PvField::Structure(ofields) = options else {
            panic!("_options value is a structure");
        };
        ofields
            .fields
            .iter()
            .map(|(k, v)| {
                let PvField::Scalar(sv) = v else {
                    panic!("option {k} must be a scalar value");
                };
                (k.clone(), sv.clone())
            })
            .collect()
    }

    #[test]
    fn parses_dotted_dialect() {
        assert_eq!(
            fields("field(value,alarm.severity)"),
            ["value", "alarm.severity"]
        );
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
        assert_eq!(fields("field(a{b{c,d},e})"), ["a.b.c", "a.b.d", "a.e"]);
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
        assert_eq!(fields("field(a.b{c,d})"), ["a.b.c", "a.b.d"]);
        // …and that equals the fully-dotted spelling.
        assert_eq!(
            PvRequestExpr::parse("field(a.b{c,d})").unwrap(),
            PvRequestExpr::parse("field(a.b.c,a.b.d)").unwrap()
        );
    }

    #[test]
    fn build_pv_request_fields_nests_dotted_paths() {
        // Regression: build_pv_request_fields("value.index") must emit a
        // nested `field { value { index {} } }`, not a flat member
        // literally named "value.index". A flat name matches no
        // top-level field and a strict server rejects the
        // PUT INIT with EmptyMask ("pvRequest selected no existing
        // fields") — this broke NTEnum int puts.
        use crate::pvdata::ScalarType;
        use std::io::Cursor;

        let blob = build_pv_request_fields(&["value.index"], false);
        let mut cur = Cursor::new(blob.as_slice());
        let req = crate::pvdata::encode::decode_type_desc(&mut cur, ByteOrder::Little)
            .expect("decode pvRequest descriptor");

        let field = match &req {
            FieldDesc::Structure { fields, .. } => fields
                .iter()
                .find(|(n, _)| n == "field")
                .map(|(_, d)| d)
                .expect("`field` substructure"),
            _ => panic!("pvRequest root must be a structure"),
        };
        let value = match field {
            FieldDesc::Structure { fields, .. } => {
                assert!(
                    fields.iter().all(|(n, _)| n != "value.index"),
                    "dotted path must not appear as a flat member"
                );
                fields
                    .iter()
                    .find(|(n, _)| n == "value")
                    .map(|(_, d)| d)
                    .expect("nested `value` member")
            }
            _ => panic!("`field` must be a structure"),
        };
        match value {
            FieldDesc::Structure { fields, .. } => {
                assert!(
                    fields.iter().any(|(n, _)| n == "index"),
                    "nested `index` member"
                );
            }
            _ => panic!("`value` must be a structure"),
        }

        // The server's request_to_mask must resolve it against an
        // NTEnum-shaped value descriptor without EmptyMask.
        let ntenum = FieldDesc::Structure {
            struct_id: "epics:nt/NTEnum:1.0".to_string(),
            fields: vec![
                (
                    "value".to_string(),
                    FieldDesc::Structure {
                        struct_id: String::new(),
                        fields: vec![("index".to_string(), FieldDesc::Scalar(ScalarType::Int))],
                    },
                ),
                (
                    "alarm".to_string(),
                    FieldDesc::Structure {
                        struct_id: String::new(),
                        fields: Vec::new(),
                    },
                ),
            ],
        };
        assert!(
            request_to_mask(&ntenum, &req).is_ok(),
            "request_to_mask must resolve a nested value.index path"
        );
    }

    #[test]
    fn request_to_mask_field_as_scalar_errors() {
        // pvxs `request2mask` (pvrequest.cpp) — when `field` is
        // present but is NOT a sub-structure, the trailing `else`
        // leaves `foundrequested == false` and it throws "pvRequest
        // must select at least one field". A scalar `field` must error,
        // not silently widen to a wildcard.
        use crate::pvdata::ScalarType;

        let value = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![
                ("value".to_string(), FieldDesc::Scalar(ScalarType::Double)),
                (
                    "alarm".to_string(),
                    FieldDesc::Structure {
                        struct_id: String::new(),
                        fields: Vec::new(),
                    },
                ),
            ],
        };

        // pvRequest whose `field` is a scalar rather than a sub-struct.
        let req_field_scalar = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![("field".to_string(), FieldDesc::Scalar(ScalarType::Int))],
        };
        assert_eq!(
            request_to_mask(&value, &req_field_scalar),
            Err(RequestMaskError::EmptyMask),
            "a non-structure `field` must error, not select all"
        );

        // Control: a pvRequest with NO `field` entry is still the
        // whole-structure wildcard (pvxs `!fields.valid()` branch).
        let req_no_field = FieldDesc::Structure {
            struct_id: String::new(),
            fields: Vec::new(),
        };
        let mask = request_to_mask(&value, &req_no_field).expect("absent field → wildcard");
        for i in 0..value.total_bits() {
            assert!(mask.get(i), "wildcard must set bit {i}");
        }
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
        let expr = PvRequestExpr::parse("field(value{a,b})record[pipeline=true]").unwrap();
        assert_eq!(expr.fields, ["value.a", "value.b"]);
        assert_eq!(expr.record_options, [rec_s("pipeline", "true")]);
    }

    #[test]
    fn whitespace_inside_brace_group() {
        assert_eq!(fields("field( value { a , b } )"), ["value.a", "value.b"]);
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

    // ── typed builder vs parsed-text wire descriptor (pvxs
    //    testpvreq.cpp:172-256: testAssemble's typed `record(...)` vs
    //    testParse*'s string `record[...]`) ───────────────────────────

    #[test]
    fn builder_record_pipeline_emits_typed_bool() {
        // pvxs `CommonBuilder::record("pipeline", true)` →
        // `bool pipeline = true` (client.h:661-675, clientreq.cpp:312-323).
        use crate::pvdata::ScalarType;
        let req = PvRequestBuilder::new().record("pipeline", true).build();
        assert_eq!(req.record_options, [("pipeline".to_string(), true.into())]);
        assert_eq!(
            option_descs(&req),
            [("pipeline".to_string(), ScalarType::Boolean)]
        );
        assert_eq!(
            option_values(&req),
            [("pipeline".to_string(), ScalarValue::Boolean(true))]
        );
    }

    #[test]
    fn builder_record_queue_size_emits_typed_uint() {
        // pvxs `record("queueSize", 8u32)` → `uint queueSize = 8`.
        use crate::pvdata::ScalarType;
        let req = PvRequestBuilder::new().record("queueSize", 8u32).build();
        assert_eq!(
            option_descs(&req),
            [("queueSize".to_string(), ScalarType::UInt)]
        );
        assert_eq!(
            option_values(&req),
            [("queueSize".to_string(), ScalarValue::UInt(8))]
        );
    }

    #[test]
    fn builder_record_block_emits_typed_bool() {
        // pvxs PUT INIT pvRequest carries `bool block` (pvalink_channel.cpp:36).
        use crate::pvdata::ScalarType;
        let req = PvRequestBuilder::new().record("block", true).build();
        assert_eq!(
            option_descs(&req),
            [("block".to_string(), ScalarType::Boolean)]
        );
        assert_eq!(
            option_values(&req),
            [("block".to_string(), ScalarValue::Boolean(true))]
        );
    }

    #[test]
    fn parsed_text_record_pipeline_is_string() {
        // pvxs PVRParser `record[pipeline=true]` → `string pipeline = "true"`
        // (testpvreq.cpp:232-256). The parsed-text path never types options.
        use crate::pvdata::ScalarType;
        let req = PvRequestExpr::parse("record[pipeline=true]").unwrap();
        assert_eq!(req.record_options, [rec_s("pipeline", "true")]);
        assert_eq!(
            option_descs(&req),
            [("pipeline".to_string(), ScalarType::String)]
        );
        assert_eq!(
            option_values(&req),
            [(
                "pipeline".to_string(),
                ScalarValue::String("true".to_string())
            )]
        );
    }

    #[test]
    fn typed_builder_and_parsed_text_pipeline_differ_on_the_wire() {
        // The whole point of PVA-92: `record("pipeline", true)` (typed bool)
        // and `record[pipeline=true]` (parsed string) must produce DISTINCT
        // wire descriptors — same option name, different type code/value.
        use crate::pvdata::ScalarType;
        let typed = PvRequestBuilder::new().record("pipeline", true).build();
        let parsed = PvRequestExpr::parse("record[pipeline=true]").unwrap();

        assert_eq!(
            option_descs(&typed),
            [("pipeline".to_string(), ScalarType::Boolean)]
        );
        assert_eq!(
            option_descs(&parsed),
            [("pipeline".to_string(), ScalarType::String)]
        );
        assert_ne!(option_descs(&typed), option_descs(&parsed));

        // The full encoded pvRequest bytes differ in both byte orders: the
        // type code (0x00 bool vs 0x60 string) and the value body differ.
        assert_ne!(typed.encode(false), parsed.encode(false));
        assert_ne!(typed.encode(true), parsed.encode(true));
    }

    // ── per-field option brackets (pvDataCPP createRequest) ──────────

    #[test]
    fn parses_per_field_option_bracket() {
        // pvDataCPP dialect: `field(value[deadband=abs:1.0])` selects
        // `value` and attaches the `deadband` option to it.
        let expr = PvRequestExpr::parse("field(value[deadband=abs:1.0])").unwrap();
        assert_eq!(expr.fields, ["value"]);
        assert_eq!(
            expr.field_options,
            [(
                "value".to_string(),
                vec![("deadband".to_string(), "abs:1.0".to_string())]
            )]
        );
    }

    #[test]
    fn per_field_option_value_keeps_colon_and_range() {
        // `array=1:3` — the value carries a `:` and digits.
        let expr = PvRequestExpr::parse("field(value[array=1:3])").unwrap();
        assert_eq!(
            expr.field_options[0].1,
            [("array".to_string(), "1:3".to_string())]
        );
    }

    #[test]
    fn per_field_multiple_options_in_one_bracket() {
        let expr = PvRequestExpr::parse("field(value[deadband=abs:1.0,array=1:3])").unwrap();
        assert_eq!(
            expr.field_options[0].1,
            [
                ("deadband".to_string(), "abs:1.0".to_string()),
                ("array".to_string(), "1:3".to_string()),
            ]
        );
    }

    #[test]
    fn per_field_options_land_under_field_options_substruct() {
        // pvDataCPP `createRequest` puts the option under
        // `field.value._options.deadband` as a string member.
        use crate::pvdata::{FieldDesc, ScalarType};
        let expr = PvRequestExpr::parse("field(value[deadband=abs:1.0])").unwrap();
        let FieldDesc::Structure { fields: top, .. } = expr.to_field_desc() else {
            panic!("top must be a structure");
        };
        let (_, field_desc) = top.iter().find(|(n, _)| n == "field").unwrap();
        let FieldDesc::Structure { fields: fdesc, .. } = field_desc else {
            panic!("field must be a structure");
        };
        let (_, value_desc) = fdesc.iter().find(|(n, _)| n == "value").unwrap();
        let FieldDesc::Structure {
            fields: vfields, ..
        } = value_desc
        else {
            panic!("value must be a structure");
        };
        let (_, opts_desc) = vfields
            .iter()
            .find(|(n, _)| n == "_options")
            .expect("value must carry an _options sub-structure");
        let FieldDesc::Structure {
            fields: ofields, ..
        } = opts_desc
        else {
            panic!("_options must be a structure");
        };
        assert_eq!(ofields.len(), 1);
        assert_eq!(ofields[0].0, "deadband");
        assert!(matches!(
            ofields[0].1,
            FieldDesc::Scalar(ScalarType::String)
        ));
    }

    #[test]
    fn per_field_option_value_is_carried_in_pv_field() {
        // The encoded value half must carry the actual option string
        // under `field.value._options.deadband`, not an empty string.
        use crate::pvdata::{PvField, ScalarValue};
        let expr = PvRequestExpr::parse("field(value[deadband=abs:1.0])").unwrap();
        let PvField::Structure(top) = expr.to_pv_field() else {
            panic!("top struct");
        };
        let field = top.fields.iter().find(|(n, _)| n == "field").unwrap();
        let PvField::Structure(fs) = &field.1 else {
            panic!("field struct");
        };
        let value = fs.fields.iter().find(|(n, _)| n == "value").unwrap();
        let PvField::Structure(vs) = &value.1 else {
            panic!("value struct");
        };
        let opts = vs.fields.iter().find(|(n, _)| n == "_options").unwrap();
        let PvField::Structure(os) = &opts.1 else {
            panic!("_options struct");
        };
        let deadband = os.fields.iter().find(|(n, _)| n == "deadband").unwrap();
        assert!(matches!(
            &deadband.1,
            PvField::Scalar(ScalarValue::String(s)) if s == "abs:1.0"
        ));
    }

    #[test]
    fn per_field_options_on_dotted_path() {
        // The option bracket may follow a dotted sub-field path.
        let expr = PvRequestExpr::parse("field(alarm.severity[deadband=abs:2])").unwrap();
        assert_eq!(expr.fields, ["alarm.severity"]);
        assert_eq!(
            expr.field_options,
            [(
                "alarm.severity".to_string(),
                vec![("deadband".to_string(), "abs:2".to_string())]
            )]
        );
    }

    #[test]
    fn per_field_options_after_brace_group() {
        // `value{a,b}[opt=v]` — brace group selects `value.a`/`value.b`
        // and the bracket attaches an option to `value` itself. `value`
        // is not a selected leaf, but `to_field_desc` still materializes
        // a `value` node so the `_options` sub-struct has a home.
        let expr = PvRequestExpr::parse("field(value{a,b}[pipeline=true])").unwrap();
        assert_eq!(expr.fields, ["value.a", "value.b"]);
        assert_eq!(
            expr.field_options,
            [(
                "value".to_string(),
                vec![("pipeline".to_string(), "true".to_string())]
            )]
        );
        // The `value` node carries an `_options` member.
        use crate::pvdata::FieldDesc;
        let FieldDesc::Structure { fields: top, .. } = expr.to_field_desc() else {
            panic!("top struct");
        };
        let (_, fd) = top.iter().find(|(n, _)| n == "field").unwrap();
        let FieldDesc::Structure { fields: f, .. } = fd else {
            panic!("field struct");
        };
        let (_, vd) = f.iter().find(|(n, _)| n == "value").unwrap();
        let FieldDesc::Structure { fields: vf, .. } = vd else {
            panic!("value struct");
        };
        assert!(vf.iter().any(|(n, _)| n == "_options"));
        assert!(vf.iter().any(|(n, _)| n == "a"));
        assert!(vf.iter().any(|(n, _)| n == "b"));
    }

    #[test]
    fn per_field_options_coexist_with_other_fields_and_record() {
        let expr =
            PvRequestExpr::parse("field(value[deadband=abs:1.0],timeStamp)record[pipeline=true]")
                .unwrap();
        assert_eq!(expr.fields, ["value", "timeStamp"]);
        assert_eq!(
            expr.field_options,
            [(
                "value".to_string(),
                vec![("deadband".to_string(), "abs:1.0".to_string())]
            )]
        );
        assert_eq!(expr.record_options, [rec_s("pipeline", "true")]);
    }

    #[test]
    fn record_option_value_accepts_colon() {
        // The permissive value lexer also lets `record[...]` carry
        // non-identifier values like `abs:1.0`.
        let expr = PvRequestExpr::parse("record[deadband=abs:1.0]").unwrap();
        assert_eq!(expr.record_options, [rec_s("deadband", "abs:1.0")]);
    }

    #[test]
    fn plain_dialects_still_parse_without_field_options() {
        // No brackets → `field_options` stays empty; existing dotted /
        // brace dialects are untouched.
        let dotted = PvRequestExpr::parse("field(value,alarm.severity)").unwrap();
        assert!(dotted.field_options.is_empty());
        let brace = PvRequestExpr::parse("field(a{b,c})").unwrap();
        assert!(brace.field_options.is_empty());
        let rec = PvRequestExpr::parse("field(value)record[pipeline=true]").unwrap();
        assert!(rec.field_options.is_empty());
    }

    #[test]
    fn unterminated_option_bracket_is_error() {
        assert!(PvRequestExpr::parse("field(value[deadband=abs:1.0").is_err());
    }

    #[test]
    fn option_with_missing_value_is_error() {
        assert!(PvRequestExpr::parse("field(value[deadband=])").is_err());
    }

    #[test]
    fn bare_name_shorthand_accepts_option_bracket() {
        // pvDataCPP allows the option bracket without a `field(...)`
        // wrapper, just as it allows the brace group there.
        let expr = PvRequestExpr::parse("value[deadband=abs:1.0]").unwrap();
        assert_eq!(expr.fields, ["value"]);
        assert_eq!(
            expr.field_options[0].1,
            [("deadband".to_string(), "abs:1.0".to_string())]
        );
    }

    #[test]
    fn split_brackets_on_same_field_merge() {
        // `value[a=1],value[b=2]` accumulates both options under one
        // `value` field-option entry.
        let expr = PvRequestExpr::parse("field(value[a=1],value[b=2])").unwrap();
        assert_eq!(expr.field_options.len(), 1);
        assert_eq!(
            expr.field_options[0].1,
            [
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "2".to_string()),
            ]
        );
    }

    #[test]
    fn field_options_round_trip_through_encode() {
        // The expression must encode to wire bytes without panicking
        // and the decode-side shape stays self-consistent.
        let expr = PvRequestExpr::parse("field(value[deadband=abs:1.0])").unwrap();
        let le = expr.encode(false);
        let be = expr.encode(true);
        assert!(!le.is_empty() && le[0] == 0x80);
        assert!(!be.is_empty() && be[0] == 0x80);
    }

    /// Regression: the pvRequest string parser must be able to
    /// express the `record._options._filter` JSON carrier the PVA
    /// server expects. The bare value lexer accepts only
    /// alphanumerics plus `_ . : - +`, so it rejects `{`, `}`, `,`,
    /// and `"` before a JSON value can be read. A quoted option value
    /// closes that gap.
    #[test]
    fn ex_r11_record_filter_json_via_quoted_value() {
        let json = r#"{"dbnd":{"d":0.5},"dec":{"n":3}}"#;
        // The JSON's inner double quotes are escaped at the
        // pvRequest-string layer with `\"`.
        let escaped = json.replace('"', "\\\"");
        let req = format!("record[_filter=\"{escaped}\"]");
        let expr = PvRequestExpr::parse(&req).expect("quoted _filter must parse");
        assert_eq!(
            expr.record_options,
            [rec_s("_filter", json)],
            "the unescaped JSON must reach record_options verbatim"
        );

        // Round-trip through encode: the JSON bytes must appear in the
        // wire-format value half (encode emits the option string value
        // via to_pv_field).
        let wire = expr.encode(false);
        assert!(
            wire.windows(json.len()).any(|w| w == json.as_bytes()),
            "the _filter JSON must survive into the encoded wire body"
        );
    }

    /// A `_filter` quoted value also works in the per-field option
    /// bracket, since both option surfaces lex through `lex_value`.
    #[test]
    fn ex_r11_quoted_value_in_field_option_bracket() {
        let expr = PvRequestExpr::parse(r#"field(value[note="a,b}c"])"#)
            .expect("quoted per-field option must parse");
        assert_eq!(
            expr.field_options[0].1,
            [("note".to_string(), "a,b}c".to_string())],
            "delimiters inside quotes must be carried literally"
        );
    }

    /// Backslash escapes inside a quoted option value.
    #[test]
    fn ex_r11_quoted_value_escapes() {
        let expr = PvRequestExpr::parse(r#"record[k="x\\y\"z"]"#).unwrap();
        assert_eq!(expr.record_options, [rec_s("k", "x\\y\"z")]);
        // Empty quoted value is valid.
        let empty = PvRequestExpr::parse(r#"record[k=""]"#).unwrap();
        assert_eq!(empty.record_options, [rec_s("k", "")]);
    }

    /// An unterminated quote and an invalid escape are parse errors.
    #[test]
    fn ex_r11_malformed_quoted_value_is_error() {
        // No closing quote before end of input.
        assert!(PvRequestExpr::parse(r#"record[_filter="{abc]"#).is_err());
        // Unknown escape sequence `\q`.
        assert!(PvRequestExpr::parse(r#"record[k="bad\q"]"#).is_err());
    }

    /// The bare (unquoted) value form is untouched — existing
    /// `deadband=abs:1.0` style options still parse exactly as before.
    #[test]
    fn ex_r11_bare_value_form_unchanged() {
        let expr = PvRequestExpr::parse("record[deadband=abs:1.0,pipeline=true]").unwrap();
        assert_eq!(
            expr.record_options,
            [rec_s("deadband", "abs:1.0"), rec_s("pipeline", "true")]
        );
    }

    // ── strict pvxs `PVRParser` grammar (parse_pvxs_compat) ───────────
    //
    // Parity with pvxs `clientreq.cpp:137-283`: the strict mode must
    // reject every lenient pvDataCPP extension while still accepting the
    // `field(name,...)` + `record[name=name,...]` core grammar.

    /// The pvxs core forms still parse in strict mode.
    #[test]
    fn pvxs_compat_accepts_core_field_and_record_forms() {
        let f = PvRequestExpr::parse_pvxs_compat("field(value,alarm.severity)")
            .expect("field(name,name) is pvxs grammar");
        assert_eq!(f.fields, ["value", "alarm.severity"]);

        let bare = PvRequestExpr::parse_pvxs_compat("value")
            .expect("bare-name short-hand is pvxs grammar");
        assert_eq!(bare.fields, ["value"]);

        let r = PvRequestExpr::parse_pvxs_compat("record[k=v,pipeline=true]")
            .expect("record[name=name] is pvxs grammar");
        assert_eq!(
            r.record_options,
            [rec_s("k", "v"), rec_s("pipeline", "true")]
        );

        // Combined field + record, like a real pvxs request string.
        let both = PvRequestExpr::parse_pvxs_compat("field(value)record[block=true]")
            .expect("field(...)record[...] is pvxs grammar");
        assert_eq!(both.fields, ["value"]);
        assert_eq!(both.record_options, [rec_s("block", "true")]);
    }

    /// Brace member groups are a pvDataCPP extension — strict mode rejects
    /// them; lenient mode still accepts them.
    #[test]
    fn pvxs_compat_rejects_brace_member_group() {
        assert!(
            PvRequestExpr::parse_pvxs_compat("field(value{a,b})").is_err(),
            "pvxs parse_fields accepts only name/comma until ')'"
        );
        // Lenient mode unchanged.
        assert!(PvRequestExpr::parse("field(value{a,b})").is_ok());
    }

    /// Per-field option brackets are a pvDataCPP extension — strict mode
    /// rejects them; lenient mode still accepts them.
    #[test]
    fn pvxs_compat_rejects_per_field_option_bracket() {
        assert!(
            PvRequestExpr::parse_pvxs_compat("field(value[deadband=abs:1.0])").is_err(),
            "pvxs allows options only on record[...], not per-field"
        );
        assert!(PvRequestExpr::parse("field(value[deadband=abs:1.0])").is_ok());
    }

    /// Quoted option values are a Rust/pvDataCPP extension — strict mode
    /// rejects them; lenient mode still accepts them.
    #[test]
    fn pvxs_compat_rejects_quoted_option_value() {
        assert!(
            PvRequestExpr::parse_pvxs_compat(r#"record[_filter="{}"]"#).is_err(),
            "pvxs parse_options requires the value to be a bare name token"
        );
        assert!(PvRequestExpr::parse(r#"record[_filter="{}"]"#).is_ok());
    }

    /// A `:`-bearing bare option value (`abs:1.0`) is outside the pvxs
    /// `name` grammar — strict mode rejects it; lenient mode accepts it.
    #[test]
    fn pvxs_compat_rejects_colon_bearing_option_value() {
        assert!(
            PvRequestExpr::parse_pvxs_compat("record[deadband=abs:1.0]").is_err(),
            "pvxs value is a name token; ':' ends it and the trailing run is a parse error"
        );
        assert!(PvRequestExpr::parse("record[deadband=abs:1.0]").is_ok());
    }
}
