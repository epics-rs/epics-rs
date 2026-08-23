//! pvxs-compatible output formatting for PVA values and type descriptors.
//!
//! Operates on native [`crate::pvdata`] types (`FieldDesc` / `PvField` /
//! `PvStructure`). Mirrors the layout pvxs `pvget` / `pvinfo` produce.

use std::fmt::Write as _;

use epics_base_rs::server::records::printf::format_g;

use crate::pvdata::{
    FieldDesc, PvField, PvStructure, ScalarType, ScalarValue, TypedScalarArray, VariantValue,
};

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
            write_raw_structure(out, struct_id, name, fields, s, depth);
        }
        (FieldDesc::StructureArray { struct_id, fields }, PvField::StructureArray(items)) => {
            let id = if struct_id.is_empty() {
                "structure"
            } else {
                struct_id
            };
            let _ = writeln!(out, "{indent}{id}[] {name}");
            for s in items {
                match s {
                    // Base `PVStructureArray::dumpValue`
                    // (PVStructureArray.cpp:230-239) hands each present
                    // element to the element's own
                    // `PVStructure::dumpValue` — the same writer a named
                    // structure field goes through, only with no field
                    // name — and prints `(none)` for an absent one. The
                    // element header was spelled out a second time here,
                    // which is how it drifted to `(null)`.
                    Some(s) => write_raw_structure(out, struct_id, "", fields, s, depth + 1),
                    None => {
                        let _ = writeln!(out, "{indent}    (none)");
                    }
                }
            }
        }
        (
            FieldDesc::Union {
                struct_id,
                variants,
            },
            PvField::Union {
                selector,
                variant_name,
                value,
            },
        ) => {
            let id = if struct_id.is_empty() {
                "union"
            } else {
                struct_id
            };
            let _ = writeln!(out, "{indent}{id} {name}");
            write_raw_union_member(out, variants, *selector, variant_name, value, depth + 1);
        }
        (
            FieldDesc::UnionArray {
                struct_id,
                variants,
            },
            PvField::UnionArray(items),
        ) => {
            let id = if struct_id.is_empty() {
                "union"
            } else {
                struct_id
            };
            let _ = writeln!(out, "{indent}{id}[] {name}");
            for it in items {
                match it {
                    Some(it) => write_raw_union_member(
                        out,
                        variants,
                        it.selector,
                        &it.variant_name,
                        &it.value,
                        depth + 1,
                    ),
                    None => {
                        let _ = writeln!(out, "{indent}    (none)");
                    }
                }
            }
        }
        (FieldDesc::Variant, PvField::Variant(v)) => {
            let _ = writeln!(out, "{indent}any {name}");
            write_raw_variant_member(out, v, depth + 1);
        }
        (FieldDesc::VariantArray, PvField::VariantArray(items)) => {
            let _ = writeln!(out, "{indent}any[] {name}");
            for it in items {
                match it {
                    Some(v) => write_raw_variant_member(out, v, depth + 1),
                    None => {
                        let _ = writeln!(out, "{indent}    (none)");
                    }
                }
            }
        }
        _ => {
            let tn = type_name(desc);
            let _ = writeln!(out, "{indent}{tn} {name} {}", format_value_inline(value));
        }
    }
}

