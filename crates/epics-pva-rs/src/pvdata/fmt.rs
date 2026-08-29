//! pvxs `SB() << Value` — the single owner of rendering a [`PvField`] as
//! human-readable diagnostic text.
//!
//! pvxs streams a `Value` through `operator<<(std::ostream&, const Value&)`
//! (`src/pvxs/data.h:940-943`), which is `strm << val.format()` with the
//! DEFAULT `Fmt`: `_format = Tree`, `_showValue = true`, `_limit = 0`
//! (`data.h:787-813`). `FmtTree::show()` (`src/datafmt.cpp:124-311`) then
//! writes
//!
//! ```text
//! <typecode>[ "<id>"][ <member>] = <value>\n
//! ```
//!
//! The trailing newline is written on every path and IS part of the text pvxs
//! puts on the wire in a `logRemote()` diagnostic, so it is part of the
//! contract here.
//!
//! Two call sites in this workspace had each grown their own approximation of
//! this — the native PVA server's monitor-option diagnostics and the QSRV
//! bridge's `record._options.process` diagnostic — and they disagreed with
//! each other AND with pvxs (both invented the pvData type spellings `int32` /
//! `float64`, where `TypeCode::name()` is C-ish: `int32_t`, `double` —
//! `src/type.cpp:126-166`). This module is the one owner both now call.
//!
//! Scope: this renders a VALUE. pvxs's formatter reads the type from the
//! `Value`'s descriptor, so where a [`PvField`] variant does not carry its own
//! type the rendering cannot recover it — see [`render_value`] for the single
//! such case (an empty untyped `ScalarArray`, which the decoder never
//! produces).

use super::scalar::{ScalarType, ScalarValue};
use super::structure::{PvField, PvStructure, UnionItem, VariantValue};
use super::typed_array::TypedScalarArray;

/// pvxs `detail::Escaper` (`src/util.cpp:210-243`) — the escaping
/// `FmtTree::show_value` applies inside the quotes of a string value.
///
/// C escapes for `\a \b \f \n \r \t \v \\ \' \"`, printable ASCII
/// (`' '..='~'`) verbatim, everything else as `\xNN` over the raw BYTES. Rust
/// strings are UTF-8, so a non-ASCII character emits one `\xNN` per UTF-8 byte
/// — which is exactly what pvxs does with the `char`s of a `std::string`
/// holding the same UTF-8.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
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
            b' '..=b'~' => out.push(b as char),
            other => out.push_str(&format!("\\x{other:02x}")),
        }
    }
    out
}

/// C `printf("%.*g", precision, v)` — the crate's single owner of
/// "double → text".
///
/// pvxs streams every real through `operator<<(std::ostream&, double)` with
/// the default stream state, `defaultfloat` + `precision(6)`
/// (`datafmt.cpp:148-151`); pvData's CLI printer sets the same six digits
/// explicitly (`printer.cpp:431,440`); the JSON generator asks for seventeen
/// (`yajl_gen.c:236`). All three are this one conversion, and splitting it
/// across modules is how the CLI formatter came to print `NaN` where every
/// other caller printed `nan`.
///
/// `%g` with precision `P`: round to `P` significant digits; let `X` be the
/// resulting decimal exponent. Use `%e` when `X < -4 || X >= P`, else `%f`
/// with `P-1-X` decimals. Either way strip trailing zeros and any trailing
/// `.`. The `%e` exponent carries a sign and at least two digits.
///
/// Rust's `{}` is not `%g`: it prints the shortest round-tripping form
/// (`0.1f64` → `0.1`, but `1e6` → `1000000` and `1.0/3.0` →
/// `0.3333333333333333`) and spells a NaN `NaN`.
pub(crate) fn format_g(v: f64, precision: usize) -> String {
    // glibc prints the same three words for %g, %f and %e, and libstdc++
    // carries the sign bit of a NaN through to the text.
    if v.is_nan() {
        return if v.is_sign_negative() { "-nan" } else { "nan" }.to_string();
    }
    if v.is_infinite() {
        return if v < 0.0 { "-inf" } else { "inf" }.to_string();
    }
    // `{:.*e}` rounds the mantissa to `precision` significant digits AND
    // renormalizes the exponent (`9_999_999.0` → `1.00000e7`), so the
    // exponent read back here is the one `%g` branches on.
    let sci = format!("{:.*e}", precision - 1, v);
    let (mantissa, exponent) = sci
        .split_once('e')
        .expect("LowerExp always emits an 'e' separator");
    let exp: i32 = exponent
        .parse()
        .expect("LowerExp always emits a decimal exponent");
    if exp < -4 || exp >= precision as i32 {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", strip_trailing_zeros(mantissa), exp.abs())
    } else {
        let decimals = (precision as i32 - 1 - exp).max(0) as usize;
        strip_trailing_zeros(&format!("{v:.decimals$}"))
    }
}

