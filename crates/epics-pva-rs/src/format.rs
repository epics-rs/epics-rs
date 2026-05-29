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
            } else if struct_id == "alarm_t" {
                // EPICS Base raw formatter appends the one-line alarm
                // summary on the `alarm_t` structure line (printer.cpp:368-372).
                let summary = format_alarm_summary(s);
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
    if id.starts_with("epics:nt/NTTable:") {
        // EPICS Base routes the `epics:nt/NTTable:1` prefix to
        // printTable (printer.cpp:414-421). printTable cowardly refuses
        // a malformed table (`return false`); we mirror that by falling
        // back to the raw formatter when the table is not well-formed.
        format_nt_table(pv_name, s).unwrap_or_else(|| format_raw(pv_name, desc, value))
    } else if id.starts_with("epics:nt/NTScalar:") {
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
    // EPICS Base NTScalar order is `<timeStamp> <value> <alarm>`
    // (pvData printer.cpp:428-434: printTimeT, value, printAlarmT). The
    // alarm summary is empty unless severity is nonzero.
    let alarm = top_alarm_summary(s);
    format!("{ts}  {val} {alarm}\n")
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
    // EPICS Base NTEnum order is `<timeStamp> <alarm> (index) choice`
    // (pvData printer.cpp:162-176: printTimeT, printAlarmT, then the enum).
    let alarm = top_alarm_summary(s);
    format!("{pv_name} {ts} {alarm}({idx}) {choice}\n")
}

/// Render an NTTable the way EPICS Base `printTable` does
/// (pvData printer.cpp:194-283): a metadata line (timeStamp / alarm),
/// then a right-aligned grid of CSV-escaped columns with per-column
/// widths and ragged columns truncated to the shortest length.
///
/// Returns `None` when the structure is not a well-formed table — no
/// `value` substructure, an empty column set, or a column that is not a
/// scalar array — so the caller falls back to the raw formatter, exactly
/// as printTable returns `false` for those cases (printer.cpp:196-205).
///
/// The PV name and a separating space are prepended here so the shape
/// matches the other NT formatters (`<pv_name> <metadata>\n<header>\n
/// <rows>`), mirroring pvAccessCPP `pvget` which streams
/// `name << ' ' << formatter` (pvtoolsSrc/pvget.cpp:76,93).
fn format_nt_table(pv_name: &str, s: &PvStructure) -> Option<String> {
    let PvField::Structure(value) = s.get_field("value")? else {
        return None;
    };
    if value.fields.is_empty() {
        return None;
    }

    // Every column must be a scalar array (printTable refuses anything
    // else, printer.cpp:200-204). Render each element to its string form,
    // then CSV-escape it (escaping happens before width measurement).
    let mut columns: Vec<(&str, Vec<String>)> = Vec::with_capacity(value.fields.len());
    for (name, field) in &value.fields {
        let cells = scalar_array_cells(field)?;
        let escaped: Vec<String> = cells.iter().map(|c| csv_escape(c)).collect();
        columns.push((name.as_str(), escaped));
    }

    // Labels from the `labels` field; a column with no corresponding
    // label falls back to its field name (printer.cpp:240-246). Labels
    // taken from the `labels` field are CSV-escaped; field-name fallbacks
    // are not (field names are already token-safe).
    let labels = string_array_values(s.get_field("labels"));
    let widths: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(i, (name, cells))| {
            let label_len = labels
                .get(i)
                .map(|l| csv_escape(l).len())
                .unwrap_or_else(|| name.len());
            cells.iter().map(String::len).fold(label_len, usize::max)
        })
        .collect();

    // Ragged columns truncate to the shortest length (printer.cpp:233).
    let nrows = columns
        .iter()
        .map(|(_, cells)| cells.len())
        .min()
        .unwrap_or(0);

    let mut out = String::new();
    let _ = write!(out, "{pv_name} ");

    // Metadata line (printer.cpp:207-222): timeStamp then alarm, each
    // followed by a space, then a newline — printed whenever either is
    // present (and an empty line otherwise). Reuses the same
    // printTimeTx/printAlarmTx-derived helpers as the NTScalar path; the
    // upstream `setw(24)` timestamp padding is approximated identically.
    if let Some(PvField::Structure(ts)) = s.get_field("timeStamp") {
        let _ = write!(out, "{} ", format_timestamp(ts));
    }
    if matches!(s.get_field("alarm"), Some(PvField::Structure(_))) {
        let _ = write!(out, "{} ", top_alarm_summary(s));
    }
    out.push('\n');

    // Header line: each label right-justified to its column width, single
    // space between columns, no trailing space (printer.cpp:264-272).
    write_table_row(
        &mut out,
        &widths,
        columns.iter().enumerate().map(|(i, (name, _))| {
            labels
                .get(i)
                .map(|l| csv_escape(l))
                .unwrap_or_else(|| (*name).to_string())
        }),
    );

    // Data rows (printer.cpp:274-282).
    for r in 0..nrows {
        write_table_row(
            &mut out,
            &widths,
            columns.iter().map(|(_, cells)| cells[r].clone()),
        );
    }

    Some(out)
}

