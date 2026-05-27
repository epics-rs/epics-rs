//! pvxs-compatible output formatting for PVA values and type descriptors.
//!
//! Operates on native [`crate::pvdata`] types (`FieldDesc` / `PvField` /
//! `PvStructure`) — no `spvirit_codec` dependency. Mirrors the layout pvxs
//! `pvget` / `pvinfo` produce.

use std::fmt::Write as _;

use crate::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};

// ─── pvinfo formatting (type descriptor) ────────────────────────────────────

/// Format a top-level structure descriptor in pvxs `pvinfo` style.
///
/// ```text
/// epics:nt/NTNDArray:1.0
///     union value
///         boolean[] booleanValue
///     codec_t codec
///         string name
/// ```
pub fn format_info(desc: &FieldDesc) -> String {
    format_info_indented(desc, 0)
}

/// Format with a base indentation level (used by pvinfo-rs to nest under
/// the `Type:` header).
pub fn format_info_indented(desc: &FieldDesc, base_depth: usize) -> String {
    let mut out = String::new();
    let indent = "    ".repeat(base_depth);
    let id = struct_id_or_default(desc, "structure");
    let _ = writeln!(out, "{indent}{id}");
    write_info_children(&mut out, desc, base_depth + 1);
    out
}

fn struct_id_or_default<'a>(desc: &'a FieldDesc, fallback: &'a str) -> &'a str {
    match desc {
        FieldDesc::Structure { struct_id, .. } | FieldDesc::StructureArray { struct_id, .. }
            if !struct_id.is_empty() =>
        {
            struct_id.as_str()
        }
        FieldDesc::Union { struct_id, .. } | FieldDesc::UnionArray { struct_id, .. }
            if !struct_id.is_empty() =>
        {
            struct_id.as_str()
        }
        _ => fallback,
    }
}

fn write_info_children(out: &mut String, desc: &FieldDesc, depth: usize) {
    match desc {
        FieldDesc::Structure { fields, .. } | FieldDesc::StructureArray { fields, .. } => {
            for (name, child) in fields {
                write_info_field(out, name, child, depth);
            }
        }
        FieldDesc::Union { variants, .. } | FieldDesc::UnionArray { variants, .. } => {
            for (name, child) in variants {
                write_info_field(out, name, child, depth);
            }
        }
        _ => {}
    }
}

fn write_info_field(out: &mut String, name: &str, desc: &FieldDesc, depth: usize) {
    let indent = "    ".repeat(depth);
    match desc {
        FieldDesc::Structure { struct_id, fields } => {
            let id = if struct_id.is_empty() {
                "structure"
            } else {
                struct_id
            };
            let _ = writeln!(out, "{indent}{id} {name}");
            for (n, c) in fields {
                write_info_field(out, n, c, depth + 1);
            }
        }
        FieldDesc::StructureArray { struct_id, fields } => {
            let id = if struct_id.is_empty() {
                "structure"
            } else {
                struct_id
            };
            let _ = writeln!(out, "{indent}{id}[] {name}");
            let inner_indent = "    ".repeat(depth + 1);
            let _ = writeln!(out, "{inner_indent}{id}");
            for (n, c) in fields {
                write_info_field(out, n, c, depth + 2);
            }
        }
        FieldDesc::Union { variants, .. } => {
            let _ = writeln!(out, "{indent}union {name}");
            for (n, c) in variants {
                write_info_field(out, n, c, depth + 1);
            }
        }
        FieldDesc::UnionArray { variants, .. } => {
            let _ = writeln!(out, "{indent}union[] {name}");
            for (n, c) in variants {
                write_info_field(out, n, c, depth + 1);
            }
        }
        _ => {
            let _ = writeln!(out, "{indent}{} {name}", type_name(desc));
        }
    }
}

fn type_name(desc: &FieldDesc) -> &'static str {
    match desc {
        FieldDesc::Scalar(st) => scalar_type_name(*st),
        FieldDesc::ScalarArray(st) => scalar_array_type_name(*st),
        FieldDesc::Variant => "any",
        FieldDesc::VariantArray => "any[]",
        FieldDesc::BoundedString(_) => "string",
        FieldDesc::Structure { .. } => "structure",
        FieldDesc::StructureArray { .. } => "structure[]",
        FieldDesc::Union { .. } => "union",
        FieldDesc::UnionArray { .. } => "union[]",
    }
}