/// pvxs's precision for a real (`datafmt.cpp:148-151`): the C++ stream
/// default of six significant digits.
fn render_real(v: f64) -> String {
    format_g(v, 6)
}

/// `%g`'s trailing-zero removal: only meaningful for a fractional form, and
/// the `.` goes with the last zero.
fn strip_trailing_zeros(s: &str) -> String {
    if !s.contains('.') {
        return s.to_string();
    }
    let trimmed = s.trim_end_matches('0');
    trimmed.strip_suffix('.').unwrap_or(trimmed).to_string()
}

/// pvxs `TypeCode::name()` (`src/type.cpp:126-166`). Deliberately the C-ish
/// spellings pvxs prints (`int32_t`, `uint8_t`, `float`, `double`), NOT the
/// pvData wire names.
fn scalar_type_name(st: ScalarType) -> &'static str {
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

/// pvxs `FmtTree::show_value()` for a SCALAR (`datafmt.cpp:128-154`): bool as
/// `true`/`false`, signed integers through `int64_t`, unsigned through
/// `uint64_t`, reals through `double`, strings quoted and escaped.
fn render_scalar(v: &ScalarValue) -> String {
    match v {
        ScalarValue::Boolean(b) => if *b { "true" } else { "false" }.to_string(),
        ScalarValue::Byte(v) => v.to_string(),
        ScalarValue::Short(v) => v.to_string(),
        ScalarValue::Int(v) => v.to_string(),
        ScalarValue::Long(v) => v.to_string(),
        ScalarValue::UByte(v) => v.to_string(),
        ScalarValue::UShort(v) => v.to_string(),
        ScalarValue::UInt(v) => v.to_string(),
        ScalarValue::ULong(v) => v.to_string(),
        ScalarValue::Float(v) => render_real(f64::from(*v)),
        ScalarValue::Double(v) => render_real(*v),
        ScalarValue::String(s) => format!("\"{}\"", escape(&s.as_str_lossy())),
    }
}

/// pvxs `showArr()` (`src/sharedarray.cpp:104-123`) — an ARRAY element, which
/// is NOT rendered the same way as the scalar of that type: `bool` prints
/// through the default `operator<<(bool)` as `1`/`0` (no `boolalpha`), and
/// `int8_t`/`uint8_t` are promoted to `int`/`unsigned` so they print as
/// numbers rather than characters. Reals and strings match the scalar form.
fn render_array_element(v: &ScalarValue) -> String {
    match v {
        ScalarValue::Boolean(b) => if *b { "1" } else { "0" }.to_string(),
        other => render_scalar(other),
    }
}

/// pvxs `showArr()`'s array body: `{<count>}[e0, e1, …]`.
fn render_array_body(elements: impl ExactSizeIterator<Item = ScalarValue>) -> String {
    let count = elements.len();
    let mut out = format!("{{{count}}}[");
    for (i, e) in elements.enumerate() {
        if i != 0 {
            out.push_str(", ");
        }
        out.push_str(&render_array_element(&e));
    }
    out.push(']');
    out
}

