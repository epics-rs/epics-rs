//! Boundary-value case generation.
//!
//! Cases are generated **from the field's declared type**, not from stories
//! about what the field is for. Per-scenario tests pass while leaving
//! boundaries uncovered; per-boundary cases are what actually catch a port
//! that narrowed a `ULONG` to a `LONG` or saturated where C wraps.
//!
//! For every type the classes are the same shape: zero, negative-one, the
//! type's min and max, one step *past* min and max, and the type-specific
//! nasties (NaN/Inf for floats, empty/oversized for strings, out-of-range
//! ordinals for enums). `-1` into an *unsigned* field is called out separately
//! because it is where signed/unsigned narrowing bugs actually surface, and the
//! port has a filed history of exactly that (R18-108: count fields declared
//! LONG where C says ULONG).

use crate::dbd::{DbfType, FieldDef};

/// One boundary value to write, with the class it is probing.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct BoundaryCase {
    /// The literal string handed to `caput` — exactly what a client would send.
    pub value: String,
    /// Which boundary class this is. Carried into the report so a failure says
    /// *what kind* of boundary broke, not just which number.
    pub class: &'static str,
}

fn c(value: impl Into<String>, class: &'static str) -> BoundaryCase {
    BoundaryCase {
        value: value.into(),
        class,
    }
}

/// The boundary values for a scalar numeric type.
fn numeric_cases(t: DbfType) -> Vec<BoundaryCase> {
    let mut v = vec![c("0", "zero"), c("1", "one")];
    match t {
        DbfType::Char => v.extend([
            c("-1", "negative-one"),
            c("127", "type-max"),
            c("-128", "type-min"),
            c("128", "over-max"),
            c("-129", "under-min"),
        ]),
        DbfType::UChar => v.extend([
            // -1 into an unsigned field: the signed/unsigned narrowing probe.
            c("-1", "negative-into-unsigned"),
            c("255", "type-max"),
            c("256", "over-max"),
        ]),
        DbfType::Short => v.extend([
            c("-1", "negative-one"),
            c("32767", "type-max"),
            c("-32768", "type-min"),
            c("32768", "over-max"),
            c("-32769", "under-min"),
        ]),
        DbfType::UShort => v.extend([
            c("-1", "negative-into-unsigned"),
            c("65535", "type-max"),
            c("65536", "over-max"),
        ]),
        DbfType::Long => v.extend([
            c("-1", "negative-one"),
            c("2147483647", "type-max"),
            c("-2147483648", "type-min"),
            c("2147483648", "over-max"),
            c("-2147483649", "under-min"),
        ]),
        DbfType::ULong => v.extend([
            c("-1", "negative-into-unsigned"),
            c("4294967295", "type-max"),
            c("4294967296", "over-max"),
        ]),
        DbfType::Int64 => v.extend([
            c("-1", "negative-one"),
            c("9223372036854775807", "type-max"),
            c("-9223372036854775808", "type-min"),
            c("9223372036854775808", "over-max"),
        ]),
        DbfType::UInt64 => v.extend([
            c("-1", "negative-into-unsigned"),
            c("18446744073709551615", "type-max"),
            c("18446744073709551616", "over-max"),
        ]),
        DbfType::Float | DbfType::Double => v.extend([
            c("-1", "negative-one"),
            c("NaN", "nan"),
            c("Inf", "positive-infinity"),
            c("-Inf", "negative-infinity"),
            // Past the range of a 32-bit float: exercises the float/double
            // narrowing path that only DBF_FLOAT fields take.
            c("1e39", "over-float-max"),
            c("1e308", "near-double-max"),
            // Past the range of a double entirely.
            c("1e400", "over-double-max"),
        ]),
        _ => {}
    }
    // Every numeric field must also be told something that is not a number.
    v.push(c("notanumber", "non-numeric-text"));
    v
}

/// Boundary values for `DBF_STRING`, sized against the field's `size(N)`.
///
/// `size(N)` counts the NUL, so the longest string a field can hold is `N-1`
/// characters. The interesting cases are therefore exactly-fits and
/// one-past-fits — the classic off-by-one in a hand-written table.
fn string_cases(size: Option<u32>) -> Vec<BoundaryCase> {
    let mut v = vec![
        c("", "empty-string"),
        c("x", "one-char"),
        c("0", "numeric-looking-string"),
    ];
    if let Some(n) = size {
        let capacity = n.saturating_sub(1) as usize; // size() includes the NUL
        if capacity > 0 {
            v.push(c("a".repeat(capacity), "exactly-fits"));
            v.push(c("b".repeat(capacity + 1), "one-over-capacity"));
        }
        // Far past capacity: a client can send up to MAX_STRING_SIZE.
        v.push(c("c".repeat((capacity + 40).max(41)), "far-over-capacity"));
    }
    v
}

