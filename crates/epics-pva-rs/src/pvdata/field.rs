//! Field type description (introspection, no values).
//!
//! Includes a depth-first field-numbering walk used for monitor `BitSet`
//! deltas and `pvRequest` selectors.

use std::fmt;

use super::scalar::ScalarType;

/// Description of a field's type (introspection only — no values).
///
/// This mirrors the full pvData type space (matching pvxs `data.h::TypeDef`):
///
/// - `Scalar` / `ScalarArray` cover the 12 scalar types and their arrays
///   (`String` and `String[]` flow through the `ScalarType::String` variant).
/// - `Structure` / `StructureArray` are named records.
/// - `Union` / `UnionArray` are tagged unions over a fixed list of variants.
/// - `Variant` / `VariantArray` are "any" — the value carries its own
///   descriptor on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FieldDesc {
    Scalar(ScalarType),
    ScalarArray(ScalarType),
    Structure {
        struct_id: String,
        fields: Vec<(String, FieldDesc)>,
    },
    StructureArray {
        struct_id: String,
        fields: Vec<(String, FieldDesc)>,
    },
    Union {
        struct_id: String,
        variants: Vec<(String, FieldDesc)>,
    },
    UnionArray {
        struct_id: String,
        variants: Vec<(String, FieldDesc)>,
    },
    Variant,
    VariantArray,
}

impl FieldDesc {
    /// Get the scalar type of a `value` field in a structure.
    pub fn value_scalar_type(&self) -> Option<ScalarType> {
        if let FieldDesc::Structure { fields, .. } = self {
            for (name, desc) in fields {
                if name == "value" {
                    if let FieldDesc::Scalar(st) = desc {
                        return Some(*st);
                    }
                }
            }
        }
        None
    }

    /// Number of immediate fields (for structures and unions).
    pub fn field_count(&self) -> usize {
        match self {
            FieldDesc::Structure { fields, .. } | FieldDesc::StructureArray { fields, .. } => {
                fields.len()
            }
            FieldDesc::Union { variants, .. } | FieldDesc::UnionArray { variants, .. } => {
                variants.len()
            }
            _ => 0,
        }
    }

    /// Total number of bit positions this descriptor occupies in a monitor
    /// `BitSet`. The root structure occupies bit 0; each nested field adds 1
    /// (and recursively for nested structures). pvData spec §5.4.
    ///
    /// Unions, scalars, and arrays count as a single bit.
    pub fn total_bits(&self) -> usize {
        1 + match self {
            FieldDesc::Structure { fields, .. } => fields
                .iter()
                .map(|(_, child)| child.total_bits())
                .sum::<usize>(),
            _ => 0,
        }
    }

    /// Resolve a dotted field path (e.g. `"alarm.severity"`) to the bit index
    /// it occupies in a monitor `BitSet`. Returns `None` if any path segment
    /// is missing or the root descriptor is not a structure.
    pub fn bit_for_path(&self, path: &str) -> Option<usize> {
        if path.is_empty() {
            return Some(0); // root
        }
        let parts: Vec<&str> = path.split('.').collect();
        find_bit_for_path(self, 0, &parts).map(|(idx, _)| idx)
    }

    /// Resolve a dotted field path to its `(start, end)` bit span —
    /// `start` is the field's own bit, `end` is one past its last
    /// descendant (`start + total_bits`). Non-structure fields span one
    /// bit. Returns `None` for a missing segment; the empty path is the
    /// root's full span.
    pub(crate) fn bit_span_for_path(&self, path: &str) -> Option<(usize, usize)> {
        if path.is_empty() {
            return Some((0, self.total_bits()));
        }
        let parts: Vec<&str> = path.split('.').collect();
        find_bit_for_path(self, 0, &parts)
    }

    /// True iff this descriptor names a string type (Scalar/String).
    pub fn is_string_like(&self) -> bool {
        matches!(self, FieldDesc::Scalar(ScalarType::String))
    }
}

fn find_bit_for_path(desc: &FieldDesc, base: usize, path: &[&str]) -> Option<(usize, usize)> {
    if path.is_empty() {
        return Some((base, base + desc.total_bits()));
    }
    let head = path[0];
    let tail = &path[1..];
    if let FieldDesc::Structure { fields, .. } = desc {
        let mut offset = base + 1;
        for (name, child) in fields {
            if name == head {
                return find_bit_for_path(child, offset, tail);
            }
            offset += child.total_bits();
        }
    }
    None
}