/// The elements of a [`TypedScalarArray`] as [`ScalarValue`]s.
fn typed_elements(arr: &TypedScalarArray) -> Vec<ScalarValue> {
    match arr {
        TypedScalarArray::Boolean(a) => a.iter().map(|&v| ScalarValue::Boolean(v)).collect(),
        TypedScalarArray::Byte(a) => a.iter().map(|&v| ScalarValue::Byte(v)).collect(),
        TypedScalarArray::UByte(a) => a.iter().map(|&v| ScalarValue::UByte(v)).collect(),
        TypedScalarArray::Short(a) => a.iter().map(|&v| ScalarValue::Short(v)).collect(),
        TypedScalarArray::UShort(a) => a.iter().map(|&v| ScalarValue::UShort(v)).collect(),
        TypedScalarArray::Int(a) => a.iter().map(|&v| ScalarValue::Int(v)).collect(),
        TypedScalarArray::UInt(a) => a.iter().map(|&v| ScalarValue::UInt(v)).collect(),
        TypedScalarArray::Long(a) => a.iter().map(|&v| ScalarValue::Long(v)).collect(),
        TypedScalarArray::ULong(a) => a.iter().map(|&v| ScalarValue::ULong(v)).collect(),
        TypedScalarArray::Float(a) => a.iter().map(|&v| ScalarValue::Float(v)).collect(),
        TypedScalarArray::Double(a) => a.iter().map(|&v| ScalarValue::Double(v)).collect(),
        TypedScalarArray::String(a) => a.iter().cloned().map(ScalarValue::String).collect(),
    }
}

/// Render a [`PvField`] exactly as pvxs's `SB() << Value` does.
///
/// The output always ends in `\n` — `FmtTree::show()` terminates every branch
/// with one, and pvxs sends that byte to the client inside a `logRemote()`
/// message, so callers building such a message must not add or trim it.
///
/// One shape cannot be rendered faithfully from a value alone: an EMPTY
/// [`PvField::ScalarArray`] (the untyped array variant) carries no element
/// type, and pvxs reads that from the descriptor. It renders as pvxs's
/// unallocated-array form `null = {?}[]`. This is unreachable from the wire:
/// `decode_typed_scalar_array` covers every [`ScalarType`], so a decoded array
/// is always a [`PvField::ScalarArrayTyped`], which carries its element type
/// even at length 0.
pub fn render_value(f: &PvField) -> String {
    let mut out = String::new();
    show(f, "", 0, &mut out);
    out
}