/// A structure as EPICS Base `PVStructure::dumpValue` prints it: the
/// header `id fieldName` — with the `time_t` / `enum_t` / `alarm_t`
/// one-line summaries Base appends (printer.cpp:368-379) — then every
/// field of the descriptor that the value carries. The one owner for
/// both a named structure field and an element of a structure array,
/// which has the same shape with an empty field name.
fn write_raw_structure(
    out: &mut String,
    struct_id: &str,
    name: &str,
    fields: &[(String, FieldDesc)],
    s: &PvStructure,
    depth: usize,
) {
    let indent = "    ".repeat(depth);
    let id = if struct_id.is_empty() {
        "structure"
    } else {
        struct_id
    };
    if struct_id == "time_t" {
        // EPICS Base raw formatter: `id ' ' name ' '` then
        // `printTimeTx` then `'\n'` (printer.cpp:368,372-374,379).
        // The block carries the `setw(24)` padding and the trailing
        // space(s) Base streams, so the line ends with them as Base's
        // does.
        let ts_block = format_time_tx(Some(s));
        let _ = writeln!(out, "{indent}{id} {name} {ts_block}");
    } else if struct_id == "enum_t" {
        let summary = format_enum_summary(s);
        let _ = writeln!(out, "{indent}{id} {name} {summary}");
    } else if struct_id == "alarm_t" {
        // EPICS Base raw formatter appends the one-line alarm summary on
        // the `alarm_t` structure line (printer.cpp:368-372).
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

/// One union member as EPICS Base `PVUnion::dumpValue` prints it
/// (PVUnion.cpp:181-195): `(none)` when no variant is selected, otherwise
/// the selected member through ITS OWN dump one level deeper — so a
/// scalar member is the single line `type memberName value` and a
/// structure member prints every one of its fields.
///
/// Rendering the member with [`format_value_inline`] instead sent it to
/// the [`PvField`] `Display`, a diagnostic that collapses a structure to
/// its `value` subfield and drops every sibling
/// (`pvdata/structure.rs:440-452`). A diagnostic shortcut is not a
/// wire-output renderer, so the raw writer recurses into itself here.
fn write_raw_union_member(
    out: &mut String,
    variants: &[(String, FieldDesc)],
    selector: i32,
    variant_name: &str,
    value: &PvField,
    depth: usize,
) {
    let member = if selector < 0 {
        None
    } else {
        variants
            .iter()
            .find(|(n, _)| n == variant_name)
            .map(|(_, d)| d.clone())
            // A value always knows its own shape even when the union
            // descriptor and the value disagree about the member name.
            .or_else(|| value.wire_descriptor())
    };
    match member {
        Some(d) => write_raw_field(out, variant_name, &d, value, depth),
        None => {
            let _ = writeln!(out, "{}(none)", "    ".repeat(depth));
        }
    }
}

/// The `any` counterpart of [`write_raw_union_member`]: a variant union
/// stores its member with no field name, so the member line carries the
/// type and the value with an empty name between them, exactly as Base
/// streams `getID() << ' ' << getFieldName() << ' ' << value`.
fn write_raw_variant_member(out: &mut String, v: &VariantValue, depth: usize) {
    let desc = match v.value {
        PvField::Null => None,
        _ => v.desc.clone().or_else(|| v.value.wire_descriptor()),
    };
    match desc {
        Some(d) => write_raw_field(out, "", &d, &v.value, depth),
        None => {
            let _ = writeln!(out, "{}(none)", "    ".repeat(depth));
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
        return format_nt_table(pv_name, s).unwrap_or_else(|| format_raw(pv_name, desc, value));
    }
    // Every other NT shape — NTScalar, NTScalarArray, NTEnum, "or anything
    // with '.value'" — is dispatched by the TYPE of the `value` subfield,
    // not the struct ID (EPICS Base printer.cpp:422-453). Keying on the
    // value type (rather than the ID prefix, which only matched NTScalar
    // and NTEnum) is what gives NTScalarArray — and any other
    // `.value`-bearing structure — the standard one-line NT output instead
    // of the multi-line raw fallback.
    match s.get_field("value") {
        // scalar `.value`: `<timeStamp> <value> <alarm>`
        // (printer.cpp:428-434).
        Some(PvField::Scalar(_)) => format_nt_scalar(pv_name, s),
        // scalar-array `.value`: `<timeStamp> <alarm> <value>`
        // (printer.cpp:436-441) — the alarm precedes the value here,
        // unlike the scalar branch above.
        Some(PvField::ScalarArray(_)) | Some(PvField::ScalarArrayTyped(_)) => {
            format_nt_scalar_array(pv_name, s)
        }
        // structure `.value` that is an `enum_t` (index + choices): printed
        // as `(index) choice` via printEnumT (printer.cpp:443-447).
        // printEnumT returns false — and Base then falls through to raw —
        // when either field is missing, which `is_enum_t` mirrors.
        Some(PvField::Structure(es)) if is_enum_t(es) => format_nt_enum(pv_name, s),
        // No `.value`, or an unsupported value type: raw fallback
        // (printer.cpp:449-462).
        _ => format_raw(pv_name, desc, value),
    }
}

/// Whether a `.value` substructure is an `enum_t` — an `index` scalar plus
/// a `choices` array — the shape EPICS Base `printEnumT` requires before it
/// will print `(index) choice` (printer.cpp:158-160 returns false when
/// either field is absent). Both array encodings are accepted so a
/// wire-decoded NTEnum reaches [`format_nt_enum`] exactly as the prior
/// ID-prefix dispatch did.
fn is_enum_t(value: &PvStructure) -> bool {
    matches!(value.get_field("index"), Some(PvField::Scalar(_)))
        && matches!(
            value.get_field("choices"),
            Some(PvField::ScalarArray(_)) | Some(PvField::ScalarArrayTyped(_))
        )
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
    // EPICS Base NTScalar order is `printTimeT, value, printAlarmT`
    // (pvData printer.cpp:428-434). The time block already carries the
    // post-timestamp space(s) and an optional `userTag` token, so the
    // value follows it directly. The alarm summary is empty unless
    // severity is nonzero.
    let ts_block = top_time_tx(s);
    let alarm = top_alarm_summary(s);
    format!("{ts_block}{val} {alarm}\n")
}

/// Default NT output for a scalar-array `value` (NTScalarArray, or any
/// structure whose `.value` is a scalar array). Mirrors EPICS Base's
/// `scalarArray` NT branch (pvData printer.cpp:436-441):
///
///   indent; printTimeT; printAlarmT; `setprecision(6)` value; `'\n'`
///
/// so the order is `<pv_name> <timeStamp>  <alarm><value>\n`. Two points
/// of asymmetry with the NTScalar one-liner ([`nt_payload`]) are faithful
/// to Base: the alarm is printed BEFORE the array value (not after), and
/// the array value is the last token before the newline. The array is
/// rendered through [`format_value_inline`], which already applies the
/// six-significant-digit float precision (`std::setprecision(6)`,
/// printer.cpp:440) and the per-type element separator.
fn format_nt_scalar_array(pv_name: &str, s: &PvStructure) -> String {
    let ts_block = top_time_tx(s);
    let alarm = top_alarm_summary(s);
    let val = s
        .get_field("value")
        .map(format_value_inline)
        .unwrap_or_default();
    format!("{pv_name} {ts_block}{alarm}{val}\n")
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

fn format_nt_enum(pv_name: &str, s: &PvStructure) -> String {
    let ts_block = top_time_tx(s);
    let (idx, choice) = match s.get_field("value") {
        Some(PvField::Structure(es)) => enum_index_and_choice(es),
        _ => ("0".to_string(), String::new()),
    };
    // EPICS Base NTEnum order is `<timeStamp> <alarm> (index) choice`
    // (pvData printer.cpp:162-176: printTimeT, printAlarmT, then the enum).
    // The time block carries the post-timestamp space(s); the index
    // follows the alarm directly.
    let alarm = top_alarm_summary(s);
    format!("{pv_name} {ts_block}{alarm}({idx}) {choice}\n")
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
                .map(|l| csv_escape(l.as_bytes()).len())
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
        // The block carries the `setw(24)` pad + post-timestamp space and
        // any `userTag`; no extra separator needed.
        let _ = write!(out, "{}", format_time_tx(Some(ts)));
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
                .map(|l| csv_escape(l.as_bytes()))
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

/// Extract a scalar-array column as per-element raw bytes, or `None` if the
/// field is not a scalar array (the case printTable refuses). Byte-valued
/// so a non-UTF-8 string cell reaches [`csv_escape`] intact (pvxs escapes
/// the original bytes, not a lossy text view); numeric cells are ASCII.
fn scalar_array_cells(field: &PvField) -> Option<Vec<Vec<u8>>> {
    let cell_bytes = |sv: &ScalarValue| -> Vec<u8> {
        match sv {
            ScalarValue::String(s) => s.as_bytes().to_vec(),
            other => scalar_cell_text(other).into_bytes(),
        }
    };
    match field {
        PvField::ScalarArray(items) => Some(items.iter().map(cell_bytes).collect()),
        PvField::ScalarArrayTyped(arr) => {
            Some(arr.to_scalar_values().iter().map(cell_bytes).collect())
        }
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
                ScalarValue::String(x) => x.as_str_lossy().into_owned(),
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
fn csv_escape(s: &[u8]) -> String {
    let mut esc = String::with_capacity(s.len());
    for &b in s {
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
    if s.iter()
        .any(|&b| b == b'"' || b == b' ' || b == b',' || b == b'\\')
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
        ScalarValue::String(s) => json_string(&s.as_str_lossy()),
        ScalarValue::Float(f) => json_double(*f as f64),
        ScalarValue::Double(f) => json_double(*f),
        other => format!("{other}"),
    }
}

/// A double as C `yajl_gen_double` writes it (libCom yajl_gen.c:222-247),
/// which is the generator every EPICS JSON writer goes through:
///
/// ```c
/// if (isnan(number)) strcpy(i, "NaN");
/// else if (isinf(number)) sprintf(i, "%cInfinity", number < 0 ? '-' : '+');
/// else { sprintf(i, "%.17g", number);
///        if (strspn(i, "0123456789-") == strlen(i)) strcat(i, ".0"); }
/// ```
///
/// The sign character on `Infinity` is explicit and `+Infinity` is the
/// only spelling a JSON5 reader accepts; Rust's `{}` writes `inf`, which
/// no JSON or JSON5 parser reads back. `%.17g` is also not the shortest
/// round-trip form Rust prints, and the `.0` suffix belongs only on a
/// rendering that is nothing but digits and `-`, so it must not be
/// appended to `1e+30`.
fn json_double(x: f64) -> String {
    if x.is_nan() {
        return "NaN".to_string();
    }
    if x.is_infinite() {
        return if x < 0.0 { "-Infinity" } else { "+Infinity" }.to_string();
    }
    let s = format_g(x, 17);
    if s.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
        format!("{s}.0")
    } else {
        s
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn format_enum_summary(s: &PvStructure) -> String {
    let (idx, choice) = enum_index_and_choice(s);
    format!("({idx}) {choice}")
}

/// The index text and choice text of an `enum_t`, the one owner of both
/// halves for the NT line ([`format_nt_enum`]) and the raw structure line
/// ([`format_enum_summary`]).
///
/// EPICS Base `printEnumT` (pvData printer.cpp:168-175) reads the index
/// with `getAs<uint32>`, which REINTERPRETS a negative int32 rather than
/// clamping it: index -1 is 4294967295, out of range for any `choices`
/// array, so Base prints `(4294967295) <undefined>`. The printed number
/// and the array subscript are the same uint32, which is why they are
/// produced together here — parsing the rendered text into `usize` and
/// falling back to 0 printed choice 0, naming a state the PV is not in.
fn enum_index_and_choice(s: &PvStructure) -> (String, String) {
    let raw = s
        .get_field("index")
        .map(format_value_inline)
        .unwrap_or_else(|| "0".to_string());
    // An index that is not an integer at all cannot name a choice, so it
    // takes the same path as a negative one.
    let n = raw.parse::<i64>().unwrap_or(-1) as u32;
    (
        n.to_string(),
        enum_choice_for_index(s.get_field("choices"), n as usize),
    )
}

/// Extract the NTEnum choice text for `index` from a `choices` field that
/// may be either an untyped [`PvField::ScalarArray`] (the in-crate builder
/// shape) or a wire-decoded
/// [`PvField::ScalarArrayTyped`]`(`[`TypedScalarArray::String`]`)`. The
/// descriptor-driven decoder returns string arrays in the typed form, so a
/// branch that only matched `ScalarArray` printed an empty choice for real
/// interop data (a pvxs/QSRV `enum_t`). Mirrors EPICS Base `printEnumT`
/// (pvData printer.cpp:168-175): an in-range index renders the
/// `maybeQuote`-escaped choice, an out-of-range index renders `<undefined>`.
fn enum_choice_for_index(choices: Option<&PvField>, index: usize) -> String {
    match choices {
        Some(PvField::ScalarArray(items)) => items
            .get(index)
            .map(enum_choice_text)
            .unwrap_or_else(|| "<undefined>".to_string()),
        Some(PvField::ScalarArrayTyped(TypedScalarArray::String(items))) => items
            .get(index)
            .map(|s| maybe_quote(s.as_bytes()))
            .unwrap_or_else(|| "<undefined>".to_string()),
        // No `choices`, or a non-string array (a malformed enum_t): Base's
        // `getSubField<PVStringArray>` would be null and `printEnumT`
        // returns false; here the caller is already committed to the enum
        // line, so emit no choice text.
        _ => String::new(),
    }
}

/// Render an NTEnum choice for display, mirroring EPICS Base `printEnumT`
/// which streams the selected choice through `maybeQuote(ch[I])`
/// (pvData printer.cpp:168-175). A non-string choice element (malformed
/// enum) falls back to its plain scalar text.
fn enum_choice_text(v: &ScalarValue) -> String {
    match v {
        ScalarValue::String(s) => maybe_quote(s.as_bytes()),
        other => scalar_cell_text(other),
    }
}

/// The CLI sentinel for a timestamp with no valid time (missing or zero
/// `secondsPastEpoch`). EPICS Base never emits this — it would format the
/// 1990 EPICS epoch — but the Rust CLI prints it instead of a fake date.
const UNDEFINED_TS: &str = "<undefined>";

/// The timestamp text EPICS Base would format from a `time_t` structure
/// (`YYYY-MM-DD HH:MM:SS.mmm`, local time), or `None` when the CLI renders
/// the time as undefined ([`UNDEFINED_TS`]): missing/zero
/// `secondsPastEpoch`, or a value `chrono` cannot represent.
fn timestamp_text(s: &PvStructure) -> Option<String> {
    let sec = match s.get_field("secondsPastEpoch") {
        Some(PvField::Scalar(ScalarValue::Long(v))) => *v,
        Some(PvField::Scalar(ScalarValue::Int(v))) => *v as i64,
        _ => return None,
    };
    if sec == 0 {
        return None;
    }
    let nsec = match s.get_field("nanoseconds") {
        Some(PvField::Scalar(ScalarValue::Int(v))) => *v as u32,
        Some(PvField::Scalar(ScalarValue::UInt(v))) => *v,
        _ => 0,
    };
    let dt = chrono::DateTime::from_timestamp(sec, nsec)?;
    Some(
        dt.with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S.%3f")
            .to_string(),
    )
}

/// The `userTag` of a `time_t` structure, or `0` when absent. Base reads
/// this as `int64` (printer.cpp:123,136).
fn timestamp_user_tag(s: &PvStructure) -> i64 {
    match s.get_field("userTag") {
        Some(PvField::Scalar(ScalarValue::Int(v))) => *v as i64,
        Some(PvField::Scalar(ScalarValue::Long(v))) => *v,
        Some(PvField::Scalar(ScalarValue::UInt(v))) => *v as i64,
        _ => 0,
    }
}

/// Render the EPICS Base `printTimeTx` token block for a `timeStamp`
/// substructure (pvData printer.cpp:116-140), the single owner of the
/// `time_t` textual output contract shared by the raw `time_t` line and
/// the NTScalar / NTScalarArray / NTEnum / NTTable formatters.
///
/// Base streams `std::setw(24) << std::left << timeText << ' '`, then —
/// when `userTag` is present and nonzero — `tag << ' '`. So the block is
/// the timestamp text left-justified to width 24 then a space, followed
/// by the optional `tag` and its own trailing space. The returned string
/// ends in that trailing space (two spaces before the value for the
/// common 23-char timestamp, since `setw(24)` adds exactly one pad
/// space), matching what Base streams before the value/alarm.
///
/// Two contract points the previous per-caller string handling dropped:
///  - the `userTag` is a separate token *after* the time text, so it is
///    emitted even when the time is undefined — Base reads `userTag`
///    after formatting the time and appends a nonzero tag regardless,
///    whereas the old code returned early on an undefined timestamp and
///    silently dropped the tag;
///  - the tag carries its own trailing space (Base `tag << ' '`), instead
///    of one space before and two after as the embedded-in-string form
///    produced.
///
/// `ts == None` (no `timeStamp` field at all) renders the undefined
/// block. The Rust [`UNDEFINED_TS`] sentinel is not padded to the
/// 24-column — Base never produces it, so there is no Base column to
/// align it to; its established two-space spacing is preserved.
fn format_time_tx(ts: Option<&PvStructure>) -> String {
    use std::fmt::Write;
    let (text, tag) = match ts {
        Some(ts) => (timestamp_text(ts), timestamp_user_tag(ts)),
        None => (None, 0),
    };
    let mut out = match text {
        // Real timestamp: `setw(24) << left << timeText << ' '`
        // (printer.cpp:134).
        Some(t) => format!("{t:<24} "),
        // Undefined sentinel: keep two-space spacing (see doc above).
        None => format!("{UNDEFINED_TS}  "),
    };
    if tag != 0 {
        // Base `tag << ' '` (printer.cpp:138) — tag then its own space.
        let _ = write!(out, "{tag} ");
    }
    out
}

/// The `printTimeTx` block for the `timeStamp` substructure of an NT
/// top-level structure (or the undefined block when there is none).
fn top_time_tx(top: &PvStructure) -> String {
    match top.get_field("timeStamp") {
        Some(PvField::Structure(ts)) => format_time_tx(Some(ts)),
        _ => format_time_tx(None),
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
        PvField::ScalarArray(items) => format_scalar_array_inline(items),
        // Typed wire-decoded arrays render through the same display path
        // as untyped ones — not the generic `TypedScalarArray::Display`,
        // which is a diagnostic representation that does not apply Base's
        // `maybeQuote` string rule (EPICS Base routes both through the
        // PVField stream operator, printer.cpp:382-386).
        PvField::ScalarArrayTyped(arr) => format_scalar_array_inline(&arr.to_scalar_values()),
        PvField::Null => String::new(),
        other => format!("{other}"),
    }
}

/// Render a scalar array for raw / NT inline display: `[` elements `]`.
/// Each element goes through [`scalar_to_inline`], so string elements are
/// `maybeQuote`-escaped while numeric elements print bare with six
/// significant digits. The element separator follows Base's per-type
/// array dump: a string array uses `", "`
/// (`PVValueArray<std::string>::dumpValue`, PVDataCreateFactory.cpp:
/// 240-251) while every numeric/bool array uses a bare `","`
/// (`PVValueArray<T>::dumpValue`, :216-229).
fn format_scalar_array_inline(items: &[ScalarValue]) -> String {
    let parts: Vec<String> = items.iter().map(scalar_to_inline).collect();
    let sep = if matches!(items.first(), Some(ScalarValue::String(_))) {
        ", "
    } else {
        ","
    };
    format!("[{}]", parts.join(sep))
}

/// Inline display rendering of a scalar for raw / NT output. Mirrors the
/// EPICS Base PVField stream operator: a string is `maybeQuote`-escaped
/// (`PVString::dumpValue`, PVDataCreateFactory.cpp:145-149); a float is
/// printed with six significant digits, matching Base's C++ stream
/// default precision for `PVScalarValue<T>::dumpValue` (`o << get()`,
/// PVDataCreateFactory.cpp:64-68) and the NT formatter's explicit
/// `precision(6)` (printer.cpp:428-440); other scalars print exact text.
fn scalar_to_inline(v: &ScalarValue) -> String {
    match v {
        ScalarValue::String(s) => maybe_quote(s.as_bytes()),
        ScalarValue::Double(f) => format_g(*f, 6),
        ScalarValue::Float(f) => format_g(*f as f64, 6),
        other => scalar_cell_text(other),
    }
}

/// Raw, unquoted scalar text used for table cells (which apply their own
/// CSV escaping) — distinct from [`scalar_to_inline`], whose strings are
/// `maybeQuote`-escaped for inline display. Keeping the two separate
/// prevents a table cell from being `maybeQuote`-escaped and then
/// `csv_escape`-escaped a second time.
///
/// Mirrors Base `PVScalarArray::getAs<std::string>()`, the conversion
/// `printTable` applies to every column (pvData printer.cpp:252): each
/// element passes through `castUnsafe<std::string, FROM>`
/// (typeCast.h:101-110), i.e. `std::ostringstream << value` at the C++
/// default stream precision. For `double`/`float` that default is six
/// significant digits (`defaultfloat`), so numeric cells use
/// `format_g(_, 6)` — the same renderer the inline NT path uses — NOT
/// Rust's shortest-round-trip `{f}`, which would print extra digits Base
/// never emits and would over-widen the column. Bool maps to
/// `true`/`false` (`print_convolute<boolean>`) and integers print in
/// decimal.
fn scalar_cell_text(v: &ScalarValue) -> String {
    match v {
        ScalarValue::Double(f) => format_g(*f, 6),
        ScalarValue::Float(f) => format_g(*f as f64, 6),
        ScalarValue::String(s) => s.as_str_lossy().into_owned(),
        ScalarValue::Boolean(b) => (if *b { "true" } else { "false" }).to_string(),
        other => format!("{other}"),
    }
}

/// Render a PVA display string the way EPICS Base `maybeQuote` does
/// (pvData printer.cpp:521-548): if the string contains a space, a quote,
/// a backslash, an apostrophe, a control character (`\a \b \f \n \r \t
/// \v`), or any non-printable byte, wrap it in double quotes and escape
/// it; otherwise emit it verbatim. The escaping uses Base's default
/// (non-CSV) `escape` style, so a literal `"` becomes `\"` (printer.cpp:
/// 485-516) — distinct from the CSV style [`csv_escape`] uses, where `"`
/// is doubled.
fn maybe_quote(s: &[u8]) -> String {
    let needs_quote = s.iter().any(|&b| {
        matches!(
            b,
            0x07 | 0x08 | 0x0c | b'\n' | b'\r' | b'\t' | b' ' | 0x0b | b'\\' | b'\'' | b'"'
        ) || !(0x20..=0x7e).contains(&b)
    });
    if needs_quote {
        format!("\"{}\"", escape_display(s))
    } else {
        // Not quoted ⟹ every byte is printable ASCII, so the lossy view is
        // exact here (no replacement characters are introduced).
        String::from_utf8_lossy(s).into_owned()
    }
}

/// Byte-wise escape body shared by the two references this formatter
/// serves. They agree on every escape — named backslash escapes for
/// `\a \b \f \n \r \t \v`, `\\`, `\'`, `\"`, any other non-printable
/// byte as `\xHH`, printable ASCII verbatim — and on the printable test
/// (Base `isprint((unsigned char)C)`, pvxs `c>=' ' && c<='~'`), so a
/// non-ASCII UTF-8 byte becomes `\xHH` under both. They disagree on one
/// thing only, the case of the hex digits, which is why this takes
/// `hex_upper` rather than being two copies. Call it through
/// [`escape_display`] or [`escape_pvxs`], never directly, so each call
/// site names the reference it is reproducing.
fn escape_bytes(s: &[u8], hex_upper: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s {
        match b {
            0x07 => out.push_str("\\a"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x0b => out.push_str("\\v"),
            b'\\' => out.push_str("\\\\"),
            b'\'' => out.push_str("\\'"),
            b'"' => out.push_str("\\\""),
            0x20..=0x7e => out.push(b as char),
            other if hex_upper => {
                let _ = write!(out, "\\x{other:02X}");
            }
            other => {
                let _ = write!(out, "\\x{other:02x}");
            }
        }
    }
    out
}

/// EPICS Base's default `escape` style (`style_t::C`, pvData
/// printer.cpp:485-516), whose `hexdigit` (printer.cpp:467-473) is
/// uppercase. That `hexdigit` also carries an off-by-one mapping a nibble
/// of 9 to `@` (CBUG-H2 in doc/upstream-c-bugs.md); this does not
/// reproduce it and emits correct hex.
fn escape_display(s: &[u8]) -> String {
    escape_bytes(s, true)
}

/// pvxs's `Escaper` (src/util.cpp:230-235), which writes
/// `"\\x" << std::hex << setw(2) << setfill('0')` with no
/// `std::uppercase`, so its hex digits are lowercase. Every printer on
/// the `datafmt.cpp` path goes through it (`escape(...)` at
/// datafmt.cpp:27, 38, 153), so the `-F tree` and Delta printers here
/// must too.
fn escape_pvxs(s: &[u8]) -> String {
    escape_bytes(s, false)
}

// ─── pvxs `Value::format()` — Tree / Delta output (datafmt.cpp) ──────────────
//
// A faithful port of pvxs `operator<<(ostream, Value::Fmt)` and its two
// helper visitors `FmtTree` / `FmtDelta` (src/datafmt.cpp). These drive the
// `-F tree` / `-F delta` output of `pvget`/`pvmonitor` and the type tree of
// `pvinfo` (`Value::format().showValue(false)`). They are kept separate from
// the EPICS-Base `-M raw|nt|json` formatters above: the pvxs formatter uses
// the pvxs `TypeCode::name()` strings (`int32_t`, not `int`), the pvxs array
// dump (`{N}[a, b, ...]`), and the pvxs default-`escape` string style.

/// pvxs `Value::Fmt::format_t` (data.h:788-791): the two `-F` output modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueFormat {
    /// Full nested structure with `{ }` braces (pvxs `Tree`, the
    /// `Value::Fmt` default). `pvinfo` uses this with values hidden.
    Tree,
    /// Flat, dotted-path lines (pvxs `Delta`); the `pvget`/`pvmonitor`
    /// default. Only the *marked* fields are shown.
    Delta,
}

/// pvxs `Value::Fmt` builder state (data.h:785-805): output mode, the
/// per-array element limit (`-#`; 0 = unlimited, matching pvxs `_limit`),
/// and whether scalar values are shown (`pvinfo` clears this).
#[derive(Debug, Clone, Copy)]
pub struct ValueFmt {
    pub format: ValueFormat,
    pub array_limit: usize,
    pub show_value: bool,
}

impl Default for ValueFmt {
    fn default() -> Self {
        // pvxs `Value::Fmt` defaults (data.h:787-792): Tree, no array
        // limit, showValue=true.
        Self {
            format: ValueFormat::Tree,
            array_limit: 0,
            show_value: true,
        }
    }
}

/// Parse a `-F` argument (pvxs get.cpp/monitor.cpp:78-86) into a
/// [`ValueFormat`]. A `None` input (the flag is absent) yields `None`, so the
/// caller keeps its `-M` mode. `tree`/`delta` map directly; any other value
/// warns on stderr and falls back to `Delta`, mirroring pvxs's
/// "Warning: ignoring unknown format '…'".
pub fn parse_value_format(arg: Option<&str>) -> Option<ValueFormat> {
    let s = arg?;
    Some(match s {
        "tree" => ValueFormat::Tree,
        "delta" => ValueFormat::Delta,
        other => {
            eprintln!("Warning: ignoring unknown format '{other}'");
            ValueFormat::Delta
        }
    })
}

/// pvxs `pvinfo` per-PV output block (info.cpp:90-94):
/// `cout << argv[n] << " from " << peerName << "\n" <<
/// result().format().showValue(false)`. Emits the `<pv> from <peer>` header
/// line, then the type tree with values hidden — `Tree` mode, `base_depth=0`
/// (info.cpp streams the formatter with no `Indented` wrapper, unlike
/// get/monitor which pass 1), and `value=None` because a describe carries
/// only the introspection, not data. This is the pvxs-compatible replacement
/// for the prior Rust-specific `<pv>` / `Server:` / `Type:` label block.
pub fn format_info_value(pv_name: &str, peer: std::net::SocketAddr, desc: &FieldDesc) -> String {
    let fmt = ValueFmt {
        format: ValueFormat::Tree,
        array_limit: 0,
        show_value: false,
    };
    format!(
        "{pv_name} from {peer}\n{}",
        format_value(desc, None, &fmt, None, 0)
    )
}

/// Render a descriptor + value the way pvxs `operator<<(ostream, Value::Fmt)`
/// does (datafmt.cpp:312-326).
///
/// `base_depth` is the ambient indent the caller has already established:
/// `pvget`/`pvmonitor` print the PV-name line and then wrap the formatter in
/// `Indented I(std::cout)` (get.cpp:112-113, monitor.cpp:142-143), so they
/// pass `1`; `pvinfo` streams the formatter with no wrapper
/// (info.cpp:90-94), so it passes `0`.
///
/// `value` is optional so a type-only `pvinfo` describe (introspection but
/// no data) can render the `Tree` with `show_value=false`. `Delta` requires
/// a value (it walks the marked fields); with `value=None` it yields an
/// empty string.
///
/// `marked` selects which leaves the `Delta` format prints (pvxs
/// `Value::imarked()`): `None` treats every present leaf as marked — a GET
/// marks every field it returns, and a monitor's first update is full — while
/// `Some(set)` restricts to the dotted leaf paths in `set` (a monitor's
/// per-update changed-field set).
pub fn format_value(
    desc: &FieldDesc,
    value: Option<&PvField>,
    fmt: &ValueFmt,
    marked: Option<&std::collections::HashSet<String>>,
    base_depth: usize,
) -> String {
    let mut out = String::new();
    match fmt.format {
        ValueFormat::Tree => {
            // datafmt.cpp:315-317: leading indent{}, then show(top, "").
            out.push_str(&pvxs_indent(base_depth));
            tree_show(&mut out, desc, value, "", fmt, base_depth);
        }
        ValueFormat::Delta => {
            // datafmt.cpp:319-320: FmtDelta{}.top("", *top, true).
            if value.is_some() {
                delta_top(&mut out, "", desc, value, true, fmt, marked, base_depth);
            }
        }
    }
    out
}

/// pvxs `indent{}` (util.cpp:128-139): four spaces per `Indented` level.
fn pvxs_indent(depth: usize) -> String {
    "    ".repeat(depth)
}

/// pvxs `TypeCode::name()` (type.cpp:126-166) — note these differ from the
/// EPICS-Base names [`type_name`] uses (`int32_t`, not `int`).
fn pvxs_type_name(desc: &FieldDesc) -> String {
    match desc {
        FieldDesc::Scalar(st) => pvxs_scalar_name(*st).to_string(),
        FieldDesc::ScalarArray(st) => format!("{}[]", pvxs_scalar_name(*st)),
        FieldDesc::Variant => "any".to_string(),
        FieldDesc::VariantArray => "any[]".to_string(),
        FieldDesc::Structure { .. } => "struct".to_string(),
        FieldDesc::StructureArray { .. } => "struct[]".to_string(),
        FieldDesc::Union { .. } => "union".to_string(),
        FieldDesc::UnionArray { .. } => "union[]".to_string(),
    }
}

fn pvxs_scalar_name(st: ScalarType) -> &'static str {
    match st {
        ScalarType::Boolean => "bool",
        ScalarType::Byte => "int8_t",
        ScalarType::Short => "int16_t",
        ScalarType::Int => "int32_t",
        ScalarType::Long => "int64_t",
        ScalarType::UByte => "uint8_t",
        ScalarType::UShort => "uint16_t",
        ScalarType::UInt => "uint32_t",
        ScalarType::ULong => "uint64_t",
        ScalarType::Float => "float",
        ScalarType::Double => "double",
        ScalarType::String => "string",
    }
}

/// The struct/union id of a compound descriptor (`Value::id()`), or `None`
/// for a scalar / `any`.
fn compound_id(desc: &FieldDesc) -> Option<&str> {
    match desc {
        FieldDesc::Structure { struct_id, .. }
        | FieldDesc::StructureArray { struct_id, .. }
        | FieldDesc::Union { struct_id, .. }
        | FieldDesc::UnionArray { struct_id, .. } => Some(struct_id.as_str()),
        _ => None,
    }
}

/// pvxs `type.kind()==Kind::Compound` — struct / union / any (and their
/// arrays); false for scalars and scalar arrays.
fn is_compound(desc: &FieldDesc) -> bool {
    !matches!(desc, FieldDesc::Scalar(_) | FieldDesc::ScalarArray(_))
}

/// pvxs `shared_array::format().limit(n)` (sharedarray.cpp `showArr`):
/// `{count}[e, e, ...]` with a `", "` separator for every element type, a
/// `limit==0` meaning unlimited, and `...` when the limit is reached. `int8`
/// / `uint8` render as signed / unsigned integers (not chars), `bool` as
/// `1`/`0` (default `ostream<<bool`, no `boolalpha`), float / double with the
/// six-significant-digit C++ stream default, and strings as `"<escape>"`.
fn pvxs_array(items: &[ScalarValue], limit: usize) -> String {
    let count = items.len();
    let lim = if limit == 0 { usize::MAX } else { limit };
    let mut out = format!("{{{count}}}[");
    for (i, v) in items.iter().enumerate() {
        if i != 0 {
            out.push_str(", ");
        }
        if i >= lim {
            out.push_str("...");
            break;
        }
        out.push_str(&pvxs_array_elem(v));
    }
    out.push(']');
    out
}

fn pvxs_array_elem(v: &ScalarValue) -> String {
    match v {
        ScalarValue::Boolean(b) => (if *b { "1" } else { "0" }).to_string(),
        ScalarValue::String(s) => format!("\"{}\"", escape_pvxs(s.as_bytes())),
        ScalarValue::Float(f) => format_g(*f as f64, 6),
        ScalarValue::Double(f) => format_g(*f, 6),
        // signed / unsigned integer types print as decimal via Display.
        other => other.to_string(),
    }
}

// ── FmtTree (datafmt.cpp:124-308) ───────────────────────────────────────────

/// pvxs `FmtTree::show` (datafmt.cpp:187-307): emit at least one full line
/// for `desc`/`value` under field name `member`. Desc-driven so a type-only
/// `pvinfo` describe renders without a value; the value (when present) feeds
/// `show_value`.
fn tree_show(
    out: &mut String,
    desc: &FieldDesc,
    value: Option<&PvField>,
    member: &str,
    fmt: &ValueFmt,
    depth: usize,
) {
    // type name + struct/union id. The Tree format streams the id verbatim
    // (datafmt.cpp:198-201: `strm<<id`, no escaping) — unlike Delta.
    out.push_str(&pvxs_type_name(desc));
    if let Some(id) = compound_id(desc) {
        if !id.is_empty() {
            let _ = write!(out, " \"{id}\"");
        }
    }

    // Scalar / scalar-array: `type member = value` (datafmt.cpp:203-212).
    if !is_compound(desc) {
        if !member.is_empty() {
            out.push(' ');
            out.push_str(member);
        }
        if fmt.show_value {
            if let Some(v) = value {
                out.push_str(" = ");
                tree_show_value(out, v, fmt);
            }
        }
        out.push('\n');
        return;
    }

    // Compound branches (datafmt.cpp:214-306), in the same precedence order.
    let sv = fmt.show_value;
    let inline = matches!(desc, FieldDesc::Variant)
        || (!sv && matches!(desc, FieldDesc::VariantArray))
        || (sv && matches!(desc, FieldDesc::Union { .. }));
    let braced = matches!(desc, FieldDesc::Structure { .. })
        || matches!(desc, FieldDesc::Union { .. })
        || (!sv && matches!(desc, FieldDesc::StructureArray { .. }))
        || (!sv && matches!(desc, FieldDesc::UnionArray { .. }));

    if inline {
        tree_show_inline(out, desc, value, member, fmt, depth);
    } else if braced {
        tree_show_braced(out, desc, value, member, fmt, depth);
    } else {
        tree_show_array(out, desc, value, member, fmt, depth);
    }
}

/// pvxs scalar value rendering for the Tree format (datafmt.cpp:128-184,
/// `FmtTree::show_value`).
fn tree_show_value(out: &mut String, value: &PvField, fmt: &ValueFmt) {
    match value {
        PvField::Scalar(sv) => match sv {
            ScalarValue::Boolean(b) => out.push_str(if *b { "true" } else { "false" }),
            ScalarValue::Float(f) => out.push_str(&format_g(*f as f64, 6)),
            ScalarValue::Double(f) => out.push_str(&format_g(*f, 6)),
            ScalarValue::String(s) => {
                let _ = write!(out, "\"{}\"", escape_pvxs(s.as_bytes()));
            }
            other => {
                let _ = write!(out, "{other}");
            }
        },
        PvField::ScalarArray(items) => out.push_str(&pvxs_array(items, fmt.array_limit)),
        PvField::ScalarArrayTyped(arr) => {
            out.push_str(&pvxs_array(&arr.to_scalar_values(), fmt.array_limit))
        }
        _ => {}
    }
}

/// pvxs Tree inline branch (datafmt.cpp:214-238): `any NAME = VAL` /
/// `union NAME.MEM TYPE = VAL`. The type name (and id) have already been
/// emitted by [`tree_show`]; `member` is emitted here, before the union
/// selector (datafmt.cpp:224-230).
fn tree_show_inline(
    out: &mut String,
    desc: &FieldDesc,
    value: Option<&PvField>,
    member: &str,
    fmt: &ValueFmt,
    depth: usize,
) {
    if !member.is_empty() {
        out.push(' ');
        out.push_str(member);
    }
    match desc {
        // (showValue is implied true for the Union inline branch.)
        FieldDesc::Union { variants, .. } => match value {
            Some(PvField::Union {
                selector,
                variant_name,
                value: inner,
            }) if *selector >= 0 => {
                out.push('.');
                out.push_str(variant_name);
                out.push(' ');
                let cdesc = variants
                    .iter()
                    .find(|(n, _)| n == variant_name)
                    .map(|(_, d)| d.clone())
                    .unwrap_or_else(|| inner.descriptor());
                tree_show(out, &cdesc, Some(inner), "", fmt, depth);
            }
            // null union → pvxs `show(empty)` prints " null".
            _ => out.push_str(" null\n"),
        },
        FieldDesc::Variant => {
            if fmt.show_value {
                out.push(' ');
                match value {
                    Some(PvField::Variant(v)) => {
                        let cdesc = v.desc.clone().unwrap_or_else(|| v.value.descriptor());
                        tree_show(out, &cdesc, Some(&v.value), "", fmt, depth);
                    }
                    Some(other) if !matches!(other, PvField::Null) => {
                        let cdesc = other.descriptor();
                        tree_show(out, &cdesc, Some(other), "", fmt, depth);
                    }
                    _ => out.push_str("null\n"),
                }
            } else {
                out.push('\n');
            }
        }
        // `any[]` with showValue=false: just terminate the line.
        _ => out.push('\n'),
    }
}

/// pvxs Tree braced branch (datafmt.cpp:239-277): `struct "id" { ... } NAME`.
fn tree_show_braced(
    out: &mut String,
    desc: &FieldDesc,
    value: Option<&PvField>,
    member: &str,
    fmt: &ValueFmt,
    depth: usize,
) {
    out.push_str(" {");
    let children: Vec<(&String, &FieldDesc)> = match desc {
        FieldDesc::Structure { fields, .. } | FieldDesc::StructureArray { fields, .. } => {
            fields.iter().map(|(n, d)| (n, d)).collect()
        }
        FieldDesc::Union { variants, .. } | FieldDesc::UnionArray { variants, .. } => {
            variants.iter().map(|(n, d)| (n, d)).collect()
        }
        _ => Vec::new(),
    };
    // A Structure WITH show_value supplies child values for the recursion;
    // the !show_value union/array members carry no value.
    let parent = match value {
        Some(PvField::Structure(s)) => Some(s),
        _ => None,
    };
    let mut first = true;
    for (name, cdesc) in &children {
        if first {
            out.push('\n');
        }
        out.push_str(&pvxs_indent(depth + 1));
        let cval = parent.and_then(|s| s.get_field(name));
        tree_show(out, cdesc, cval, name, fmt, depth + 1);
        first = false;
    }
    if !first {
        out.push_str(&pvxs_indent(depth));
    }
    out.push('}');
    if !member.is_empty() {
        out.push(' ');
        out.push_str(member);
    }
    out.push('\n');
}

/// pvxs Tree array-of-compound branch (datafmt.cpp:278-306):
/// `struct[] NAME = {N}[ ... ]`.
fn tree_show_array(
    out: &mut String,
    desc: &FieldDesc,
    value: Option<&PvField>,
    member: &str,
    fmt: &ValueFmt,
    depth: usize,
) {
    if !member.is_empty() {
        out.push(' ');
        out.push_str(member);
    }
    let elems = array_elements(desc, value);
    let _ = write!(out, " = {{{}}}[", elems.len());
    let lim = if fmt.array_limit == 0 {
        usize::MAX
    } else {
        fmt.array_limit
    };
    let mut shown = 0usize;
    for (edesc, eval) in &elems {
        if shown == 0 {
            out.push('\n');
        }
        out.push_str(&pvxs_indent(depth + 1));
        if shown >= lim {
            out.push_str("...\n");
            break;
        }
        match eval {
            Some(v) => tree_show(out, edesc, Some(v), "", fmt, depth + 1),
            None => out.push_str("null\n"),
        }
        shown += 1;
    }
    if shown > 0 {
        out.push_str(&pvxs_indent(depth));
    }
    out.push_str("]\n");
}

// ── FmtDelta (datafmt.cpp:13-122) ───────────────────────────────────────────

/// pvxs `FmtDelta::top` (datafmt.cpp:100-121). All Delta lines sit at the
/// caller's `base_depth` (Delta does not nest indentation).
#[allow(clippy::too_many_arguments)]
fn delta_top(
    out: &mut String,
    prefix: &str,
    desc: &FieldDesc,
    value: Option<&PvField>,
    verytop: bool,
    fmt: &ValueFmt,
    marked: Option<&std::collections::HashSet<String>>,
    base_depth: usize,
) {
    if matches!(value, None | Some(PvField::Null)) {
        out.push_str(&pvxs_indent(base_depth));
        out.push_str(prefix);
        if !verytop {
            out.push(' ');
        }
        out.push_str("null\n");
        return;
    }

    delta_field(out, prefix, desc, value, verytop, fmt, marked, base_depth);

    // datafmt.cpp:112-120: for a struct, emit each marked descendant leaf as
    // its own flat dotted-path line (pvxs iterates `val.imarked()`).
    if let (FieldDesc::Structure { .. }, Some(PvField::Structure(_))) = (desc, value) {
        let mut leaves: Vec<(String, &FieldDesc)> = Vec::new();
        collect_leaves(desc, "", &mut leaves);
        for (path, ldesc) in &leaves {
            if !is_marked(marked, path) {
                continue;
            }
            let cprefix = if verytop {
                path.clone()
            } else {
                format!("{prefix}.{path}")
            };
            let lval = value.and_then(|v| value_at_path(v, path));
            delta_field(out, &cprefix, ldesc, lval, false, fmt, marked, base_depth);
        }
    }
}

/// pvxs `FmtDelta::field` (datafmt.cpp:17-98).
#[allow(clippy::too_many_arguments)]
fn delta_field(
    out: &mut String,
    prefix: &str,
    desc: &FieldDesc,
    value: Option<&PvField>,
    verytop: bool,
    fmt: &ValueFmt,
    marked: Option<&std::collections::HashSet<String>>,
    base_depth: usize,
) {
    // datafmt.cpp:19-20: at the very top, print nothing if nothing is marked.
    if verytop && !any_marked(marked, desc) {
        return;
    }
    out.push_str(&pvxs_indent(base_depth));
    out.push_str(prefix);
    if !verytop {
        out.push(' ');
    }
    out.push_str(&pvxs_type_name(desc));
    if let FieldDesc::Structure { struct_id, .. } = desc {
        if !struct_id.is_empty() {
            // Delta escapes the id (datafmt.cpp:27: `escape(val.id())`).
            let _ = write!(out, " \"{}\"", escape_pvxs(struct_id.as_bytes()));
        }
    }
    if fmt.show_value {
        delta_show_value(out, value, fmt);
    }
    out.push('\n');

    // datafmt.cpp:53-97: recurse into union/any (`->`) and arrays-of-value.
    //
    // `marked` holds ROOT-relative dotted paths and a union, an any and a
    // value-array are each a LEAF in it
    // (`client_native::ops_v2::changed_bitset_to_marked_paths`), so this
    // recursion has already crossed out of its key space: the paths built
    // below — `attribute[0].name`, `value->sub.x` — are not spellings that
    // set ever contains, and looking them up in it silently drops every
    // field of the element. pvxs does no lookup at all here: the nested
    // Value carries its OWN valid bits, and encoding a struct into a
    // StructureArray sets them for every non-struct descendant
    // (dataencode.cpp:460-471), so `top()` iterating `val.imarked()` sees
    // the whole element marked. `nested` is that state, and it is what
    // every one of these re-entries passes instead of the caller's set.
    let nested: Option<&std::collections::HashSet<String>> = None;
    match desc {
        FieldDesc::Union { variants, .. } => {
            let mut cprefix = String::from(prefix);
            cprefix.push_str("->");
            let (cdesc, cval) = match value {
                Some(PvField::Union {
                    selector,
                    variant_name,
                    value: inner,
                }) if *selector >= 0 => {
                    cprefix.push_str(variant_name);
                    let d = variants
                        .iter()
                        .find(|(n, _)| n == variant_name)
                        .map(|(_, d)| d.clone())
                        .unwrap_or_else(|| inner.descriptor());
                    (d, Some(inner.as_ref()))
                }
                _ => (FieldDesc::Variant, None),
            };
            delta_top(out, &cprefix, &cdesc, cval, false, fmt, nested, base_depth);
        }
        FieldDesc::Variant => {
            let mut cprefix = String::from(prefix);
            cprefix.push_str("->");
            match value {
                Some(PvField::Variant(v)) => {
                    let d = v.desc.clone().unwrap_or_else(|| v.value.descriptor());
                    delta_top(
                        out,
                        &cprefix,
                        &d,
                        Some(&v.value),
                        false,
                        fmt,
                        nested,
                        base_depth,
                    );
                }
                _ => delta_top(
                    out,
                    &cprefix,
                    &FieldDesc::Variant,
                    None,
                    false,
                    fmt,
                    nested,
                    base_depth,
                ),
            }
        }
        FieldDesc::StructureArray { .. }
        | FieldDesc::UnionArray { .. }
        | FieldDesc::VariantArray => {
            for (idx, (edesc, eval)) in array_elements(desc, value).into_iter().enumerate() {
                let p = format!("{prefix}[{idx}]");
                delta_top(
                    out,
                    &p,
                    &edesc,
                    eval.as_ref(),
                    false,
                    fmt,
                    nested,
                    base_depth,
                );
            }
        }
        _ => {}
    }
}

/// pvxs Delta inline value (datafmt.cpp:30-48). Scalar arrays render through
/// [`pvxs_array`]; struct / union / any / value-arrays print no inline value
/// (their contents come from the recursion).
fn delta_show_value(out: &mut String, value: Option<&PvField>, fmt: &ValueFmt) {
    let Some(v) = value else { return };
    match v {
        PvField::Scalar(sv) => match sv {
            ScalarValue::Float(f) => {
                let _ = write!(out, " = {}", format_g(*f as f64, 6));
            }
            ScalarValue::Double(f) => {
                let _ = write!(out, " = {}", format_g(*f, 6));
            }
            ScalarValue::Boolean(b) => {
                let _ = write!(out, " = {}", if *b { "true" } else { "false" });
            }
            ScalarValue::String(s) => {
                let _ = write!(out, " = \"{}\"", escape_pvxs(s.as_bytes()));
            }
            other => {
                let _ = write!(out, " = {other}");
            }
        },
        PvField::ScalarArray(items) => {
            let _ = write!(out, " = {}", pvxs_array(items, fmt.array_limit));
        }
        PvField::ScalarArrayTyped(arr) => {
            let _ = write!(
                out,
                " = {}",
                pvxs_array(&arr.to_scalar_values(), fmt.array_limit)
            );
        }
        _ => {}
    }
}

// ── shared desc/value navigation ────────────────────────────────────────────

/// Collect the dotted leaf paths of a descriptor in declaration order
/// (pvxs flattens marked descendants via `imarked`). A plain `Structure` is
/// a container — recurse into it; every other field (scalar, scalar-array,
/// union, any, array-of-compound) is a leaf.
fn collect_leaves<'a>(desc: &'a FieldDesc, prefix: &str, out: &mut Vec<(String, &'a FieldDesc)>) {
    if let FieldDesc::Structure { fields, .. } = desc {
        for (name, child) in fields {
            let p = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            match child {
                FieldDesc::Structure { .. } => collect_leaves(child, &p, out),
                _ => out.push((p, child)),
            }
        }
    }
}

/// Navigate a value by a dotted leaf path through nested structures.
fn value_at_path<'a>(value: &'a PvField, path: &str) -> Option<&'a PvField> {
    let mut cur = value;
    for seg in path.split('.') {
        match cur {
            PvField::Structure(s) => cur = s.get_field(seg)?,
            _ => return None,
        }
    }
    Some(cur)
}

/// Whether a dotted leaf path is marked. `None` = every leaf is marked
/// (a GET / a monitor's first update); `Some(set)` = only the listed paths.
fn is_marked(marked: Option<&std::collections::HashSet<String>>, path: &str) -> bool {
    match marked {
        None => true,
        Some(set) => set.contains(path),
    }
}

/// pvxs `Value::isMarked(false)` at the very top: is any descendant leaf
/// marked? `None` (all-marked) is true when the descriptor has any leaf.
fn any_marked(marked: Option<&std::collections::HashSet<String>>, desc: &FieldDesc) -> bool {
    let mut leaves = Vec::new();
    collect_leaves(desc, "", &mut leaves);
    match marked {
        None => !leaves.is_empty() || !is_compound(desc),
        Some(set) => leaves.iter().any(|(p, _)| set.contains(p)),
    }
}

/// Element `(descriptor, value)` pairs of an array-of-compound field, used by
/// both the Tree array branch and the Delta array recursion. A `None` value
/// is a pvxs `0x00` null (absent) element.
fn array_elements(desc: &FieldDesc, value: Option<&PvField>) -> Vec<(FieldDesc, Option<PvField>)> {
    match (desc, value) {
        (FieldDesc::StructureArray { struct_id, fields }, Some(PvField::StructureArray(items))) => {
            let ed = FieldDesc::Structure {
                struct_id: struct_id.clone(),
                fields: fields.clone(),
            };
            items
                .iter()
                .map(|it| {
                    (
                        ed.clone(),
                        it.as_ref().map(|s| PvField::Structure(s.clone())),
                    )
                })
                .collect()
        }
        (
            FieldDesc::UnionArray {
                struct_id,
                variants,
            },
            Some(PvField::UnionArray(items)),
        ) => {
            let ed = FieldDesc::Union {
                struct_id: struct_id.clone(),
                variants: variants.clone(),
            };
            items
                .iter()
                .map(|it| {
                    (
                        ed.clone(),
                        it.as_ref().map(|u| PvField::Union {
                            selector: u.selector,
                            variant_name: u.variant_name.clone(),
                            value: Box::new(u.value.clone()),
                        }),
                    )
                })
                .collect()
        }
        (FieldDesc::VariantArray, Some(PvField::VariantArray(items))) => items
            .iter()
            .map(|it| match it {
                Some(v) => (
                    v.desc.clone().unwrap_or_else(|| v.value.descriptor()),
                    Some(v.value.clone()),
                ),
                None => (FieldDesc::Variant, None),
            })
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pvdata::VariantValue;

    /// C `%g` rounds to `precision` significant digits BEFORE deciding
    /// between fixed and scientific notation (C99 7.19.6.1p8 takes the
    /// decision exponent X from the style-`e` conversion, which rounds).
    /// Reading `log10` off the raw value misses the carry into the next
    /// decade, so `pvget`/`pvmonitor`/`pvinfo` printed `1000000` where
    /// `caget`, `pvxs` and C `printf("%.6g")` all print `1e+06`.
    #[test]
    fn g_style_comes_from_the_rounded_exponent() {
        // Carry at the top: 999999.5 rounds to 1000000, exponent 5 -> 6,
        // which is no longer < 6, so the style flips to scientific.
        let big = PvField::Scalar(ScalarValue::Double(999999.5));
        assert_eq!(format_value_inline(&big), "1e+06");
        assert_eq!(nt_scalar_value_str(&big), "1e+06");
        assert_eq!(scalar_cell_text(&ScalarValue::Double(999999.5)), "1e+06");
        assert_eq!(
            format_value_inline(&PvField::Scalar(ScalarValue::Float(-999999.5))),
            "-1e+06"
        );

        // Carry at the bottom: 9.9999995e-05 rounds to 0.0001, exponent
        // -5 -> -4, which is no longer < -4, so the style flips to fixed.
        let small = PvField::Scalar(ScalarValue::Double(9.9999995e-05));
        assert_eq!(format_value_inline(&small), "0.0001");
        assert_eq!(
            scalar_cell_text(&ScalarValue::Double(9.9999995e-05)),
            "0.0001"
        );

        // Values that do not round across a decade are unaffected.
        assert_eq!(
            format_value_inline(&PvField::Scalar(ScalarValue::Double(999998.0))),
            "999998"
        );
        assert_eq!(
            format_value_inline(&PvField::Scalar(ScalarValue::Double(1234567.0))),
            "1.23457e+06"
        );
        assert_eq!(
            format_value_inline(&PvField::Scalar(ScalarValue::Double(0.000123456))),
            "0.000123456"
        );
    }

    /// An absent structure-array element prints `(none)`, the spelling
    /// EPICS Base `PVStructureArray::dumpValue` uses
    /// (PVStructureArray.cpp:230-239); `(null)` is the `PvField`
    /// diagnostic Display's word and had been copied into the output
    /// path. The present element goes through the same writer a named
    /// structure field uses, with an empty field name, so its header
    /// stays `id ` and its fields keep their own indent.
    #[test]
    fn raw_absent_structure_array_element_is_none() {
        let desc = FieldDesc::Structure {
            struct_id: "epics:nt/NTNDArray:1.0".into(),
            fields: vec![(
                "attribute".into(),
                FieldDesc::StructureArray {
                    struct_id: "epics:nt/NTAttribute:1.0".into(),
                    fields: vec![("name".into(), FieldDesc::Scalar(ScalarType::String))],
                },
            )],
        };
        let mut elem = PvStructure::new("epics:nt/NTAttribute:1.0");
        elem.set(
            "name",
            PvField::Scalar(ScalarValue::String("ColorMode".into())),
        );
        let mut top = PvStructure::new("epics:nt/NTNDArray:1.0");
        top.set("attribute", PvField::StructureArray(vec![Some(elem), None]));

        let out = format_raw("X", &desc, &PvField::Structure(top));
        assert!(
            out.contains("    epics:nt/NTAttribute:1.0[] attribute\n"),
            "got: {out}"
        );
        assert!(
            out.contains("        epics:nt/NTAttribute:1.0 \n            string name ColorMode\n"),
            "got: {out}"
        );
        assert!(out.contains("        (none)\n"), "got: {out}");
        assert!(!out.contains("(null)"), "got: {out}");
    }

    /// `-M raw` must render a union member through the raw writer, not
    /// through the `PvField` `Display`: that Display is a diagnostic that
    /// collapses a structure to its `value` subfield and drops every
    /// sibling (pvdata/structure.rs:440-452), so a union selecting a
    /// structure printed one number and lost the rest of the member.
    /// Base `PVUnion::dumpValue` (PVUnion.cpp:181-195) prints the union
    /// id and field name, then the selection through its own dump one
    /// level deeper, and `(none)` when no variant is selected.
    #[test]
    fn raw_union_member_prints_every_field_of_a_structure_selection() {
        let member = FieldDesc::Structure {
            struct_id: "epics:nt/NTAttribute:1.0".into(),
            fields: vec![
                ("value".into(), FieldDesc::Scalar(ScalarType::Double)),
                ("name".into(), FieldDesc::Scalar(ScalarType::String)),
            ],
        };
        let desc = FieldDesc::Structure {
            struct_id: "epics:nt/NTNDArray:1.0".into(),
            fields: vec![(
                "u".into(),
                FieldDesc::Union {
                    struct_id: String::new(),
                    variants: vec![("s".into(), member)],
                },
            )],
        };
        let mut inner = PvStructure::new("epics:nt/NTAttribute:1.0");
        inner.set("value", PvField::Scalar(ScalarValue::Double(1.5)));
        inner.set(
            "name",
            PvField::Scalar(ScalarValue::String("ColorMode".into())),
        );
        let mut top = PvStructure::new("epics:nt/NTNDArray:1.0");
        top.set(
            "u",
            PvField::Union {
                selector: 0,
                variant_name: "s".into(),
                value: Box::new(PvField::Structure(inner)),
            },
        );

        let out = format_raw("X", &desc, &PvField::Structure(top.clone()));
        assert!(out.contains("    union u\n"), "got: {out}");
        assert!(
            out.contains("        epics:nt/NTAttribute:1.0 s\n"),
            "got: {out}"
        );
        assert!(out.contains("            double value 1.5\n"), "got: {out}");
        assert!(
            out.contains("            string name ColorMode\n"),
            "got: {out}"
        );

        // No variant selected: `(none)`, not the Display's `null`.
        top.set(
            "u",
            PvField::Union {
                selector: -1,
                variant_name: String::new(),
                value: Box::new(PvField::Null),
            },
        );
        let out = format_raw("X", &desc, &PvField::Structure(top));
        assert!(out.contains("    union u\n        (none)\n"), "got: {out}");
    }

    /// The `any` half of the same delegation: a variant union stores its
    /// member with no field name, so the member line carries the type and
    /// the value with an empty name between them, and an empty `any` is
    /// `(none)`.
    #[test]
    fn raw_any_member_prints_through_the_raw_writer() {
        let desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![("p".into(), FieldDesc::Variant)],
        };
        let mut top = PvStructure::new("");
        top.set(
            "p",
            PvField::Variant(Box::new(VariantValue::scalar(ScalarValue::Int(5)))),
        );
        let out = format_raw("X", &desc, &PvField::Structure(top.clone()));
        assert!(out.contains("    any p\n        int  5\n"), "got: {out}");

        top.set("p", PvField::Variant(Box::new(VariantValue::null())));
        let out = format_raw("X", &desc, &PvField::Structure(top));
        assert!(out.contains("    any p\n        (none)\n"), "got: {out}");
    }

    /// pvxs#46 residue: the Tree inline branch (`any` / `any[]` /
    /// valued `union`) must emit the member name before the union
    /// selector and value (datafmt.cpp:224-230). Pre-fix `pvinfo-rs`
    /// printed `any` where pvxs prints `any parameters`.
    #[test]
    fn tree_inline_branch_keeps_the_member_name() {
        let desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![
                ("parameters".into(), FieldDesc::Variant),
                ("history".into(), FieldDesc::VariantArray),
                (
                    "u".into(),
                    FieldDesc::Union {
                        struct_id: String::new(),
                        variants: vec![
                            ("d".into(), FieldDesc::Scalar(ScalarType::Double)),
                            ("i".into(), FieldDesc::Scalar(ScalarType::Int)),
                        ],
                    },
                ),
            ],
        };

        // Describe mode (show_value=false, pvinfo): `any`/`any[]` are
        // inline; the union renders braced and is not asserted here.
        let describe = ValueFmt {
            format: ValueFormat::Tree,
            array_limit: 0,
            show_value: false,
        };
        let out = format_value(&desc, None, &describe, None, 0);
        assert!(out.contains("any parameters\n"), "got: {out:?}");
        assert!(out.contains("any[] history\n"), "got: {out:?}");

        // Value mode (show_value=true): `any` and the valued `union`
        // are inline; the union adds `.MEM` after the member name.
        let mut s = PvStructure::new("");
        s.set(
            "parameters",
            PvField::Variant(Box::new(VariantValue {
                desc: Some(FieldDesc::Scalar(ScalarType::Int)),
                value: PvField::Scalar(ScalarValue::Int(5)),
            })),
        );
        s.set("history", PvField::VariantArray(Vec::new()));
        s.set(
            "u",
            PvField::Union {
                selector: 1,
                variant_name: "i".into(),
                value: Box::new(PvField::Scalar(ScalarValue::Int(7))),
            },
        );
        let value_mode = ValueFmt {
            format: ValueFormat::Tree,
            array_limit: 0,
            show_value: true,
        };
        let out = format_value(
            &desc,
            Some(&PvField::Structure(s.clone())),
            &value_mode,
            None,
            0,
        );
        assert!(out.contains("any parameters int32_t = 5\n"), "got: {out:?}");
        assert!(out.contains("union u.i int32_t = 7\n"), "got: {out:?}");

        // Null-union boundary: member name, no selector, ` null`.
        s.set(
            "u",
            PvField::Union {
                selector: -1,
                variant_name: String::new(),
                value: Box::new(PvField::Null),
            },
        );
        let out = format_value(&desc, Some(&PvField::Structure(s)), &value_mode, None, 0);
        assert!(out.contains("union u null\n"), "got: {out:?}");
    }

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
        // EPICS Base `printTimeTx` (pvData printer.cpp:134-139) writes
        // `setw(24) << left << timeText << ' '` then `tag << ' '`, so for a
        // 23-char timestamp there are TWO spaces before the tag (one pad,
        // one explicit) and ONE space after it; pvxs/QSRV forces e.g.
        // UTAG 142 (testqsingle.cpp:280-283). The earlier embedded-in-
        // string form produced one space before and two after.
        let (desc, val) = nt_scalar_double(1.0, 1_700_000_000, 0);
        let PvField::Structure(mut s) = val else {
            panic!("nt scalar must be a structure");
        };
        if let Some(PvField::Structure(ts)) = s.get_field_mut("timeStamp") {
            ts.set("userTag", PvField::Scalar(ScalarValue::Int(142)));
        }
        let out = format_nt("MY:PV", &desc, &PvField::Structure(s));
        assert!(
            out.contains("  142 "),
            "userTag must follow two spaces and precede one, got: {out:?}"
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

    /// Exact `printTimeTx` block contract (pvData printer.cpp:116-140),
    /// asserted against the helper directly so the spacing is checked
    /// without a timezone-dependent date. One case per boundary:
    /// nonzero tag, zero tag, undefined timestamp + nonzero tag, and a
    /// missing `timeStamp`.
    #[test]
    fn time_tx_block_spacing_and_user_tag_boundaries() {
        let ts0 = |sec: i64, tag: i32| {
            let mut t = PvStructure::new("time_t");
            t.set("secondsPastEpoch", PvField::Scalar(ScalarValue::Long(sec)));
            t.set("nanoseconds", PvField::Scalar(ScalarValue::Int(0)));
            t.set("userTag", PvField::Scalar(ScalarValue::Int(tag)));
            t
        };

        // Undefined (sec == 0) + nonzero tag: the tag is STILL emitted
        // (Base reads userTag after the time text). Fully deterministic.
        assert_eq!(format_time_tx(Some(&ts0(0, 142))), "<undefined>  142 ");
        // Undefined + zero tag: sentinel, two spaces, no tag.
        assert_eq!(format_time_tx(Some(&ts0(0, 0))), "<undefined>  ");
        // No `timeStamp` substructure at all → undefined block.
        assert_eq!(format_time_tx(None), "<undefined>  ");

        // Valid timestamp: the date text is local-timezone dependent, so
        // assert the column/tag SHAPE and exact lengths (always 23-char
        // text for a 4-digit year). setw(24) pads to 24; the explicit
        // space makes two before the tag; the tag carries one space.
        let with_tag = format_time_tx(Some(&ts0(1_700_000_000, 142)));
        assert!(
            !with_tag.starts_with(' '),
            "time text must lead: {with_tag:?}"
        );
        assert!(
            with_tag.ends_with("  142 "),
            "tag token shape: {with_tag:?}"
        );
        assert_eq!(
            with_tag.len(),
            23 + 2 + 3 + 1,
            "23-char ts + 2sp + 142 + sp"
        );

        let no_tag = format_time_tx(Some(&ts0(1_700_000_000, 0)));
        assert!(no_tag.ends_with("  "), "two trailing spaces: {no_tag:?}");
        assert_eq!(no_tag.len(), 23 + 2, "23-char ts + 2sp");
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
    /// `-M json` numbers must read back: C `yajl_gen_double`
    /// (yajl_gen.c:222-247) writes `NaN` and a sign-bearing
    /// `+Infinity`/`-Infinity`, and otherwise `%.17g` with `.0` appended
    /// only when the text is nothing but digits and `-`. The port wrote
    /// Rust's `{}`, so a non-finite came out as `inf` — a token no JSON
    /// or JSON5 parser accepts — and a finite one came out in the
    /// shortest round-trip form, or fully expanded to hundreds of digits
    /// when `fract() == 0`.
    #[test]
    fn json_doubles_follow_yajl_gen_double() {
        let j = |v: f64| scalar_to_json(&ScalarValue::Double(v));
        assert_eq!(j(f64::INFINITY), "+Infinity");
        assert_eq!(j(f64::NEG_INFINITY), "-Infinity");
        assert_eq!(j(f64::NAN), "NaN");
        // %.17g, not the shortest round-trip form.
        assert_eq!(j(0.1), "0.10000000000000001");
        // `.0` only when the text is digits and `-` alone...
        assert_eq!(j(1.0), "1.0");
        assert_eq!(j(-2.0), "-2.0");
        assert_eq!(j(-0.0), "-0.0");
        // ...so an exponent form keeps its own shape, and a whole double
        // of that magnitude is no longer expanded to 31 digits.
        assert_eq!(j(1e30), "1e+30");
        // A float widens to double first, as the C generator's argument does.
        assert_eq!(scalar_to_json(&ScalarValue::Float(1.5)), "1.5");
    }

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
        let v = PvField::Scalar(ScalarValue::String(raw.into()));
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
            ScalarValue::String("a\nb".into()),
            ScalarValue::String("c\"d".into()),
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
                        .map(|l| ScalarValue::String((*l).into()))
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
        assert_eq!(csv_escape(b"plain"), "plain");
        assert_eq!(csv_escape(b"a,b"), "\"a,b\"");
        assert_eq!(csv_escape(b"a b"), "\"a b\"");
        assert_eq!(csv_escape(b"he\"llo"), "\"he\"\"llo\"");
        // tab is escaped to \t, but no quote/space/comma/backslash → no wrap.
        assert_eq!(csv_escape(b"tab\there"), "tab\\there");
        // a non-printable byte (0x01) → \x01.
        assert_eq!(csv_escape(b"\x01"), "\\x01");
        // a raw non-UTF-8 byte (0xFF) → \xFF (pvxs byte-wise, not U+FFFD).
        assert_eq!(csv_escape(b"\xff"), "\\xFF");
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

    /// `maybe_quote` mirrors Base `maybeQuote`/default `escape`: quote and
    /// escape on space / control / quote / backslash / apostrophe /
    /// non-printable; emit verbatim otherwise. A literal `"` becomes `\"`
    /// (default style), not the CSV `""`.
    #[test]
    fn maybe_quote_matches_base_rules() {
        assert_eq!(maybe_quote(b"plain"), "plain");
        assert_eq!(maybe_quote(b"a b"), "\"a b\"");
        assert_eq!(maybe_quote(b"a\tb"), "\"a\\tb\"");
        assert_eq!(maybe_quote(b"a\nb"), "\"a\\nb\"");
        assert_eq!(maybe_quote(b"a\"b"), "\"a\\\"b\"");
        assert_eq!(maybe_quote(b"a\\b"), "\"a\\\\b\"");
        assert_eq!(maybe_quote(b"a'b"), "\"a\\'b\"");
        assert_eq!(maybe_quote(b"\x01"), "\"\\x01\"");
        // a raw non-UTF-8 byte (0xFF) → quoted "\xFF"; Base `hexdigit` is
        // uppercase, so this path stays uppercase (see `escape_pvxs`).
        assert_eq!(maybe_quote(b"\xff"), "\"\\xFF\"");
    }

    /// Inline/raw display of a string scalar applies `maybeQuote`: a value
    /// with a space is quoted; a simple value stays verbatim
    /// (PVString::dumpValue, PVDataCreateFactory.cpp:145-149).
    #[test]
    fn raw_string_scalar_is_maybequoted() {
        assert_eq!(
            format_value_inline(&PvField::Scalar(ScalarValue::String("a b".into()))),
            "\"a b\""
        );
        assert_eq!(
            format_value_inline(&PvField::Scalar(ScalarValue::String("plain".into()))),
            "plain"
        );
    }

    /// A string array quotes each element via `maybeQuote` and joins with
    /// ", " (PVValueArray<std::string>::dumpValue, PVDataCreateFactory.cpp:
    /// 240-251).
    #[test]
    fn raw_string_array_quotes_each_element() {
        let v = PvField::ScalarArray(vec![
            ScalarValue::String("a b".into()),
            ScalarValue::String("c".into()),
        ]);
        assert_eq!(format_value_inline(&v), "[\"a b\", c]");
    }

    /// NTEnum choice text is `maybeQuote`-escaped (printEnumT streams the
    /// selected choice through maybeQuote, printer.cpp:168-175).
    #[test]
    fn nt_enum_choice_is_maybequoted() {
        let mut e = PvStructure::new("enum_t");
        e.set("index", PvField::Scalar(ScalarValue::Int(1)));
        e.set(
            "choices",
            PvField::ScalarArray(vec![
                ScalarValue::String("off".into()),
                ScalarValue::String("on hold".into()),
            ]),
        );
        assert_eq!(format_enum_summary(&e), "(1) \"on hold\"");
    }

    /// A wire-decoded `enum_t.choices` is a typed string array
    /// (`ScalarArrayTyped(String)`), not the untyped builder shape. The
    /// formatter must still render the selected choice — regression: the
    /// `ScalarArray`-only branch printed an empty choice for real
    /// pvxs/QSRV interop data.
    #[test]
    fn nt_enum_choice_from_typed_string_array() {
        let mut e = PvStructure::new("enum_t");
        e.set("index", PvField::Scalar(ScalarValue::Int(1)));
        e.set(
            "choices",
            PvField::ScalarArrayTyped(TypedScalarArray::String(
                ["OFF", "ON"].iter().map(|s| (*s).into()).collect(),
            )),
        );
        assert_eq!(format_enum_summary(&e), "(1) ON");
    }

    /// An out-of-range index renders `<undefined>`, matching EPICS Base
    /// `printEnumT` (pvData printer.cpp:171-172).
    #[test]
    fn nt_enum_out_of_range_index_is_undefined() {
        let mut e = PvStructure::new("enum_t");
        e.set("index", PvField::Scalar(ScalarValue::Int(5)));
        e.set(
            "choices",
            PvField::ScalarArray(vec![
                ScalarValue::String("off".into()),
                ScalarValue::String("on".into()),
            ]),
        );
        assert_eq!(format_enum_summary(&e), "(5) <undefined>");
    }

    /// EPICS Base `printEnumT` reads the index with `getAs<uint32>`
    /// (pvData printer.cpp:168-175), so a negative index reinterprets
    /// instead of clamping and lands out of range: -1 prints
    /// `(4294967295) <undefined>`. The port parsed the rendered text into
    /// `usize` and fell back to 0 on failure, so `pvget`/`pvmonitor`
    /// reported the FIRST choice for an NTEnum whose index is negative —
    /// naming a state the device is not in.
    #[test]
    fn nt_enum_negative_index_is_undefined_not_the_first_choice() {
        let mut e = PvStructure::new("enum_t");
        e.set("index", PvField::Scalar(ScalarValue::Int(-1)));
        e.set(
            "choices",
            PvField::ScalarArray(vec![
                ScalarValue::String("off".into()),
                ScalarValue::String("on".into()),
            ]),
        );
        assert_eq!(format_enum_summary(&e), "(4294967295) <undefined>");

        // The NT line is the second reader of the same rule.
        let mut nt = PvStructure::new("epics:nt/NTEnum:1.0");
        nt.set("value", PvField::Structure(e));
        assert!(
            format_nt_enum("X", &nt).contains("(4294967295) <undefined>"),
            "got: {:?}",
            format_nt_enum("X", &nt)
        );
    }

    /// A raw double scalar prints with six significant digits, matching
    /// Base's C++ stream default precision (PVDataCreateFactory.cpp:64-68),
    /// not Rust's shortest-round-trip Display.
    #[test]
    fn raw_double_scalar_six_significant_digits() {
        assert_eq!(
            format_value_inline(&PvField::Scalar(ScalarValue::Double(1.23456789))),
            "1.23457"
        );
    }

    /// Numeric (and bool) scalar arrays join elements with a bare comma,
    /// matching Base `PVValueArray<T>::dumpValue` (PVDataCreateFactory.cpp:
    /// 216-229) — not `, `.
    #[test]
    fn numeric_array_uses_bare_comma_separator() {
        let ints = PvField::ScalarArray(vec![
            ScalarValue::Int(1),
            ScalarValue::Int(2),
            ScalarValue::Int(3),
        ]);
        assert_eq!(format_value_inline(&ints), "[1,2,3]");
        let dbls = PvField::ScalarArray(vec![ScalarValue::Double(1.5), ScalarValue::Double(2.25)]);
        assert_eq!(format_value_inline(&dbls), "[1.5,2.25]");
    }

    /// An NTTable
    /// double column renders through Base `getAs<std::string>` — each cell
    /// passes `castUnsafe<std::string, double>` → `std::ostringstream <<
    /// value` at the C++ default precision of six significant digits
    /// (printer.cpp:252, typeCast.h:101-110) — NOT Rust's shortest-round-
    /// trip full precision. So `1.23456789` prints as `"1.23457"`, matching
    /// `pvget`'s table output character-for-character. The earlier
    /// "keeps full precision" guard asserted the divergent behaviour.
    #[test]
    fn nt_table_double_cell_uses_six_sig_digits() {
        let (desc, value) = nt_table(&["d"], &[("d", vec![ScalarValue::Double(1.23456789)])]);
        let out = format_nt("PV", &desc, &value);
        assert_eq!(out, "PV \n      d\n1.23457\n");
    }

    /// Companion to [`nt_table_double_cell_uses_six_sig_digits`]: the
    /// six-significant-digit cell, not the discarded full-precision text,
    /// drives the column width. With a one-char label `"v"` and a
    /// `1.23456789` cell, the column is 7 wide (`"1.23457"`), so the header
    /// right-justifies `"v"` with six leading spaces — full precision (ten
    /// chars) would have produced nine. A short second row (`2.5`) confirms
    /// every data row aligns to the same shortened width.
    #[test]
    fn nt_table_double_column_width_tracks_six_sig_digits() {
        let (desc, value) = nt_table(
            &["v"],
            &[(
                "v",
                vec![ScalarValue::Double(1.23456789), ScalarValue::Double(2.5)],
            )],
        );
        let out = format_nt("PV", &desc, &value);
        assert_eq!(out, "PV \n      v\n1.23457\n    2.5\n");
    }

    /// Build an NTScalarArray top structure carrying the given scalar-array
    /// `value`, an epoch-0 (`<undefined>`) timestamp, and optionally an
    /// alarm. epoch-0 keeps the timestamp text deterministic across
    /// machine timezones.
    fn nt_scalar_array(value: PvField, alarm: Option<PvField>) -> (FieldDesc, PvField) {
        let desc = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalarArray:1.0".into(),
            fields: vec![],
        };
        let mut s = PvStructure::new("epics:nt/NTScalarArray:1.0");
        s.set("value", value);
        let mut ts = PvStructure::new("time_t");
        ts.set("secondsPastEpoch", PvField::Scalar(ScalarValue::Long(0)));
        ts.set("nanoseconds", PvField::Scalar(ScalarValue::Int(0)));
        ts.set("userTag", PvField::Scalar(ScalarValue::Int(0)));
        s.set("timeStamp", PvField::Structure(ts));
        if let Some(a) = alarm {
            s.set("alarm", a);
        }
        (desc, PvField::Structure(s))
    }

    /// PVA-150: an NTScalarArray value renders on ONE line through the
    /// Base-style `.value` NT branch (printer.cpp:436-441), not the
    /// multi-line raw fallback. Numeric elements print with six
    /// significant digits and a bare-comma separator.
    #[test]
    fn nt_scalar_array_double_renders_one_line() {
        let (desc, val) = nt_scalar_array(
            PvField::ScalarArray(vec![ScalarValue::Double(1.5), ScalarValue::Double(2.5)]),
            None,
        );
        let out = format_nt("PV", &desc, &val);
        assert_eq!(out, "PV <undefined>  [1.5,2.5]\n");
        // Single line: the only newline is the terminator.
        assert_eq!(out.matches('\n').count(), 1);
    }

    /// A string-valued NTScalarArray quotes each element via `maybeQuote`
    /// and joins with `", "` (PVValueArray<std::string>::dumpValue), still
    /// on one NT line.
    #[test]
    fn nt_scalar_array_string_quotes_each_element() {
        let (desc, val) = nt_scalar_array(
            PvField::ScalarArray(vec![
                ScalarValue::String("a b".into()),
                ScalarValue::String("c".into()),
            ]),
            None,
        );
        let out = format_nt("PV", &desc, &val);
        assert_eq!(out, "PV <undefined>  [\"a b\", c]\n");
    }

    /// For a scalar ARRAY, Base prints the alarm BEFORE the value
    /// (printer.cpp:436-441: printTimeT, printAlarmT, then the array) —
    /// the opposite of the NTScalar order. Verify the alarm summary
    /// precedes the array text, and severity 0 prints no alarm.
    #[test]
    fn nt_scalar_array_alarm_precedes_value() {
        let (desc, val) = nt_scalar_array(
            PvField::ScalarArray(vec![ScalarValue::Int(7), ScalarValue::Int(8)]),
            Some(alarm_struct(2, 3, "HIGH")),
        );
        let out = format_nt("PV", &desc, &val);
        assert_eq!(out, "PV <undefined>  MAJOR RECORD HIGH [7,8]\n");
        let ai = out.find("MAJOR").expect("alarm present");
        let vi = out.find("[7,8]").expect("value present");
        assert!(ai < vi, "alarm must precede the array value: {out:?}");
    }

    /// severity 0 → no alarm tokens on the NTScalarArray line.
    #[test]
    fn nt_scalar_array_omits_zero_severity_alarm() {
        let (desc, val) = nt_scalar_array(
            PvField::ScalarArray(vec![ScalarValue::Int(1)]),
            Some(alarm_struct(0, 0, "ignored")),
        );
        let out = format_nt("PV", &desc, &val);
        assert_eq!(out, "PV <undefined>  [1]\n");
    }

    /// Structural check: the `.value` dispatch is keyed on the value TYPE,
    /// not the NT struct ID — so a structure with an empty struct ID but a
    /// scalar-array `.value` ("anything with '.value'", printer.cpp:423)
    /// also gets the one-line NT output instead of the raw fallback.
    #[test]
    fn nt_generic_value_scalar_array_uses_nt_branch() {
        let desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![],
        };
        let mut s = PvStructure::new("");
        s.set(
            "value",
            PvField::ScalarArray(vec![ScalarValue::Int(1), ScalarValue::Int(2)]),
        );
        let out = format_nt("PV", &desc, &PvField::Structure(s));
        assert_eq!(out, "PV <undefined>  [1,2]\n");
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

    // ── pvxs Value::format() Tree / Delta golden output ─────────────────────

    fn tree(desc: &FieldDesc, value: Option<&PvField>, show_value: bool) -> String {
        format_value(
            desc,
            value,
            &ValueFmt {
                format: ValueFormat::Tree,
                array_limit: 0,
                show_value,
            },
            None,
            0,
        )
    }

    /// pvget `-F tree` of an NTScalar: nested `{ }` blocks, pvxs `TypeCode`
    /// names (`int64_t`, not `long`), four-space indent per level, and
    /// `member = value` leaves (datafmt.cpp FmtTree::show).
    #[test]
    fn pvxs_tree_nt_scalar_golden() {
        let (desc, val) = nt_scalar_double(42.5, 0, 0);
        let out = tree(&desc, Some(&val), true);
        assert_eq!(
            out,
            "struct \"epics:nt/NTScalar:1.0\" {\n\
             \x20   double value = 42.5\n\
             \x20   struct \"time_t\" {\n\
             \x20       int64_t secondsPastEpoch = 0\n\
             \x20       int32_t nanoseconds = 0\n\
             \x20       int32_t userTag = 0\n\
             \x20   } timeStamp\n\
             }\n"
        );
    }

    /// pvinfo's `Value::format().showValue(false)` (info.cpp:92-94): the same
    /// Tree structure with no `= value` on the leaves.
    #[test]
    fn pvxs_tree_show_value_false_is_type_only() {
        let (desc, _val) = nt_scalar_double(42.5, 0, 0);
        let out = tree(&desc, None, false);
        assert_eq!(
            out,
            "struct \"epics:nt/NTScalar:1.0\" {\n\
             \x20   double value\n\
             \x20   struct \"time_t\" {\n\
             \x20       int64_t secondsPastEpoch\n\
             \x20       int32_t nanoseconds\n\
             \x20       int32_t userTag\n\
             \x20   } timeStamp\n\
             }\n"
        );
    }

    /// pvxs escapes through `Escaper` (src/util.cpp:230-235), whose
    /// `std::hex` carries no `std::uppercase`, so every printer on the
    /// `datafmt.cpp` path emits lowercase `\xhh`. Base's `hexdigit`
    /// (pvData printer.cpp:467-473) is uppercase, and that is what
    /// `maybeQuote` uses. One escape helper served both references, so
    /// `-F tree` and `-F delta` printed Base's case where pvxget prints
    /// pvxs's.
    #[test]
    fn datafmt_hex_escapes_are_lowercase_like_pvxs() {
        let desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::String))],
        };
        let mut s = PvStructure::new("");
        s.set(
            "value",
            PvField::Scalar(ScalarValue::String("\u{e9}\u{19}".into())),
        );
        let val = PvField::Structure(s);

        let t = tree(&desc, Some(&val), true);
        assert!(t.contains("string value = \"\\xc3\\xa9\\x19\""), "{t}");

        let d = format_value(
            &desc,
            Some(&val),
            &ValueFmt {
                format: ValueFormat::Delta,
                array_limit: 0,
                show_value: true,
            },
            None,
            0,
        );
        assert!(d.contains("= \"\\xc3\\xa9\\x19\""), "{d}");

        // The Base-referenced path keeps Base's uppercase hex.
        assert_eq!(
            maybe_quote("\u{e9}\u{19}".as_bytes()),
            "\"\\xC3\\xA9\\x19\""
        );
    }

    /// pvget `-F delta`: flat dotted-path lines, `path type = value`, with
    /// every leaf shown when nothing restricts the marked set (a GET marks
    /// every field it returns). Mirrors datafmt.cpp FmtDelta.
    #[test]
    fn pvxs_delta_nt_scalar_all_marked_golden() {
        let (desc, val) = nt_scalar_double(42.5, 0, 0);
        let out = format_value(
            &desc,
            Some(&val),
            &ValueFmt {
                format: ValueFormat::Delta,
                array_limit: 0,
                show_value: true,
            },
            None,
            0,
        );
        assert_eq!(
            out,
            "struct \"epics:nt/NTScalar:1.0\"\n\
             value double = 42.5\n\
             timeStamp.secondsPastEpoch int64_t = 0\n\
             timeStamp.nanoseconds int32_t = 0\n\
             timeStamp.userTag int32_t = 0\n"
        );
    }

    /// A monitor's changed set holds ROOT-relative paths and a `structure[]`
    /// is ONE leaf in it (`changed_bitset_to_marked_paths`), so the
    /// recursion into an element must stop consulting it. pvxs prints the
    /// element's fields because the element Value carries its own valid
    /// bits, set for every non-struct descendant when a struct is encoded
    /// into a StructureArray (dataencode.cpp:460-471). The port looked up
    /// `name` / `value` in a set holding only `attribute`, every lookup
    /// missed, and the element printed as an empty header line.
    #[test]
    fn pvxs_delta_marked_structure_array_prints_its_element_fields() {
        let elem = vec![
            ("name".to_string(), FieldDesc::Scalar(ScalarType::String)),
            ("value".to_string(), FieldDesc::Scalar(ScalarType::Double)),
        ];
        let desc = FieldDesc::Structure {
            struct_id: "epics:nt/NTNDArray:1.0".into(),
            fields: vec![
                ("value".into(), FieldDesc::Scalar(ScalarType::Double)),
                (
                    "attribute".into(),
                    FieldDesc::StructureArray {
                        struct_id: "epics:nt/NTAttribute:1.0".into(),
                        fields: elem,
                    },
                ),
            ],
        };
        let mut attr = PvStructure::new("epics:nt/NTAttribute:1.0");
        attr.fields.push((
            "name".into(),
            PvField::Scalar(ScalarValue::String("ColorMode".into())),
        ));
        attr.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.5))));
        let mut root = PvStructure::new("epics:nt/NTNDArray:1.0");
        root.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(7.0))));
        root.fields.push((
            "attribute".into(),
            PvField::StructureArray(vec![Some(attr)]),
        ));

        let mut marked = std::collections::HashSet::new();
        marked.insert("attribute".to_string());
        let out = format_value(
            &desc,
            Some(&PvField::Structure(root)),
            &ValueFmt {
                format: ValueFormat::Delta,
                array_limit: 0,
                show_value: true,
            },
            Some(&marked),
            0,
        );
        assert_eq!(
            out,
            "struct \"epics:nt/NTNDArray:1.0\"\n\
             attribute struct[]\n\
             attribute[0] struct \"epics:nt/NTAttribute:1.0\"\n\
             attribute[0].name string = \"ColorMode\"\n\
             attribute[0].value double = 1.5\n"
        );
    }

    /// A restricted marked set (pvxs `imarked()` for a monitor's changed
    /// fields) prints only those leaves under the top struct line. This is
    /// the wiring point for the monitor changed-set; the formatter already
    /// honours it.
    #[test]
    fn pvxs_delta_marked_subset_shows_only_marked() {
        let (desc, val) = nt_scalar_double(42.5, 0, 0);
        let mut marked = std::collections::HashSet::new();
        marked.insert("value".to_string());
        let out = format_value(
            &desc,
            Some(&val),
            &ValueFmt {
                format: ValueFormat::Delta,
                array_limit: 0,
                show_value: true,
            },
            Some(&marked),
            0,
        );
        assert_eq!(
            out,
            "struct \"epics:nt/NTScalar:1.0\"\nvalue double = 42.5\n"
        );
    }

    /// The `Indented I(std::cout)` wrapper pvget/pvmonitor apply
    /// (get.cpp:112-113) is the `base_depth=1` argument: every line gains
    /// one four-space level.
    #[test]
    fn pvxs_delta_base_depth_indents_every_line() {
        let (desc, val) = nt_scalar_double(1.0, 0, 0);
        let out = format_value(
            &desc,
            Some(&val),
            &ValueFmt {
                format: ValueFormat::Delta,
                array_limit: 0,
                show_value: true,
            },
            None,
            1,
        );
        for line in out.lines() {
            assert!(
                line.starts_with("    "),
                "base_depth=1 must indent every delta line, got: {line:?}"
            );
        }
    }

    /// pvxs array dump (`shared_array::format()`, sharedarray.cpp showArr):
    /// `{count}[e, e, ...]`, `", "` separator for every type, `int8`/`uint8`
    /// as numbers, `bool` as `1`/`0`, strings quoted+escaped, and `...` once
    /// the `-#` limit is reached (limit 0 = unlimited).
    #[test]
    fn pvxs_array_format_matches_showarr() {
        let ints = [
            ScalarValue::Int(1),
            ScalarValue::Int(2),
            ScalarValue::Int(3),
        ];
        assert_eq!(pvxs_array(&ints, 0), "{3}[1, 2, 3]");
        assert_eq!(pvxs_array(&ints, 2), "{3}[1, 2, ...]");
        let bytes = [ScalarValue::Byte(-3), ScalarValue::UByte(200)];
        assert_eq!(pvxs_array(&bytes, 0), "{2}[-3, 200]");
        let bools = [ScalarValue::Boolean(true), ScalarValue::Boolean(false)];
        assert_eq!(pvxs_array(&bools, 0), "{2}[1, 0]");
        let strs = [
            ScalarValue::String("a b".into()),
            ScalarValue::String("c".into()),
        ];
        assert_eq!(pvxs_array(&strs, 0), "{2}[\"a b\", \"c\"]");
    }

    /// A scalar-array leaf renders through the pvxs array dump in both Tree
    /// and Delta, and `-#` truncates it.
    #[test]
    fn pvxs_tree_scalar_array_uses_limit() {
        let desc = FieldDesc::Structure {
            struct_id: "x".into(),
            fields: vec![("value".into(), FieldDesc::ScalarArray(ScalarType::Int))],
        };
        let mut s = PvStructure::new("x");
        s.set(
            "value",
            PvField::ScalarArray(vec![
                ScalarValue::Int(10),
                ScalarValue::Int(20),
                ScalarValue::Int(30),
            ]),
        );
        let val = PvField::Structure(s);
        let out = format_value(
            &desc,
            Some(&val),
            &ValueFmt {
                format: ValueFormat::Tree,
                array_limit: 2,
                show_value: true,
            },
            None,
            0,
        );
        assert_eq!(
            out,
            "struct \"x\" {\n    int32_t[] value = {3}[10, 20, ...]\n}\n"
        );
    }

    /// pvinfo's full per-PV block (info.cpp:90-94): the `<pv> from <peer>`
    /// header line, then the values-hidden type tree at `base_depth=0`. Locks
    /// the pvxs-compatible shape that replaced the Rust `<pv>`/`Server:`/
    /// `Type:` labels.
    #[test]
    fn pvxs_pvinfo_block_golden() {
        let (desc, _val) = nt_scalar_double(0.0, 0, 0);
        let peer: std::net::SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let out = format_info_value("TST:ai", peer, &desc);
        assert_eq!(
            out,
            "TST:ai from 127.0.0.1:5075\n\
             struct \"epics:nt/NTScalar:1.0\" {\n\
             \x20   double value\n\
             \x20   struct \"time_t\" {\n\
             \x20       int64_t secondsPastEpoch\n\
             \x20       int32_t nanoseconds\n\
             \x20       int32_t userTag\n\
             \x20   } timeStamp\n\
             }\n"
        );
    }

    /// `parse_value_format` is the `-F` entry point shared by pvget/pvmonitor
    /// (and the future pvinfo wiring). It must: map `tree`/`delta` directly,
    /// treat an absent flag (`None`) as "keep the `-M` mode" by returning
    /// `None`, and — like pvxs — fall back to `Delta` (with a stderr warning)
    /// on any unknown value rather than erroring out.
    #[test]
    fn parse_value_format_maps_known_and_falls_back() {
        assert_eq!(parse_value_format(Some("tree")), Some(ValueFormat::Tree));
        assert_eq!(parse_value_format(Some("delta")), Some(ValueFormat::Delta));
        // Unknown -> Delta (pvxs "Warning: ignoring unknown format").
        assert_eq!(parse_value_format(Some("bogus")), Some(ValueFormat::Delta));
        assert_eq!(parse_value_format(Some("")), Some(ValueFormat::Delta));
        // Flag absent -> None, so the caller keeps its `-M` mode.
        assert_eq!(parse_value_format(None), None);
    }
}
