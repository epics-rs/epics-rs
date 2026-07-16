//! R17-61: "a link is a CONSTANT iff `epicsParseDouble` accepts it"
//! (`dbStaticLib.c:2346-2349`) — and the port must run C's `epicsParseDouble`,
//! not a look-alike.
//!
//! The classifier and the constant LOADER are the same C function, so whatever
//! classifies as CONSTANT must also load: `dbParseLink` tests with
//! `epicsParseDouble`, and `dbConstLink.c`'s `cvt_st_*` family converts with
//! the same library parse. Three families diverged, all probed on softIoc
//! (EPICS 7.0.10, linux-x86_64) with `record(ai,"X"){field(INP,"<literal>")}`
//! and `dbpr X 2`:
//!
//! ```text
//! 0x1p4                 -> INP: CONSTANT 0x1p4                 VAL 16          UDF 0
//! 0x1f                  -> INP: CONSTANT 0x1f                  VAL 31          UDF 0
//! 0xffffffffffffffffff  -> INP: CONSTANT 0xffff…               VAL 4.72236648286965e+21  UDF 0
//! inf                   -> INP: CONSTANT inf                   VAL inf         UDF 0
//! nan                   -> INP: CONSTANT nan                   VAL nan         UDF 0
//! 1e400                 -> INP: CA_LINK 1e400 NPP NMS          VAL 0           UDF 1
//! 1e-320                -> INP: CA_LINK 1e-320 NPP NMS         VAL 0           UDF 1
//! ```
//!
//! Pre-fix: the hex branch was `u64::from_str_radix`, so `0x1p4` (hex float)
//! and the 18-digit wide hex both failed it and became PV links; and the
//! decimal branch was `str::parse::<f64>`, which has no ERANGE — `1e400`
//! became a CONSTANT and the record came up DEFINED holding `inf`, where C
//! leaves it an unresolvable link with UDF=1.

use epics_base_rs::server::record::{ParsedLink, parse_c_double, parse_link_v2};

fn classify(text: &str) -> ParsedLink {
    parse_link_v2(text)
}

/// `epicsParseDouble` accepts every C99 `strtod` form, so each of these is a
/// CONSTANT in C — and loads the value `strtod` returns.
#[test]
fn strtod_forms_are_constants_and_load_their_value() {
    for (text, expect) in [
        ("0x1f", 31.0),
        ("0x10", 16.0),
        ("0x1p4", 16.0),
        ("0X1.8p1", 3.0),
        ("-0x10", -16.0),
        ("0xffffffffffffffffff", 4.722366482869645e21),
        ("5", 5.0),
        ("-2.5e2", -250.0),
        (".5", 0.5),
    ] {
        assert!(
            matches!(classify(text), ParsedLink::Constant(_)),
            "{text}: C `dbParseLink` classifies it CONSTANT"
        );
        assert_eq!(
            parse_c_double(text),
            Some(expect),
            "{text}: the classifier and the loader are one C function"
        );
    }

    // The `inf`/`nan` words leave errno clear in glibc, so C takes them too.
    assert!(matches!(classify("inf"), ParsedLink::Constant(_)));
    assert_eq!(parse_c_double("inf"), Some(f64::INFINITY));
    assert!(matches!(classify("nan"), ParsedLink::Constant(_)));
    assert!(parse_c_double("nan").unwrap().is_nan());
}

/// ERANGE is a NON-ZERO `epicsParseDouble` status, so C's `dbParseLink` falls
/// through to PV_LINK: the record comes up with an unresolvable link and UDF=1,
/// NOT defined holding `inf` (or a subnormal).
#[test]
fn erange_literals_are_pv_links_not_constants() {
    for text in ["1e400", "-1e400", "1e-320", "1e-400"] {
        assert_eq!(
            parse_c_double(text),
            None,
            "{text}: epicsParseDouble returns ERANGE, so it is not a number here"
        );
        assert!(
            matches!(classify(text), ParsedLink::Db(_)),
            "{text}: C makes it a PV link (softIoc: `CA_LINK {text} NPP NMS`, UDF=1) — \
             an unresolved local link here, which link resolution converts to CA. Got {:?}",
            classify(text)
        );
    }
}

/// The rule that keeps `"5 PP"` a PV link — trailing text is
/// `S_stdlib_extraneous` because C passes a NULL `units` pointer — is unchanged
/// by sharing the parse.
#[test]
fn trailing_text_still_makes_a_pv_link() {
    assert_eq!(parse_c_double("5 PP"), None);
    assert!(matches!(classify("5 PP"), ParsedLink::Db(_)));
    assert_eq!(parse_c_double("0x1f MS"), None);
    assert!(matches!(classify("0x1f MS"), ParsedLink::Db(_)));
}