impl fmt::Display for FieldDesc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_indent(f, 0)
    }
}

impl FieldDesc {
    fn fmt_indent(&self, f: &mut fmt::Formatter<'_>, indent: usize) -> fmt::Result {
        let pad = "    ".repeat(indent);
        match self {
            FieldDesc::Scalar(st) => write!(f, "{st}"),
            FieldDesc::ScalarArray(st) => write!(f, "{st}[]"),
            FieldDesc::Variant => write!(f, "any"),
            FieldDesc::VariantArray => write!(f, "any[]"),
            FieldDesc::Structure { struct_id, fields }
            | FieldDesc::StructureArray { struct_id, fields } => {
                let suffix = if matches!(self, FieldDesc::StructureArray { .. }) {
                    "[]"
                } else {
                    ""
                };
                if struct_id.is_empty() {
                    writeln!(f, "structure{suffix}")?;
                } else {
                    writeln!(f, "structure{suffix} {struct_id}")?;
                }
                for (name, desc) in fields {
                    write!(f, "{pad}    {name}: ")?;
                    desc.fmt_indent(f, indent + 1)?;
                    writeln!(f)?;
                }
                Ok(())
            }
            FieldDesc::Union { variants, .. } | FieldDesc::UnionArray { variants, .. } => {
                let suffix = if matches!(self, FieldDesc::UnionArray { .. }) {
                    "[]"
                } else {
                    ""
                };
                writeln!(f, "union{suffix}")?;
                for (name, desc) in variants {
                    write!(f, "{pad}    {name}: ")?;
                    desc.fmt_indent(f, indent + 1)?;
                    writeln!(f)?;
                }
                Ok(())
            }
        }
    }
}

/// One member entry inside a [`TypeDef`] structure / union body.
/// Mirrors pvxs `Member` (data.h) — pair of (field name, descriptor).
#[derive(Debug, Clone)]
pub struct Member {
    pub name: String,
    pub desc: FieldDesc,
}

/// Validate a local member name against pvxs's field-name grammar
/// `[a-zA-Z_][a-zA-Z0-9_]*`.
///
/// An empty name is permitted: pvxs `Member::_validate()` skips
/// `name_validate()` for empty names (the unnamed root / anonymous node
/// case). Any non-empty name that violates the grammar is rejected.
///
/// Mirrors pvxs `src/type.cpp:193-224` (`name_validate()` /
/// `Member::_validate()`), which throws `std::runtime_error` at `Member`
/// construction — before a local `TypeDef` can be built. We panic for the
/// same contract so an invalid schema is caught at type-definition time
/// rather than emitted onto the wire, where a pvxs peer decoding the
/// descriptor would fault. ASCII-only by design: pvxs's check uses the
/// `'a'..='z'` / `'A'..='Z'` / `'0'..='9'` ranges, so non-ASCII letters
/// must be rejected too.
fn validate_field_name(name: &str) {
    if name.is_empty() {
        return;
    }
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    assert!(
        first_ok && rest_ok,
        "invalid pvData field name {name:?}: must match [a-zA-Z_][a-zA-Z0-9_]*"
    );
}

impl Member {
    pub fn new(name: impl Into<String>, desc: FieldDesc) -> Self {
        let name = name.into();
        validate_field_name(&name);
        Self { name, desc }
    }

    /// Convenience: scalar member `name: T`. Mirrors pvxs
    /// `Member(TypeCode::Int32, "value")` shorthand.
    pub fn scalar(name: impl Into<String>, st: ScalarType) -> Self {
        Self::new(name, FieldDesc::Scalar(st))
    }

    /// Convenience: scalar-array member `name: T[]`.
    pub fn scalar_array(name: impl Into<String>, st: ScalarType) -> Self {
        Self::new(name, FieldDesc::ScalarArray(st))
    }
}

/// Fluent builder for [`FieldDesc`] structure trees. Mirrors pvxs's
/// `TypeDef(TypeCode::Struct, "epics:nt/NTScalar:1.0", { Member(...), ... })`
/// construct, the canonical way pvxs code declares record types.
///
/// # Examples
///
/// ```ignore
/// use epics_pva_rs::pvdata::{TypeDef, Member, ScalarType};
///
/// let nt_scalar = TypeDef::structure("epics:nt/NTScalar:1.0")
///     .member(Member::scalar("value", ScalarType::Double))
///     .member(Member::new("alarm", TypeDef::structure("alarm_t")
///         .member(Member::scalar("severity", ScalarType::Int))
///         .member(Member::scalar("status", ScalarType::Int))
///         .member(Member::scalar("message", ScalarType::String))
///         .build()))
///     .build();
/// ```
#[derive(Debug, Clone)]
pub struct TypeDef {
    /// Underlying type discriminator. Wrapped so we can switch
    /// between Structure / StructureArray / Union / UnionArray at
    /// `build()` time.
    kind: TypeDefKind,
    struct_id: String,
    members: Vec<Member>,
}