/// Boundary values for an enum/menu field.
///
/// Both forms are driven, because they are different code paths in the server
/// and a port can get one right and the other wrong:
/// - the **ordinal** (`caput PV 2`), and
/// - the **choice string** (`caput PV "Passive"`).
///
/// Plus the out-of-range ordinal and an unknown string, which must be refused.
fn enum_cases(choices: Option<&[String]>) -> Vec<BoundaryCase> {
    let mut v = vec![c("0", "enum-first-ordinal")];
    match choices {
        Some(ch) if !ch.is_empty() => {
            let last = ch.len() - 1;
            v.push(c(last.to_string(), "enum-last-ordinal"));
            // One past the last valid choice: must be refused.
            v.push(c(ch.len().to_string(), "enum-over-max-ordinal"));
            v.push(c("-1", "enum-negative-ordinal"));
            // The strings themselves — first and last choice.
            v.push(c(ch[0].clone(), "enum-first-string"));
            if last > 0 {
                v.push(c(ch[last].clone(), "enum-last-string"));
            }
            v.push(c("NoSuchChoice", "enum-unknown-string"));
        }
        // A DBF_ENUM with no dbd-declared menu (bi/bo VAL: choices come from
        // ZNAM/ONAM at runtime). Ordinals only.
        _ => {
            v.push(c("1", "enum-second-ordinal"));
            v.push(c("2", "enum-over-max-ordinal"));
            v.push(c("-1", "enum-negative-ordinal"));
            v.push(c("NoSuchChoice", "enum-unknown-string"));
        }
    }
    v
}

/// The full boundary set for one field, derived from its declared type.
pub fn boundary_cases(f: &FieldDef, menu_choices: Option<&[String]>) -> Vec<BoundaryCase> {
    match f.dbf {
        t if crate::surface::is_numeric(t) => numeric_cases(t),
        DbfType::String => string_cases(f.size),
        DbfType::Enum | DbfType::Menu => enum_cases(menu_choices),
        // DEVICE (DTYP) is a menu of installed device support; its valid set is
        // runtime, not dbd. Probe only the refusal of a bogus value, which both
        // sides must agree on.
        DbfType::Device => vec![c("NoSuchDeviceSupport", "device-unknown-string")],
        // Links get a CONSTANT only -- never a PV reference. A constant sets the
        // link to a literal and creates no edge to another record, so the case
        // stays isolated to its own record instance, while still exercising the
        // put path that C's `special()` can refuse (CBUG-F6: calc INPM..INPU
        // declare special(SPC_MOD) and the record then rejects the write, so the
        // fields are unwritable over CA in C but writable in the port).
        t if t.is_link() => vec![c("0", "link-constant")],
        DbfType::NoAccess => Vec::new(),
        _ => Vec::new(),
    }
}