/// pvxs `FmtTree::show()` (`datafmt.cpp:187-311`). `member` is the field name
/// this value is bound to under its parent (empty at the top), `level` the
/// indent depth (pvxs `indent{}` = four spaces per level, `src/util.cpp:128-139`).
fn show(f: &PvField, member: &str, level: usize, out: &mut String) {
    // `if(!fld) { strm<<"null\n"; return; }` — an empty Value.
    if matches!(f, PvField::Null) {
        out.push_str("null\n");
        return;
    }
    match f {
        PvField::Null => unreachable!("handled above"),
        // Non-compound: `<type>[ <member>] = <value>\n`.
        PvField::Scalar(v) => {
            out.push_str(scalar_type_name(v.scalar_type()));
            push_member(out, member);
            out.push_str(" = ");
            out.push_str(&render_scalar(v));
            out.push('\n');
        }
        PvField::ScalarArrayTyped(arr) => {
            out.push_str(scalar_type_name(arr.scalar_type()));
            out.push_str("[]");
            push_member(out, member);
            out.push_str(" = ");
            out.push_str(&render_array_body(typed_elements(arr).into_iter()));
            out.push('\n');
        }
        PvField::ScalarArray(items) => {
            match items.first() {
                Some(first) => {
                    out.push_str(scalar_type_name(first.scalar_type()));
                    out.push_str("[]");
                }
                // No element, hence no type — see `render_value`.
                None => out.push_str("null"),
            }
            push_member(out, member);
            out.push_str(" = ");
            out.push_str(&render_array_body(items.iter().cloned()));
            out.push('\n');
        }
        // `any NAME <inner>` / `union NAME.MEM <inner>` (`datafmt.cpp:212-238`):
        // the selected value is shown by recursion, so an unselected union
        // renders `union null\n`.
        PvField::Union {
            selector,
            variant_name,
            value,
        } => {
            out.push_str("union");
            push_member(out, member);
            if *selector >= 0 && !matches!(**value, PvField::Null) {
                out.push('.');
                out.push_str(variant_name);
            }
            out.push(' ');
            show(value, "", level, out);
        }
        PvField::Variant(v) => {
            out.push_str("any");
            push_member(out, member);
            out.push(' ');
            show(&v.value, "", level, out);
        }
        // `struct "id" { … } NAME` (`datafmt.cpp:239-273`).
        PvField::Structure(s) => show_structure(s, member, level, out),
        // `<type>[] NAME = {N}[ … ]` (`datafmt.cpp:274-311`).
        PvField::StructureArray(items) => {
            out.push_str("struct[]");
            push_member(out, member);
            show_compound_array(items.len(), level, out, |i, level, out| {
                match &items[i] {
                    Some(s) => show_structure(s, "", level, out),
                    // A null element — pvxs `show()`'s `if(!fld)`.
                    None => out.push_str("null\n"),
                }
            });
        }
        PvField::UnionArray(items) => {
            out.push_str("union[]");
            push_member(out, member);
            show_compound_array(items.len(), level, out, |i, level, out| match &items[i] {
                Some(UnionItem {
                    selector,
                    variant_name,
                    value,
                }) => show(
                    &PvField::Union {
                        selector: *selector,
                        variant_name: variant_name.clone(),
                        value: Box::new(value.clone()),
                    },
                    "",
                    level,
                    out,
                ),
                None => out.push_str("null\n"),
            });
        }
        PvField::VariantArray(items) => {
            out.push_str("any[]");
            push_member(out, member);
            show_compound_array(items.len(), level, out, |i, level, out| match &items[i] {
                Some(VariantValue { value, .. }) => show(
                    &PvField::Variant(Box::new(VariantValue {
                        desc: None,
                        value: value.clone(),
                    })),
                    "",
                    level,
                    out,
                ),
                None => out.push_str("null\n"),
            });
        }
    }
}

/// `if(!member.empty()) strm<<' '<<member;`
fn push_member(out: &mut String, member: &str) {
    if !member.is_empty() {
        out.push(' ');
        out.push_str(member);
    }
}

fn indent(out: &mut String, level: usize) {
    for _ in 0..level {
        out.push_str("    ");
    }
}

/// pvxs `datafmt.cpp:239-273` — `struct "id" {` newline, one indented line per
/// member, then `}` at the parent's indent. An EMPTY structure keeps its
/// braces on one line (`first` never flips, so no newline and no closing
/// indent are written).
fn show_structure(s: &PvStructure, member: &str, level: usize, out: &mut String) {
    out.push_str("struct");
    if !s.struct_id.is_empty() {
        out.push_str(&format!(" \"{}\"", s.struct_id));
    }
    out.push_str(" {");
    for (i, (name, child)) in s.fields.iter().enumerate() {
        // pvxs writes this newline ONCE, before the first member
        // (`if(first) strm<<'\n'`, datafmt.cpp:262-264); every member's own
        // `show()` already terminates its line.
        if i == 0 {
            out.push('\n');
        }
        indent(out, level + 1);
        show(child, name, level + 1, out);
    }
    if !s.fields.is_empty() {
        indent(out, level);
    }
    out.push('}');
    push_member(out, member);
    out.push('\n');
}