#[derive(Debug, Clone, Copy)]
enum TypeDefKind {
    Structure,
    StructureArray,
    Union,
    UnionArray,
}

impl TypeDef {
    /// Begin a structure type. `struct_id` is the optional NT id
    /// (e.g., `"epics:nt/NTScalar:1.0"`); pass `""` for unnamed.
    pub fn structure(struct_id: impl Into<String>) -> Self {
        Self {
            kind: TypeDefKind::Structure,
            struct_id: struct_id.into(),
            members: Vec::new(),
        }
    }

    /// Begin a structure-array type.
    pub fn structure_array(struct_id: impl Into<String>) -> Self {
        Self {
            kind: TypeDefKind::StructureArray,
            struct_id: struct_id.into(),
            members: Vec::new(),
        }
    }

    /// Begin a union type. `struct_id` may be empty for an unnamed
    /// union. Members are tagged variants — exactly one is populated
    /// in any concrete value.
    pub fn union(struct_id: impl Into<String>) -> Self {
        Self {
            kind: TypeDefKind::Union,
            struct_id: struct_id.into(),
            members: Vec::new(),
        }
    }

    /// Begin a union-array type.
    pub fn union_array(struct_id: impl Into<String>) -> Self {
        Self {
            kind: TypeDefKind::UnionArray,
            struct_id: struct_id.into(),
            members: Vec::new(),
        }
    }

    /// Append a member. Returns self by value for fluent chaining.
    pub fn member(mut self, m: Member) -> Self {
        self.members.push(m);
        self
    }

    /// Append a scalar member by shorthand. Equivalent to
    /// `.member(Member::scalar(name, st))`.
    pub fn scalar(self, name: impl Into<String>, st: ScalarType) -> Self {
        self.member(Member::scalar(name, st))
    }

    /// Append a scalar-array member.
    pub fn scalar_array(self, name: impl Into<String>, st: ScalarType) -> Self {
        self.member(Member::scalar_array(name, st))
    }