fn scalar_type_name(st: ScalarType) -> &'static str {
    match st {
        ScalarType::Boolean => "boolean",
        ScalarType::Byte => "byte",
        ScalarType::Short => "short",
        ScalarType::Int => "int",
        ScalarType::Long => "long",
        ScalarType::UByte => "ubyte",
        ScalarType::UShort => "ushort",
        ScalarType::UInt => "uint",
        ScalarType::ULong => "ulong",
        ScalarType::Float => "float",
        ScalarType::Double => "double",
        ScalarType::String => "string",
    }
}

fn scalar_array_type_name(st: ScalarType) -> &'static str {
    match st {
        ScalarType::Boolean => "boolean[]",
        ScalarType::Byte => "byte[]",
        ScalarType::Short => "short[]",
        ScalarType::Int => "int[]",
        ScalarType::Long => "long[]",
        ScalarType::UByte => "ubyte[]",
        ScalarType::UShort => "ushort[]",
        ScalarType::UInt => "uint[]",
        ScalarType::ULong => "ulong[]",
        ScalarType::Float => "float[]",
        ScalarType::Double => "double[]",
        ScalarType::String => "string[]",
    }
}

// ─── pvget raw / verbose formatting (type + value) ──────────────────────────

/// Format value with type descriptors in pvxs raw/verbose style.
pub fn format_raw(pv_name: &str, desc: &FieldDesc, value: &PvField) -> String {
    let mut out = String::new();
    let id = struct_id_or_default(desc, "structure");
    let _ = writeln!(out, "{pv_name} {id} ");
    if let (FieldDesc::Structure { fields, .. }, PvField::Structure(s)) = (desc, value) {
        for (name, child_desc) in fields {
            if let Some(child_val) = s.get_field(name) {
                write_raw_field(&mut out, name, child_desc, child_val, 1);
            }
        }
    }
    out
}

fn write_raw_field(out: &mut String, name: &str, desc: &FieldDesc, value: &PvField, depth: usize) {
    let indent = "    ".repeat(depth);
    match (desc, value) {
        (FieldDesc::Structure { struct_id, fields }, PvField::Structure(s)) => {
            let id = if struct_id.is_empty() {
                "structure"
            } else {
                struct_id
            };
            if struct_id == "time_t" {
                let ts_str = format_timestamp(s);
                let _ = writeln!(out, "{indent}{id} {name} {ts_str}");
            } else if struct_id == "enum_t" {
                let summary = format_enum_summary(s);
                let _ = writeln!(out, "{indent}{id} {name} {summary}");
            } else {
                let _ = writeln!(out, "{indent}{id} {name}");
            }
            for (n, child_desc) in fields {
                if let Some(child_val) = s.get_field(n) {
                    write_raw_field(out, n, child_desc, child_val, depth + 1);
                }
            }
        }
        (FieldDesc::StructureArray { struct_id, fields }, PvField::StructureArray(items)) => {
            let id = if struct_id.is_empty() {
                "structure"
            } else {
                struct_id
            };
            let _ = writeln!(out, "{indent}{id}[] {name}");
            for s in items {
                // a `None` element is a null (absent) element.
                let Some(s) = s else {
                    let _ = writeln!(out, "{indent}    (null)");
                    continue;
                };
                let _ = writeln!(out, "{indent}    {id} ");
                for (n, child_desc) in fields {
                    if let Some(child_val) = s.get_field(n) {
                        write_raw_field(out, n, child_desc, child_val, depth + 2);
                    }
                }
            }
        }
        (
            FieldDesc::Union { .. },
            PvField::Union {
                variant_name,
                value,
                ..
            },
        ) => {
            // Show selected variant on the same line as `union`.
            let _ = writeln!(
                out,
                "{indent}union {name}\n{indent}    {} {variant_name} {}",
                value_type_name(value),
                format_value_inline(value),
            );
        }
        _ => {
            let tn = type_name(desc);
            let _ = writeln!(out, "{indent}{tn} {name} {}", format_value_inline(value));
        }
    }
}

// ─── pvget NT mode formatting ───────────────────────────────────────────────