/// Boundary element-counts for an array field of capacity `nelm`.
///
/// Zero-length and over-NELM are the two that actually break ports: a
/// zero-length put must not be silently turned into a 1-element put, and an
/// over-NELM put must be truncated to NELM (not rejected, not overflowed).
pub fn array_cases(nelm: u32) -> Vec<(Vec<String>, &'static str)> {
    let mk = |n: usize| (0..n).map(|i| i.to_string()).collect::<Vec<_>>();
    let mut v = vec![
        // `caput -a <pv> 0` with no values: the CA client sends a write of
        // count 0 (only `countIn > native count` is refused, nciu.cpp:354), so
        // what each server does with it is genuinely observable — and a port
        // that turns it into a 1-element put differs in NORD.
        (mk(0), "array-zero-length"),
        (mk(1), "array-single-element"),
        (mk(nelm as usize), "array-exactly-nelm"),
    ];
    if nelm > 1 {
        v.push((mk((nelm / 2) as usize), "array-partial"));
    }
    // One past capacity: C truncates to NELM. A port that rejects, or that
    // writes past the end, differs observably in NORD.
    v.push((mk(nelm as usize + 1), "array-over-nelm"));
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbd::Dbd;

    fn field(dbd: &Dbd, rt: &str, name: &str) -> FieldDef {
        dbd.record_type(rt).unwrap().field(name).unwrap().clone()
    }

    const SAMPLE: &str = r#"
menu(menuScan) {
    choice(a, "Passive")
    choice(b, "Event")
    choice(c, "1 second")
}
recordtype(ai) {
    field(VAL, DBF_DOUBLE) { pp(TRUE) }
    field(PREC, DBF_SHORT) { }
    field(NELM, DBF_ULONG) { }
    field(NAME, DBF_STRING) { size(61) }
    field(SCAN, DBF_MENU) { menu(menuScan) }
    field(RVAL, DBF_ENUM) { }
    field(INP, DBF_INLINK) { }
}
"#;

    fn classes(cases: &[BoundaryCase]) -> Vec<&str> {
        cases.iter().map(|c| c.class).collect()
    }

    #[test]
    fn doubles_get_nan_and_both_infinities() {
        let d = Dbd::parse(SAMPLE).unwrap();
        let cs = boundary_cases(&field(&d, "ai", "VAL"), None);
        let cl = classes(&cs);
        assert!(cl.contains(&"nan"));
        assert!(cl.contains(&"positive-infinity"));
        assert!(cl.contains(&"negative-infinity"));
        assert!(cl.contains(&"over-double-max"));
        assert!(cl.contains(&"non-numeric-text"));
    }

    #[test]
    fn signed_type_gets_min_max_and_one_past_each() {
        let d = Dbd::parse(SAMPLE).unwrap();
        let cs = boundary_cases(&field(&d, "ai", "PREC"), None);
        let vals: Vec<&str> = cs.iter().map(|c| c.value.as_str()).collect();
        assert!(vals.contains(&"32767"), "SHORT max");
        assert!(vals.contains(&"-32768"), "SHORT min");
        assert!(vals.contains(&"32768"), "one past SHORT max");
        assert!(vals.contains(&"-32769"), "one past SHORT min");
    }

    /// The narrowing probe that R18-108 (count fields declared LONG where C
    /// says ULONG) would have been caught by.
    #[test]
    fn unsigned_type_is_probed_with_negative_one() {
        let d = Dbd::parse(SAMPLE).unwrap();
        let cs = boundary_cases(&field(&d, "ai", "NELM"), None);
        let neg = cs
            .iter()
            .find(|c| c.value == "-1")
            .expect("-1 must be driven into an unsigned field");
        assert_eq!(neg.class, "negative-into-unsigned");
        assert!(classes(&cs).contains(&"over-max"));
    }

    /// `size(61)` means 60 usable characters; the harness must probe exactly-60
    /// and 61, which is where an off-by-one in the table shows up.
    #[test]
    fn string_boundaries_respect_size_including_the_nul() {
        let d = Dbd::parse(SAMPLE).unwrap();
        let cs = boundary_cases(&field(&d, "ai", "NAME"), None);
        let fits = cs.iter().find(|c| c.class == "exactly-fits").unwrap();
        assert_eq!(fits.value.len(), 60, "size(61) holds 60 chars + NUL");
        let over = cs.iter().find(|c| c.class == "one-over-capacity").unwrap();
        assert_eq!(over.value.len(), 61);
        assert!(classes(&cs).contains(&"empty-string"));
    }

    #[test]
    fn menu_is_probed_by_ordinal_and_by_string_and_past_the_end() {
        let d = Dbd::parse(SAMPLE).unwrap();
        let choices: Vec<String> = ["Passive", "Event", "1 second"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let cs = boundary_cases(&field(&d, "ai", "SCAN"), Some(&choices));
        let vals: Vec<&str> = cs.iter().map(|c| c.value.as_str()).collect();
        assert!(vals.contains(&"2"), "last valid ordinal");
        assert!(vals.contains(&"3"), "one past the last choice: must refuse");
        assert!(vals.contains(&"Passive"), "the choice string itself");
        assert!(vals.contains(&"1 second"), "last choice string");
        assert!(vals.contains(&"NoSuchChoice"));
    }

    /// Links are written a bare constant: enough to reach the put-rejection
    /// path C's `special()` can take (CBUG-F6), without rewiring the graph.
    #[test]
    fn links_get_a_constant_put_and_never_a_pv_reference() {
        let d = Dbd::parse(SAMPLE).unwrap();
        let cs = boundary_cases(&field(&d, "ai", "INP"), None);
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].class, "link-constant");
        assert_eq!(cs[0].value, "0");
        // A value containing a PV name would create an edge to another record
        // and break case isolation.
        assert!(!cs[0].value.contains('.'));
    }

    /// The two boundaries this module's own doc calls the port-breaking ones
    /// must both produce a case. The zero-length one was named here and never
    /// generated, so the test passed while the boundary contributed nothing.
    #[test]
    fn array_cases_cover_zero_exact_and_over_nelm() {
        let cs = array_cases(4);
        let cl: Vec<&str> = cs.iter().map(|c| c.1).collect();
        assert!(cl.contains(&"array-zero-length"), "{cl:?}");
        assert!(cl.contains(&"array-exactly-nelm"), "{cl:?}");
        assert!(cl.contains(&"array-over-nelm"), "{cl:?}");
        let zero = cs.iter().find(|c| c.1 == "array-zero-length").unwrap();
        assert!(zero.0.is_empty(), "zero-length means no elements at all");
        let exact = cs.iter().find(|c| c.1 == "array-exactly-nelm").unwrap();
        assert_eq!(exact.0.len(), 4, "exactly NELM=4");
        let over = cs.iter().find(|c| c.1 == "array-over-nelm").unwrap();
        assert_eq!(over.0.len(), 5, "one past NELM=4");
    }
}