/// Write one right-justified, single-space-separated table row (no
/// trailing space), terminated by a newline. Column cells are already
/// CSV-escaped, so each is ASCII and its byte length equals its display
/// width — `{:>width$}` aligns identically to Base's
/// `std::setw(width)<<std::right` (printer.cpp:266-270).
fn write_table_row<I: Iterator<Item = String>>(out: &mut String, widths: &[usize], cells: I) {
    let n = widths.len();
    for (c, cell) in cells.enumerate() {
        let w = widths.get(c).copied().unwrap_or(0);
        let _ = write!(out, "{cell:>w$}");
        if c + 1 != n {
            out.push(' ');
        }
    }
    out.push('\n');
}

/// Extract a scalar-array column as per-element strings, or `None` if the
/// field is not a scalar array (the case printTable refuses).
fn scalar_array_cells(field: &PvField) -> Option<Vec<String>> {
    match field {
        PvField::ScalarArray(items) => Some(items.iter().map(scalar_to_inline).collect()),
        PvField::ScalarArrayTyped(arr) => Some(
            arr.to_scalar_values()
                .iter()
                .map(scalar_to_inline)
                .collect(),
        ),
        _ => None,
    }
}

/// Read a string-array field (e.g. NTTable `labels`) into owned strings,
/// rendering any non-string element through its scalar Display. Returns
/// an empty vec when the field is absent or not a scalar array.
fn string_array_values(field: Option<&PvField>) -> Vec<String> {
    let to_strings = |items: &[ScalarValue]| -> Vec<String> {
        items
            .iter()
            .map(|sv| match sv {
                ScalarValue::String(x) => x.clone(),
                other => other.to_string(),
            })
            .collect()
    };
    match field {
        Some(PvField::ScalarArray(items)) => to_strings(items),
        Some(PvField::ScalarArrayTyped(arr)) => to_strings(&arr.to_scalar_values()),
        _ => Vec::new(),
    }
}

/// Mirror EPICS Base `csvEscape` (pvData printer.cpp:178-192): escape the
/// string with the CSV style (control characters `\a \b \f \n \r \t \v`,
/// `\\`, `\'`; a literal `"` doubled to `""` per RFC4180; any other
/// non-printable byte as `\xHH`), then wrap the whole token in double
/// quotes iff the original contained any of `"`, space, `,`, or `\`.
///
/// Escaping is byte-wise to match Base's `isprint((unsigned char)C)`, so
/// a non-ASCII UTF-8 byte becomes `\xHH`. Hex digits are emitted
/// correctly as `{:02X}`; Base's `hexdigit` has an off-by-one that maps a
/// nibble value of 9 to `@`, which this does not reproduce.
fn csv_escape(s: &str) -> String {
    let mut esc = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            0x07 => esc.push_str("\\a"),
            0x08 => esc.push_str("\\b"),
            0x0c => esc.push_str("\\f"),
            b'\n' => esc.push_str("\\n"),
            b'\r' => esc.push_str("\\r"),
            b'\t' => esc.push_str("\\t"),
            0x0b => esc.push_str("\\v"),
            b'\\' => esc.push_str("\\\\"),
            b'\'' => esc.push_str("\\'"),
            b'"' => esc.push_str("\"\""),
            0x20..=0x7e => esc.push(b as char),
            other => {
                let _ = write!(esc, "\\x{other:02X}");
            }
        }
    }
    if s.bytes()
        .any(|b| b == b'"' || b == b' ' || b == b',' || b == b'\\')
    {
        format!("\"{esc}\"")
    } else {
        esc
    }
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