/// Format value in NT mode (default pvget output).
pub fn format_nt(pv_name: &str, desc: &FieldDesc, value: &PvField) -> String {
    let id = struct_id_or_default(desc, "");
    let s = match value {
        PvField::Structure(s) => s,
        _ => return format!("{pv_name} {value}\n"),
    };
    if id.starts_with("epics:nt/NTScalar:") {
        format_nt_scalar(pv_name, s)
    } else if id.starts_with("epics:nt/NTEnum:") {
        format_nt_enum(pv_name, s)
    } else {
        format_raw(pv_name, desc, value)
    }
}

fn format_nt_scalar(pv_name: &str, s: &PvStructure) -> String {
    // Legacy `pvget -M nt` shape (pvAccessCPP `pvget.cpp::printValue`):
    //   "<name> <timestamp>  <value> \n"
    // — double space between ts and val, trailing space before \n.
    format!("{pv_name} {}", nt_payload(s))
}

/// `<timestamp>  <value> \n` payload of legacy NT-scalar output —
/// reused by `pvput`'s `Old :` / `New :` echo lines.
pub fn format_nt_old_new_payload(s: &PvStructure) -> String {
    nt_payload(s)
}

fn nt_payload(s: &PvStructure) -> String {
    let val = s
        .get_field("value")
        .map(nt_scalar_value_str)
        .unwrap_or_default();
    let ts = s
        .get_field("timeStamp")
        .and_then(|f| match f {
            PvField::Structure(ts) => Some(format_timestamp(ts)),
            _ => None,
        })
        .unwrap_or_else(|| "<undefined>".to_string());
    format!("{ts}  {val} \n")
}

/// Render an NTScalar `value` field for `-M nt` output.
///
/// Legacy `pvget` calls `pvDataToString(value)` on the value field,
/// which routes to `pvDouble::toString`/`pvFloat::toString` →
/// `tr1::lexical_cast<string>(value)`. lexical_cast for floating point
/// uses `%.<digits>g` where `<digits>` is the shortest round-trip
/// precision, NOT the C `printf("%g")` 6-digit default. To stay
/// readable for typical NTScalar payloads (mini-beamline emits 6-7
/// significant digits) we mirror the shorter-of-shortest-vs-6 the
/// pvAccessCPP toString helpers actually produce on macOS today —
/// `%.6g` matches the observed live output character-for-character on
/// every value the test IOC has produced so far.
fn nt_scalar_value_str(v: &PvField) -> String {
    match v {
        PvField::Scalar(ScalarValue::Double(x)) => format_g(*x, 6),
        PvField::Scalar(ScalarValue::Float(x)) => format_g(*x as f64, 6),
        other => format_value_inline(other),
    }
}

/// `%g`-equivalent formatter (precision = significant digits). Mirrors
/// C `printf("%.*g", precision, x)` semantics: shortest of `%f`/`%e`,
/// trailing zeros stripped, signed two-digit-min exponent.
fn format_g(x: f64, precision: usize) -> String {
    if x == 0.0 {
        return "0".to_string();
    }
    if !x.is_finite() {
        return format!("{x}");
    }
    let abs = x.abs();
    let exp = abs.log10().floor() as i32;
    if exp >= -4 && exp < precision as i32 {
        let digits = (precision as i32 - 1 - exp).max(0) as usize;
        let s = format!("{x:.digits$}");
        if !s.contains('.') {
            return s;
        }
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        let s = format!("{:.*e}", precision - 1, x);
        rewrite_e(&s, true)
    }
}

fn rewrite_e(s: &str, trim_mantissa: bool) -> String {
    let Some(e_pos) = s.find('e') else {
        return s.to_string();
    };
    let mantissa = &s[..e_pos];
    let exp_part = &s[e_pos + 1..];
    let mantissa_out = if trim_mantissa && mantissa.contains('.') {
        mantissa
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    } else {
        mantissa.to_string()
    };
    let (sign, digits) = if let Some(d) = exp_part.strip_prefix('-') {
        ('-', d)
    } else if let Some(d) = exp_part.strip_prefix('+') {
        ('+', d)
    } else {
        ('+', exp_part)
    };
    let exp_padded = if digits.len() < 2 {
        format!("{sign}0{digits}")
    } else {
        format!("{sign}{digits}")
    };
    format!("{mantissa_out}e{exp_padded}")
}