/// pvxs `datafmt.cpp:274-311` — ` = {N}[` newline, one indented element per
/// line, then `]`. The member name was already emitted by the caller.
fn show_compound_array(
    len: usize,
    level: usize,
    out: &mut String,
    mut element: impl FnMut(usize, usize, &mut String),
) {
    out.push_str(&format!(" = {{{len}}}["));
    for i in 0..len {
        // As in `show_structure`: one newline, before the first element
        // (`if(!shown) strm<<'\n'`, datafmt.cpp:288-290).
        if i == 0 {
            out.push('\n');
        }
        indent(out, level + 1);
        element(i, level + 1, out);
    }
    if len > 0 {
        indent(out, level);
    }
    out.push_str("]\n");
}

/// The differential that makes three transcriptions of `%g` safe.
///
/// `epics-base-rs` (`printf` record), `epics-pva-rs` (`pvget`/`pvinfo`/
/// `pvmonitor`) and `asyn-rs` (`paramVal::report`) each carry their own
/// `format_g`, because sharing one would force a crate dependency none of
/// the three otherwise needs. What keeps them equal is not review but this
/// test: each crate runs the SAME sample through its own `format_g` and
/// through glibc's `snprintf("%.*g")` and requires byte equality. A
/// transcription that drifts fails here, in its own crate, on the sample
/// that caught the drift.
///
/// Gated on `target_env = "gnu"`: the assertion is glibc's exact output,
/// and newlib (RTEMS) / musl are not that reference.
#[cfg(all(test, unix, target_env = "gnu"))]
mod libc_g_differential {
    use super::format_g;

    /// glibc `printf("%.*g", prec, v)`.
    pub(super) fn libc_g(v: f64, prec: usize) -> String {
        let mut buf = [0u8; 512];
        // SAFETY: `buf` is 512 bytes and `snprintf` is given that length,
        // so it always NUL-terminates within bounds. `%.*g` of an f64 with
        // precision <= 17 never needs more than ~330 bytes.
        let n = unsafe {
            libc::snprintf(
                buf.as_mut_ptr().cast(),
                buf.len(),
                c"%.*g".as_ptr(),
                prec as libc::c_int,
                v,
            )
        };
        assert!(n >= 0 && (n as usize) < buf.len(), "snprintf overflow");
        String::from_utf8(buf[..n as usize].to_vec()).expect("glibc writes ASCII")
    }

    /// xorshift64*, so the sample is identical on every run and every host
    /// without pulling in a PRNG crate.
    pub(super) struct XorShift64(u64);
    impl XorShift64 {
        pub(super) fn new(seed: u64) -> Self {
            Self(seed)
        }
        pub(super) fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    /// Raw bit patterns (so NaN, infinities and subnormals arrive on their
    /// own), a decade sweep across the whole exponent range, and the
    /// boundary values `%g`'s style decision turns on.
    pub(super) fn sample() -> Vec<f64> {
        let mut out = vec![
            0.0,
            -0.0,
            f64::NAN,
            -f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MIN_POSITIVE,
            f64::from_bits(1),
            f64::MAX,
            f64::MIN,
            1.0 / 3.0,
            0.1 + 0.2,
            // the rounded-exponent boundary: 9.999995 must print as C does
            9.999_995,
            99_999.5,
            999_999.5,
            0.000_099_999_95,
        ];
        for e in -320i32..=308 {
            for m in [1.0_f64, 1.5, 3.3333333, 9.999999, 9.9999995] {
                let v = m * 10f64.powi(e);
                if v.is_finite() {
                    out.push(v);
                    out.push(-v);
                }
            }
        }
        let mut rng = XorShift64::new(0x5EED_1234_ABCD_0001);
        for _ in 0..50_000 {
            out.push(f64::from_bits(rng.next()));
        }
        out
    }