/// Emit a single JSON string token, delegating all escaping to
/// `serde_json`. This is the one owner of "string → JSON token" for the
/// CLI JSON formatter: both structure member names and string scalars
/// go through it, so quotes, backslashes, control characters (newline,
/// tab, NUL, …), and non-ASCII are emitted as valid JSON instead of the
/// prior hand-rolled escaper that quoted neither keys nor control chars.
/// EPICS Base routes the same tokens through YAJL's `yg_string()`
/// (`pvData/src/json/jprint.cpp:112-164`).
fn json_string(s: &str) -> String {
    // `Value::String(..).to_string()` is the compact, fully-escaped
    // JSON token (e.g. `"a\nb"`); it never fails.
    serde_json::Value::String(s.to_owned()).to_string()
}

fn structure_to_json(s: &PvStructure) -> String {
    // Member order is preserved by iterating `fields` in declared order
    // (EPICS Base prints `names[i]` in structure order); a
    // `serde_json::Map` would re-sort keys and break that parity, so the
    // generator owns only token escaping, not container assembly.
    let parts: Vec<String> = s
        .fields
        .iter()
        .map(|(n, v)| format!("{}:{}", json_string(n), value_to_json(v)))
        .collect();
    format!("{{{}}}", parts.join(","))
}

fn scalar_to_json(v: &ScalarValue) -> String {
    match v {
        ScalarValue::String(s) => json_string(s),
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
    // EPICS Base `printTimeTx` (pvData printer.cpp:135-139) appends the
    // `userTag` after the timestamp text when the field is present and
    // nonzero (e.g. a QSRV pulse-id / event tag). The CLI dropped it.
    let user_tag = match s.get_field("userTag") {
        Some(PvField::Scalar(ScalarValue::Int(v))) => *v as i64,
        Some(PvField::Scalar(ScalarValue::Long(v))) => *v,
        Some(PvField::Scalar(ScalarValue::UInt(v))) => *v as i64,
        _ => 0,
    };
    let dt = chrono::DateTime::from_timestamp(sec, nsec);
    match dt {
        Some(dt) => {
            let local = dt.with_timezone(&chrono::Local);
            let ts = local.format("%Y-%m-%d %H:%M:%S.%3f");
            if user_tag != 0 {
                format!("{ts} {user_tag}")
            } else {
                format!("{ts}")
            }
        }
        None => "<undefined>".to_string(),
    }
}

/// One-line alarm summary, mirroring EPICS Base `printAlarmTx`
/// (pvData printer.cpp:77-106). Empty when `severity == 0`; otherwise the
/// severity label, then the status label (omitted when status 0), then a
/// non-empty message — each token followed by a single trailing space, as
/// Base streams them.
fn format_alarm_summary(alarm: &PvStructure) -> String {
    let as_i64 = |name: &str| -> i64 {
        match alarm.get_field(name) {
            Some(PvField::Scalar(ScalarValue::Int(v))) => *v as i64,
            Some(PvField::Scalar(ScalarValue::Long(v))) => *v,
            Some(PvField::Scalar(ScalarValue::UInt(v))) => *v as i64,
            Some(PvField::Scalar(ScalarValue::Short(v))) => *v as i64,
            _ => 0,
        }
    };
    let severity = as_i64("severity");
    if severity == 0 {
        return String::new();
    }
    let mut out = String::new();
    match severity {
        1 => out.push_str("MINOR "),
        2 => out.push_str("MAJOR "),
        3 => out.push_str("INVALID "),
        4 => out.push_str("UNDEFINED "),
        n => {
            let _ = write!(out, "{n} ");
        }
    }
    match as_i64("status") {
        0 => {}
        1 => out.push_str("DEVICE "),
        2 => out.push_str("DRIVER "),
        3 => out.push_str("RECORD "),
        4 => out.push_str("DB "),
        5 => out.push_str("CONF "),
        6 => out.push_str("UNDEFINED "),
        7 => out.push_str("CLIENT "),
        n => {
            let _ = write!(out, "{n} ");
        }
    }
    if let Some(PvField::Scalar(ScalarValue::String(m))) = alarm.get_field("message") {
        if !m.is_empty() {
            let _ = write!(out, "{m} ");
        }
    }
    out
}

/// Top-level alarm summary: find the `alarm` sub-structure of an NT value
/// and render it (Base `printAlarmT`, printer.cpp:109-114). Empty when no
/// alarm or `severity == 0`.
fn top_alarm_summary(s: &PvStructure) -> String {
    match s.get_field("alarm") {
        Some(PvField::Structure(a)) => format_alarm_summary(a),
        _ => String::new(),
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
    fn nt_scalar_timestamp_renders_nonzero_user_tag() {
        // EPICS Base `printTimeTx` (pvData printer.cpp:135-139) appends the
        // userTag after the timestamp when it is nonzero; pvxs/QSRV forces
        // e.g. UTAG 142 (testqsingle.cpp:280-283).
        let (desc, val) = nt_scalar_double(1.0, 1_700_000_000, 0);
        let PvField::Structure(mut s) = val else {
            panic!("nt scalar must be a structure");
        };
        if let Some(PvField::Structure(ts)) = s.get_field_mut("timeStamp") {
            ts.set("userTag", PvField::Scalar(ScalarValue::Int(142)));
        }
        let out = format_nt("MY:PV", &desc, &PvField::Structure(s));
        assert!(
            out.contains(" 142 "),
            "nonzero userTag must appear after the timestamp, got: {out:?}"
        );
    }

    #[test]
    fn nt_scalar_timestamp_omits_zero_user_tag() {
        // userTag == 0 must not be rendered (Base prints it only when nonzero).
        let (desc, val) = nt_scalar_double(1.0, 1_700_000_000, 0);
        let out = format_nt("MY:PV", &desc, &val);
        // The only standalone integer token would be a userTag; assert the
        // zero tag is absent (value 1 renders as "1", tag would be " 0 ").
        assert!(
            !out.contains(" 0 "),
            "zero userTag must not be rendered, got: {out:?}"
        );
    }

    fn alarm_struct(severity: i32, status: i32, message: &str) -> PvField {
        let mut a = PvStructure::new("alarm_t");
        a.set("severity", PvField::Scalar(ScalarValue::Int(severity)));
        a.set("status", PvField::Scalar(ScalarValue::Int(status)));
        a.set(
            "message",
            PvField::Scalar(ScalarValue::String(message.into())),
        );
        PvField::Structure(a)
    }

    #[test]
    fn nt_scalar_renders_nonzero_alarm_after_value() {
        // EPICS Base NTScalar order is `<ts> <value> <alarm>`
        // (printer.cpp:428-434).
        let (desc, val) = nt_scalar_double(1.5, 1_700_000_000, 0);
        let PvField::Structure(mut s) = val else {
            panic!("nt scalar must be a structure");
        };
        s.set("alarm", alarm_struct(2, 3, "HIGH"));
        let out = format_nt("MY:PV", &desc, &PvField::Structure(s));
        assert!(
            out.contains("MAJOR RECORD HIGH"),
            "alarm summary must render, got: {out:?}"
        );
        let vi = out.find("1.5").expect("value present");
        let ai = out.find("MAJOR").expect("alarm present");
        assert!(vi < ai, "value must precede alarm for NTScalar: {out:?}");
    }

    #[test]
    fn nt_scalar_omits_zero_severity_alarm() {
        let (desc, val) = nt_scalar_double(1.5, 1_700_000_000, 0);
        let PvField::Structure(mut s) = val else {
            panic!("nt scalar must be a structure");
        };
        s.set("alarm", alarm_struct(0, 0, "ignored"));
        let out = format_nt("MY:PV", &desc, &PvField::Structure(s));
        assert!(
            !out.contains("MINOR")
                && !out.contains("MAJOR")
                && !out.contains("INVALID")
                && !out.contains("ignored"),
            "severity 0 must render no alarm summary, got: {out:?}"
        );
    }

    #[test]
    fn alarm_summary_maps_severity_status_and_message() {
        // printer.cpp:77-106 label mapping; trailing space per token.
        let mut a = PvStructure::new("alarm_t");
        a.set("severity", PvField::Scalar(ScalarValue::Int(1)));
        a.set("status", PvField::Scalar(ScalarValue::Int(7)));
        a.set("message", PvField::Scalar(ScalarValue::String("x".into())));
        assert_eq!(format_alarm_summary(&a), "MINOR CLIENT x ");
        // severity 0 → empty regardless of status/message.
        a.set("severity", PvField::Scalar(ScalarValue::Int(0)));
        assert_eq!(format_alarm_summary(&a), "");
        // status 0 → status label omitted, message still appended.
        a.set("severity", PvField::Scalar(ScalarValue::Int(2)));
        a.set("status", PvField::Scalar(ScalarValue::Int(0)));
        assert_eq!(format_alarm_summary(&a), "MAJOR x ");
    }

    #[test]
    fn json_formatting_for_scalar_array() {
        let v = PvField::ScalarArray(vec![ScalarValue::Int(1), ScalarValue::Int(2)]);
        let out = format_json("X", &v);
        assert_eq!(out, "X [1,2]\n");
    }

    /// Extract the JSON payload from a `format_json("X", ..)` line:
    /// `"X <json>\n"` → `<json>`.
    fn json_payload(out: &str) -> &str {
        out.strip_prefix("X ")
            .and_then(|s| s.strip_suffix('\n'))
            .expect("format_json output shape")
    }

    /// A structure member name that is not a bare JSON5 identifier
    /// (space, dash) must be emitted as a quoted, escaped key, and the
    /// whole line must be strict-JSON parseable. Pre-fix the key was
    /// spliced in unquoted (`{alarm message:...}`), which no JSON parser
    /// accepts. Member order must survive (declared order, not sorted).
    #[test]
    fn json_quotes_and_escapes_structure_keys() {
        let mut s = PvStructure::new("");
        s.set("alarm message", PvField::Scalar(ScalarValue::Int(1)));
        s.set("a-b", PvField::Scalar(ScalarValue::Int(2)));
        let out = format_json("X", &PvField::Structure(s));
        let payload = json_payload(&out);
        let parsed: serde_json::Value = serde_json::from_str(payload).expect("must be strict JSON");
        assert_eq!(parsed["alarm message"], serde_json::json!(1));
        assert_eq!(parsed["a-b"], serde_json::json!(2));
        // Declared order preserved (not lexicographically re-sorted).
        assert_eq!(payload, r#"{"alarm message":1,"a-b":2}"#);
    }

    /// A string scalar carrying control characters (newline, tab, NUL,
    /// carriage return) plus a quote and a backslash must be escaped by
    /// the JSON generator, not emitted raw. The result must parse as
    /// strict JSON and round-trip back to the exact original bytes.
    #[test]
    fn json_escapes_string_control_characters() {
        let raw = "line1\nline2\tx\r\0\"q\\z";
        let v = PvField::Scalar(ScalarValue::String(raw.to_string()));
        let out = format_json("X", &v);
        let payload = json_payload(&out);
        let parsed: serde_json::Value = serde_json::from_str(payload).expect("must be strict JSON");
        assert_eq!(parsed, serde_json::Value::String(raw.to_string()));
        // No raw newline leaked into the token.
        assert!(!payload.contains('\n'));
    }

    /// A string-array element with control characters is escaped per
    /// element and the array stays strict JSON.
    #[test]
    fn json_escapes_string_array_elements() {
        let v = PvField::ScalarArray(vec![
            ScalarValue::String("a\nb".to_string()),
            ScalarValue::String("c\"d".to_string()),
        ]);
        let out = format_json("X", &v);
        let parsed: serde_json::Value =
            serde_json::from_str(json_payload(&out)).expect("must be strict JSON");
        assert_eq!(parsed, serde_json::json!(["a\nb", "c\"d"]));
    }

    /// Build an NTTable descriptor + value with the given string columns
    /// and labels (no timeStamp/alarm). The descriptor only needs the
    /// NTTable struct id; the table formatter reads the value side.
    fn nt_table(labels: &[&str], columns: &[(&str, Vec<ScalarValue>)]) -> (FieldDesc, PvField) {
        let desc = FieldDesc::Structure {
            struct_id: "epics:nt/NTTable:1.0".into(),
            fields: vec![],
        };
        let mut value = PvStructure::new("");
        for (name, cells) in columns {
            value.set(name, PvField::ScalarArray(cells.clone()));
        }
        let mut top = PvStructure::new("epics:nt/NTTable:1.0");
        if !labels.is_empty() {
            top.set(
                "labels",
                PvField::ScalarArray(
                    labels
                        .iter()
                        .map(|l| ScalarValue::String((*l).to_string()))
                        .collect(),
                ),
            );
        }
        top.set("value", PvField::Structure(value));
        (desc, PvField::Structure(top))
    }

    /// A well-formed NTTable renders as Base `printTable` does: a
    /// (here empty) metadata line, a right-justified header, and
    /// right-justified rows truncated to the shortest column. Column 0
    /// width = max(label "A"=1, cells 1/22/333) = 3; column 1 width =
    /// max(label "Beta"=4, cells x/yy) = 4; ragged columns truncate to
    /// 2 rows.
    #[test]
    fn nt_table_renders_aligned_grid() {
        let (desc, value) = nt_table(
            &["A", "Beta"],
            &[
                (
                    "colA",
                    vec![
                        ScalarValue::Int(1),
                        ScalarValue::Int(22),
                        ScalarValue::Int(333),
                    ],
                ),
                (
                    "colB",
                    vec![
                        ScalarValue::String("x".into()),
                        ScalarValue::String("yy".into()),
                    ],
                ),
            ],
        );
        let out = format_nt("PV", &desc, &value);
        assert_eq!(out, "PV \n  A Beta\n  1    x\n 22   yy\n");
    }

    /// When the `labels` field has fewer entries than columns, the
    /// missing labels fall back to the column field names
    /// (printer.cpp:240-246).
    #[test]
    fn nt_table_label_fallback_to_field_name() {
        let (desc, value) = nt_table(
            &["First"],
            &[
                ("first", vec![ScalarValue::Int(1)]),
                ("second", vec![ScalarValue::Int(2)]),
            ],
        );
        let out = format_nt("PV", &desc, &value);
        // col0 width=max("First"=5, "1"=1)=5; col1 width=max("second"=6, "2"=1)=6.
        assert_eq!(out, "PV \nFirst second\n    1      2\n");
    }

    /// `csv_escape` mirrors Base `csvEscape`: a literal `"` is doubled,
    /// control chars get backslash escapes, and the whole token is quoted
    /// only when the original held `"`, space, `,`, or `\`.
    #[test]
    fn csv_escape_matches_base_rules() {
        assert_eq!(csv_escape("plain"), "plain");
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        assert_eq!(csv_escape("a b"), "\"a b\"");
        assert_eq!(csv_escape("he\"llo"), "\"he\"\"llo\"");
        // tab is escaped to \t, but no quote/space/comma/backslash → no wrap.
        assert_eq!(csv_escape("tab\there"), "tab\\there");
        // a non-printable byte (0x01) → \x01.
        assert_eq!(csv_escape("\u{01}"), "\\x01");
    }

    /// A CSV-significant cell (comma) is quoted in the grid, and the
    /// quoted token drives the column width.
    #[test]
    fn nt_table_csv_escapes_cells() {
        let (desc, value) = nt_table(&["c"], &[("c", vec![ScalarValue::String("a,b".into())])]);
        let out = format_nt("PV", &desc, &value);
        // cell "a,b" → quoted "\"a,b\"" (len 5); label "c" (len 1) → width 5.
        assert_eq!(out, "PV \n    c\n\"a,b\"\n");
    }

    /// printTable refuses a `value` whose column is not a scalar array
    /// (printer.cpp:200-204); `format_nt_table` returns None so the
    /// caller falls back to the raw formatter.
    #[test]
    fn nt_table_rejects_non_scalar_array_column() {
        let mut value = PvStructure::new("");
        value.set("notarray", PvField::Scalar(ScalarValue::Int(7)));
        let mut top = PvStructure::new("epics:nt/NTTable:1.0");
        top.set("value", PvField::Structure(value));
        assert!(format_nt_table("PV", &top).is_none());
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