fn format_nt_enum(pv_name: &str, s: &PvStructure) -> String {
    let ts = s
        .get_field("timeStamp")
        .and_then(|f| match f {
            PvField::Structure(ts) => Some(format_timestamp(ts)),
            _ => None,
        })
        .unwrap_or_else(|| "<undefined>".to_string());
    let (idx, choice) = match s.get_field("value") {
        Some(PvField::Structure(es)) => {
            let i = es
                .get_field("index")
                .map(format_value_inline)
                .unwrap_or_else(|| "0".to_string());
            let choice = if let Some(PvField::ScalarArray(items)) = es.get_field("choices") {
                let n: usize = i.parse().unwrap_or(0);
                items
                    .get(n)
                    .map(|v| match v {
                        ScalarValue::String(s) => s.clone(),
                        other => format!("{other}"),
                    })
                    .unwrap_or_default()
            } else {
                String::new()
            };
            (i, choice)
        }
        _ => ("0".to_string(), String::new()),
    };
    format!("{pv_name} {ts} ({idx}) {choice}\n")
}

// ─── JSON formatting ────────────────────────────────────────────────────────

/// Format value as JSON (pvget -M json style).
pub fn format_json(pv_name: &str, value: &PvField) -> String {
    format!("{pv_name} {}\n", value_to_json(value))
}

fn value_to_json(value: &PvField) -> String {
    match value {
        PvField::Scalar(sv) => scalar_to_json(sv),
        PvField::ScalarArray(items) => {
            let parts: Vec<String> = items.iter().map(scalar_to_json).collect();
            format!("[{}]", parts.join(","))
        }
        PvField::Structure(s) => structure_to_json(s),
        PvField::StructureArray(items) => {
            // a `None` element renders as JSON `null`.
            let parts: Vec<String> = items
                .iter()
                .map(|s| match s {
                    Some(s) => structure_to_json(s),
                    None => "null".to_string(),
                })
                .collect();
            format!("[{}]", parts.join(","))
        }
        PvField::Union { value, .. } => value_to_json(value),
        PvField::UnionArray(items) => {
            let parts: Vec<String> = items
                .iter()
                .map(|it| match it {
                    Some(it) => value_to_json(&it.value),
                    None => "null".to_string(),
                })
                .collect();
            format!("[{}]", parts.join(","))
        }
        PvField::Variant(v) => value_to_json(&v.value),
        PvField::VariantArray(items) => {
            let parts: Vec<String> = items
                .iter()
                .map(|it| match it {
                    Some(it) => value_to_json(&it.value),
                    None => "null".to_string(),
                })
                .collect();
            format!("[{}]", parts.join(","))
        }
        PvField::Null => "null".to_string(),
        PvField::ScalarArrayTyped(arr) => {
            // Same JSON shape as the legacy ScalarArray branch; delegate
            // through the lossy round-trip helper.
            let parts: Vec<String> = arr.to_scalar_values().iter().map(scalar_to_json).collect();
            format!("[{}]", parts.join(","))
        }
    }
}

fn structure_to_json(s: &PvStructure) -> String {
    let parts: Vec<String> = s
        .fields
        .iter()
        .map(|(n, v)| format!("{n}:{}", value_to_json(v)))
        .collect();
    format!("{{{}}}", parts.join(","))
}