    /// Materialize the [`FieldDesc`] tree.
    pub fn build(self) -> FieldDesc {
        let fields: Vec<(String, FieldDesc)> =
            self.members.into_iter().map(|m| (m.name, m.desc)).collect();
        match self.kind {
            TypeDefKind::Structure => FieldDesc::Structure {
                struct_id: self.struct_id,
                fields,
            },
            TypeDefKind::StructureArray => FieldDesc::StructureArray {
                struct_id: self.struct_id,
                fields,
            },
            TypeDefKind::Union => FieldDesc::Union {
                struct_id: self.struct_id,
                variants: fields,
            },
            TypeDefKind::UnionArray => FieldDesc::UnionArray {
                struct_id: self.struct_id,
                variants: fields,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nt_scalar_double() -> FieldDesc {
        FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![
                ("value".into(), FieldDesc::Scalar(ScalarType::Double)),
                (
                    "alarm".into(),
                    FieldDesc::Structure {
                        struct_id: "alarm_t".into(),
                        fields: vec![
                            ("severity".into(), FieldDesc::Scalar(ScalarType::Int)),
                            ("status".into(), FieldDesc::Scalar(ScalarType::Int)),
                            ("message".into(), FieldDesc::Scalar(ScalarType::String)),
                        ],
                    },
                ),
                (
                    "timeStamp".into(),
                    FieldDesc::Structure {
                        struct_id: "time_t".into(),
                        fields: vec![
                            (
                                "secondsPastEpoch".into(),
                                FieldDesc::Scalar(ScalarType::Long),
                            ),
                            ("nanoseconds".into(), FieldDesc::Scalar(ScalarType::Int)),
                            ("userTag".into(), FieldDesc::Scalar(ScalarType::Int)),
                        ],
                    },
                ),
            ],
        }
    }

    #[test]
    fn total_bits_counts_root_plus_children() {
        let nt = nt_scalar_double();
        assert_eq!(nt.total_bits(), 10);
    }

    #[test]
    fn bit_for_path_walks_depth_first() {
        let nt = nt_scalar_double();
        assert_eq!(nt.bit_for_path(""), Some(0));
        assert_eq!(nt.bit_for_path("value"), Some(1));
        assert_eq!(nt.bit_for_path("alarm"), Some(2));
        assert_eq!(nt.bit_for_path("alarm.severity"), Some(3));
        assert_eq!(nt.bit_for_path("alarm.status"), Some(4));
        assert_eq!(nt.bit_for_path("alarm.message"), Some(5));
        assert_eq!(nt.bit_for_path("timeStamp"), Some(6));
        assert_eq!(nt.bit_for_path("timeStamp.secondsPastEpoch"), Some(7));
        assert_eq!(nt.bit_for_path("timeStamp.nanoseconds"), Some(8));
        assert_eq!(nt.bit_for_path("timeStamp.userTag"), Some(9));
    }

    #[test]
    fn bit_for_path_unknown_returns_none() {
        let nt = nt_scalar_double();
        assert_eq!(nt.bit_for_path("doesNotExist"), None);
        assert_eq!(nt.bit_for_path("alarm.bogus"), None);
    }

    #[test]
    fn value_scalar_type_extraction() {
        let nt = nt_scalar_double();
        assert_eq!(nt.value_scalar_type(), Some(ScalarType::Double));
    }

    #[test]
    fn union_field_count() {
        let u = FieldDesc::Union {
            struct_id: String::new(),
            variants: vec![
                (
                    "doubleValue".into(),
                    FieldDesc::ScalarArray(ScalarType::Double),
                ),
                ("intValue".into(), FieldDesc::ScalarArray(ScalarType::Int)),
            ],
        };
        assert_eq!(u.field_count(), 2);
    }

    // ── TypeDef builder — pvxs test/testtype.cpp::testTypeDef parity ──

    #[test]
    fn typedef_structure_with_scalar_member() {
        let td = TypeDef::structure("epics:nt/NTScalar:1.0")
            .scalar("value", ScalarType::Double)
            .build();
        match td {
            FieldDesc::Structure { struct_id, fields } => {
                assert_eq!(struct_id, "epics:nt/NTScalar:1.0");
                assert_eq!(fields.len(), 1);
                assert_eq!(fields[0].0, "value");
                assert!(matches!(fields[0].1, FieldDesc::Scalar(ScalarType::Double)));
            }
            other => panic!("expected Structure, got {other:?}"),
        }
    }

    #[test]
    fn typedef_structure_with_scalar_array_member() {
        let td = TypeDef::structure("test:array")
            .scalar_array("vals", ScalarType::Int)
            .build();
        match td {
            FieldDesc::Structure { fields, .. } => {
                assert!(matches!(
                    fields[0].1,
                    FieldDesc::ScalarArray(ScalarType::Int)
                ));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn typedef_nested_structure_via_member() {
        let alarm = TypeDef::structure("alarm_t")
            .scalar("severity", ScalarType::Int)
            .scalar("status", ScalarType::Int)
            .scalar("message", ScalarType::String)
            .build();
        let nt = TypeDef::structure("epics:nt/NTScalar:1.0")
            .scalar("value", ScalarType::Double)
            .member(Member::new("alarm", alarm))
            .build();
        match nt {
            FieldDesc::Structure { fields, .. } => {
                assert_eq!(fields.len(), 2);
                assert_eq!(fields[1].0, "alarm");
                match &fields[1].1 {
                    FieldDesc::Structure { struct_id, fields } => {
                        assert_eq!(struct_id, "alarm_t");
                        assert_eq!(fields.len(), 3);
                        assert_eq!(fields[2].0, "message");
                    }
                    _ => panic!("inner must be Structure"),
                }
            }
            _ => panic!(),
        }
    }

    #[test]
    fn typedef_structure_array() {
        let td = TypeDef::structure_array("rows")
            .scalar("a", ScalarType::Int)
            .build();
        assert!(matches!(td, FieldDesc::StructureArray { .. }));
    }

    #[test]
    fn typedef_union_holds_variants_under_variants_field() {
        let td = TypeDef::union("any")
            .scalar("d", ScalarType::Double)
            .scalar("i", ScalarType::Int)
            .build();
        match td {
            FieldDesc::Union {
                struct_id,
                variants,
            } => {
                assert_eq!(struct_id, "any");
                assert_eq!(variants.len(), 2);
            }
            _ => panic!("expected Union"),
        }
    }

    #[test]
    fn typedef_union_array() {
        let td = TypeDef::union_array("cells")
            .scalar("v", ScalarType::Float)
            .build();
        assert!(matches!(td, FieldDesc::UnionArray { .. }));
    }

    #[test]
    fn typedef_empty_struct_id_allowed() {
        let td = TypeDef::structure("").scalar("x", ScalarType::Int).build();
        match td {
            FieldDesc::Structure { struct_id, .. } => assert!(struct_id.is_empty()),
            _ => panic!(),
        }
    }

    #[test]
    fn typedef_total_bits_round_trips_through_field_desc() {
        // TypeDef-built tree should produce the same total_bits as the
        // hand-rolled equivalent — guards against silent drift between
        // the builder and FieldDesc::total_bits.
        let built = TypeDef::structure("epics:nt/NTScalar:1.0")
            .scalar("value", ScalarType::Double)
            .member(Member::new(
                "alarm",
                TypeDef::structure("alarm_t")
                    .scalar("severity", ScalarType::Int)
                    .scalar("status", ScalarType::Int)
                    .scalar("message", ScalarType::String)
                    .build(),
            ))
            .build();
        let hand = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![
                ("value".into(), FieldDesc::Scalar(ScalarType::Double)),
                (
                    "alarm".into(),
                    FieldDesc::Structure {
                        struct_id: "alarm_t".into(),
                        fields: vec![
                            ("severity".into(), FieldDesc::Scalar(ScalarType::Int)),
                            ("status".into(), FieldDesc::Scalar(ScalarType::Int)),
                            ("message".into(), FieldDesc::Scalar(ScalarType::String)),
                        ],
                    },
                ),
            ],
        };
        assert_eq!(built.total_bits(), hand.total_bits());
    }

    #[test]
    fn typedef_member_scalar_helper() {
        let m = Member::scalar("x", ScalarType::Boolean);
        assert_eq!(m.name, "x");
        assert!(matches!(m.desc, FieldDesc::Scalar(ScalarType::Boolean)));
    }

    #[test]
    fn typedef_member_scalar_array_helper() {
        let m = Member::scalar_array("xs", ScalarType::String);
        assert!(matches!(m.desc, FieldDesc::ScalarArray(ScalarType::String)));
    }

    // ── Member name validation — pvxs type.cpp::name_validate parity ──

    #[test]
    fn member_name_valid_names_accepted() {
        // [a-zA-Z_][a-zA-Z0-9_]* — leading underscore, digits after first.
        for name in ["value", "_private", "field1", "a", "A_1b2"] {
            let m = Member::scalar(name, ScalarType::Int);
            assert_eq!(m.name, name);
        }
    }

    #[test]
    fn member_name_empty_allowed_for_unnamed_node() {
        // pvxs Member::_validate() skips name_validate() for empty names.
        let m = Member::new("", FieldDesc::Scalar(ScalarType::Int));
        assert!(m.name.is_empty());
    }

    #[test]
    #[should_panic(expected = "invalid pvData field name")]
    fn member_name_leading_digit_rejected() {
        let _ = Member::scalar("1value", ScalarType::Int);
    }

    #[test]
    #[should_panic(expected = "invalid pvData field name")]
    fn member_name_dotted_rejected() {
        // Dotted names collide with pvRequest / BitSet path conventions.
        let _ = Member::scalar("alarm.severity", ScalarType::Int);
    }

    #[test]
    #[should_panic(expected = "invalid pvData field name")]
    fn member_name_hyphen_rejected() {
        let _ = Member::scalar("value-status", ScalarType::Int);
    }

    #[test]
    #[should_panic(expected = "invalid pvData field name")]
    fn member_name_space_rejected() {
        let _ = Member::new("has space", FieldDesc::Scalar(ScalarType::Int));
    }

    #[test]
    #[should_panic(expected = "invalid pvData field name")]
    fn member_name_non_ascii_rejected() {
        // pvxs's ranges are ASCII-only; a unicode letter must be rejected.
        let _ = Member::scalar("café", ScalarType::Int);
    }

    #[test]
    #[should_panic(expected = "invalid pvData field name")]
    fn typedef_builder_rejects_invalid_member_name() {
        // The fluent builder funnels through Member::new, so an invalid
        // name is rejected before build().
        let _ = TypeDef::structure("epics:nt/NTScalar:1.0")
            .scalar("bad name", ScalarType::Double)
            .build();
    }
}