    #[test]
    fn format_g_is_byte_identical_to_glibc() {
        let values = sample();
        let mut checked = 0usize;
        let mut mismatches: Vec<String> = Vec::new();
        for prec in [1usize, 3, 6, 17] {
            for &v in &values {
                let ours = format_g(v, prec);
                let theirs = libc_g(v, prec);
                checked += 1;
                if ours != theirs && mismatches.len() < 20 {
                    mismatches.push(format!(
                        "%.{prec}g of {v:?} (bits {:#018x}): ours {ours:?} != glibc {theirs:?}",
                        v.to_bits()
                    ));
                }
            }
        }
        assert!(
            mismatches.is_empty(),
            "{} of {checked} samples disagree with glibc:\n{}",
            mismatches.len(),
            mismatches.join("\n")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epics_base_rs::types::PvString;

    #[test]
    fn scalars_render_with_the_pvxs_c_type_names() {
        // pvxs `TypeCode::name()` (type.cpp:126-166) — NOT the pvData wire
        // names. Both pre-R10-36 renderers invented `int32` / `float64`.
        assert_eq!(
            render_value(&PvField::Scalar(ScalarValue::Int(-5))),
            "int32_t = -5\n"
        );
        assert_eq!(
            render_value(&PvField::Scalar(ScalarValue::UInt(5))),
            "uint32_t = 5\n"
        );
        assert_eq!(
            render_value(&PvField::Scalar(ScalarValue::Byte(-1))),
            "int8_t = -1\n"
        );
        assert_eq!(
            render_value(&PvField::Scalar(ScalarValue::ULong(9))),
            "uint64_t = 9\n"
        );
        assert_eq!(
            render_value(&PvField::Scalar(ScalarValue::Float(1.5))),
            "float = 1.5\n"
        );
        assert_eq!(
            render_value(&PvField::Scalar(ScalarValue::Double(1.5))),
            "double = 1.5\n"
        );
    }

    /// `FmtTree::show_value` prints a scalar bool through an explicit
    /// `true`/`false` (datafmt.cpp:133-135).
    #[test]
    fn scalar_bool_is_true_false() {
        assert_eq!(
            render_value(&PvField::Scalar(ScalarValue::Boolean(true))),
            "bool = true\n"
        );
        assert_eq!(
            render_value(&PvField::Scalar(ScalarValue::Boolean(false))),
            "bool = false\n"
        );
    }

    #[test]
    fn strings_are_quoted_and_escaped() {
        assert_eq!(
            render_value(&PvField::Scalar(ScalarValue::String("maybe".into()))),
            "string = \"maybe\"\n"
        );
        assert_eq!(
            render_value(&PvField::Scalar(ScalarValue::String(
                "a\"b\\c\nd\te".into()
            ))),
            "string = \"a\\\"b\\\\c\\nd\\te\"\n"
        );
        // Non-printable → `\xNN` over the raw bytes (util.cpp:231-236).
        assert_eq!(escape("\x01\x7f"), "\\x01\\x7f");
    }

    /// C++ `operator<<(double)` is `%g` at precision 6, not Rust's
    /// shortest-round-trip `{}`.
    #[test]
    fn reals_render_as_cxx_default_ostream() {
        let cases = [
            (0.0, "0"),
            (-0.0, "-0"),
            (1.5, "1.5"),
            (0.1, "0.1"),
            (1.0 / 3.0, "0.333333"),
            (100000.0, "100000"),
            (1e6, "1e+06"),
            (9_999_999.0, "1e+07"),
            (123456789.0, "1.23457e+08"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (-2.5, "-2.5"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (f64::NAN, "nan"),
        ];
        for (v, want) in cases {
            assert_eq!(render_real(v), want, "%g of {v}");
        }
    }

    /// `showArr` (sharedarray.cpp:104-123): `{N}[…]`, and an ARRAY bool
    /// prints `1`/`0` — unlike the scalar bool above.
    #[test]
    fn arrays_render_with_count_prefix_and_numeric_bools() {
        assert_eq!(
            render_value(&PvField::ScalarArrayTyped(TypedScalarArray::Int(
                vec![1, 2, 3].into()
            ))),
            "int32_t[] = {3}[1, 2, 3]\n"
        );
        assert_eq!(
            render_value(&PvField::ScalarArrayTyped(TypedScalarArray::Boolean(
                vec![true, false].into()
            ))),
            "bool[] = {2}[1, 0]\n"
        );
        assert_eq!(
            render_value(&PvField::ScalarArrayTyped(TypedScalarArray::String(
                vec![PvString::from("a b"), PvString::from("c")].into()
            ))),
            "string[] = {2}[\"a b\", \"c\"]\n"
        );
        assert_eq!(
            render_value(&PvField::ScalarArrayTyped(TypedScalarArray::Double(
                vec![].into()
            ))),
            "double[] = {0}[]\n"
        );
    }

    /// `struct "id" { … }` with four-space indent per level
    /// (util.cpp:128-139).
    #[test]
    fn structures_render_as_an_indented_tree() {
        let mut inner = PvStructure::new("");
        inner
            .fields
            .push(("b".into(), PvField::Scalar(ScalarValue::Boolean(true))));
        let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
        s.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(2.5))));
        s.fields.push(("nest".into(), PvField::Structure(inner)));

        assert_eq!(
            render_value(&PvField::Structure(s)),
            "struct \"epics:nt/NTScalar:1.0\" {\n\
             \x20   double value = 2.5\n\
             \x20   struct {\n\
             \x20       bool b = true\n\
             \x20   } nest\n\
             }\n"
        );
    }

    /// An empty struct keeps `{}` on one line — pvxs's `first` flag never
    /// flips, so neither the newline nor the closing indent is written.
    #[test]
    fn empty_structure_keeps_its_braces_inline() {
        assert_eq!(
            render_value(&PvField::Structure(PvStructure::new(""))),
            "struct {}\n"
        );
    }

    /// A union shows the SELECTED member by recursion; an unselected one
    /// recurses into an empty value, which is pvxs's `null` line.
    #[test]
    fn unions_show_the_selected_member() {
        assert_eq!(
            render_value(&PvField::Union {
                selector: 1,
                variant_name: "ival".into(),
                value: Box::new(PvField::Scalar(ScalarValue::Int(7))),
            }),
            "union.ival int32_t = 7\n"
        );
        assert_eq!(
            render_value(&PvField::Union {
                selector: -1,
                variant_name: String::new(),
                value: Box::new(PvField::Null),
            }),
            "union null\n"
        );
    }

    #[test]
    fn variant_shows_its_value() {
        assert_eq!(
            render_value(&PvField::Variant(Box::new(VariantValue {
                desc: None,
                value: PvField::Scalar(ScalarValue::String("x".into())),
            }))),
            "any string = \"x\"\n"
        );
    }

    /// `struct[] = {N}[ … ]`, one indented element per line, null elements
    /// rendered as pvxs's `null`.
    #[test]
    fn structure_arrays_render_elementwise() {
        let mut e = PvStructure::new("");
        e.fields
            .push(("a".into(), PvField::Scalar(ScalarValue::Int(1))));
        assert_eq!(
            render_value(&PvField::StructureArray(vec![Some(e), None])),
            "struct[] = {2}[\n\
             \x20   struct {\n\
             \x20       int32_t a = 1\n\
             \x20   }\n\
             \x20   null\n\
             ]\n"
        );
    }

    /// The one shape a value-only renderer cannot type: an empty untyped
    /// `ScalarArray`. Unreachable from the wire (the decoder always yields
    /// `ScalarArrayTyped`), pinned so the fallback does not drift.
    #[test]
    fn empty_untyped_scalar_array_has_no_recoverable_type() {
        assert_eq!(
            render_value(&PvField::ScalarArray(Vec::new())),
            "null = {0}[]\n"
        );
        // A NON-empty untyped array recovers its type from element 0.
        assert_eq!(
            render_value(&PvField::ScalarArray(vec![ScalarValue::Short(3)])),
            "int16_t[] = {1}[3]\n"
        );
    }
}