fn scalar_to_json(v: &ScalarValue) -> String {
    match v {
        ScalarValue::String(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
        ScalarValue::Float(f) => {
            if f.fract() == 0.0 {
                format!("{f:.1}")
            } else {
                format!("{f}")
            }
        }
        ScalarValue::Double(f) => {
            if f.fract() == 0.0 {
                format!("{f:.1}")
            } else {
                format!("{f}")
            }
        }
        other => format!("{other}"),
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn format_enum_summary(s: &PvStructure) -> String {
    let idx = s
        .get_field("index")
        .map(format_value_inline)
        .unwrap_or_else(|| "0".to_string());
    let choice = if let Some(PvField::ScalarArray(items)) = s.get_field("choices") {
        let n: usize = idx.parse().unwrap_or(0);
        items
            .get(n)
            .map(|v| match v {
                ScalarValue::String(s) => s.clone(),
                other => format!("{other}"),
            })
            .unwrap_or_default()
    } else {
        String::new()
    };
    format!("({idx}) {choice}")
}

/// Format an EPICS timestamp from a `time_t` structure. Returns
/// `YYYY-MM-DD HH:MM:SS.mmm` in local time, or `<undefined>` for epoch=0.
fn format_timestamp(s: &PvStructure) -> String {
    let sec = match s.get_field("secondsPastEpoch") {
        Some(PvField::Scalar(ScalarValue::Long(v))) => *v,
        Some(PvField::Scalar(ScalarValue::Int(v))) => *v as i64,
        _ => return "<undefined>".to_string(),
    };
    if sec == 0 {
        return "<undefined>".to_string();
    }
    let nsec = match s.get_field("nanoseconds") {
        Some(PvField::Scalar(ScalarValue::Int(v))) => *v as u32,
        Some(PvField::Scalar(ScalarValue::UInt(v))) => *v,
        _ => 0,
    };
    let dt = chrono::DateTime::from_timestamp(sec, nsec);
    match dt {
        Some(dt) => {
            let local = dt.with_timezone(&chrono::Local);
            format!("{}", local.format("%Y-%m-%d %H:%M:%S.%3f"))
        }
        None => "<undefined>".to_string(),
    }
}

fn format_value_inline(v: &PvField) -> String {
    match v {
        PvField::Scalar(sv) => scalar_to_inline(sv),
        PvField::ScalarArray(items) => {
            let parts: Vec<String> = items.iter().map(scalar_to_inline).collect();
            format!("[{}]", parts.join(", "))
        }
        PvField::Null => String::new(),
        other => format!("{other}"),
    }
}

fn scalar_to_inline(v: &ScalarValue) -> String {
    match v {
        ScalarValue::Double(f) => {
            if f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{}", *f as i64)
            } else {
                format!("{f}")
            }
        }
        ScalarValue::Float(f) => {
            if f.fract() == 0.0 && f.abs() < 1e7 {
                format!("{}", *f as i32)
            } else {
                format!("{f}")
            }
        }
        ScalarValue::String(s) => s.clone(),
        ScalarValue::Boolean(b) => (if *b { "true" } else { "false" }).to_string(),
        other => format!("{other}"),
    }
}

fn value_type_name(v: &PvField) -> &'static str {
    match v {
        PvField::Scalar(sv) => scalar_type_name(sv.scalar_type()),
        PvField::ScalarArray(_) => "array",
        PvField::ScalarArrayTyped(_) => "array",
        PvField::Structure(_) => "structure",
        PvField::StructureArray(_) => "structure[]",
        PvField::Union { .. } => "union",
        PvField::UnionArray(_) => "union[]",
        PvField::Variant(_) => "any",
        PvField::VariantArray(_) => "any[]",
        PvField::Null => "null",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nt_scalar_double(value: f64, sec: i64, nsec: i32) -> (FieldDesc, PvField) {
        let desc = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![
                ("value".into(), FieldDesc::Scalar(ScalarType::Double)),
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
        };
        let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
        s.set("value", PvField::Scalar(ScalarValue::Double(value)));
        let mut ts = PvStructure::new("time_t");
        ts.set("secondsPastEpoch", PvField::Scalar(ScalarValue::Long(sec)));
        ts.set("nanoseconds", PvField::Scalar(ScalarValue::Int(nsec)));
        ts.set("userTag", PvField::Scalar(ScalarValue::Int(0)));
        s.set("timeStamp", PvField::Structure(ts));
        (desc, PvField::Structure(s))
    }

    #[test]
    fn nt_formatting_includes_value() {
        let (desc, val) = nt_scalar_double(42.5, 0, 0);
        let out = format_nt("MY:PV", &desc, &val);
        assert!(out.contains("MY:PV"));
        assert!(out.contains("42.5"));
    }

    #[test]
    fn json_formatting_for_scalar_array() {
        let v = PvField::ScalarArray(vec![ScalarValue::Int(1), ScalarValue::Int(2)]);
        let out = format_json("X", &v);
        assert_eq!(out, "X [1,2]\n");
    }

    #[test]
    fn info_formatting_includes_struct_id() {
        let desc = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        };
        let out = format_info(&desc);
        assert!(out.contains("epics:nt/NTScalar:1.0"));
        assert!(out.contains("double value"));
    }
}
