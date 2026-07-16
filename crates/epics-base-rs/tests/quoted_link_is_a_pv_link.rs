//! R17-68: there is no quoted-string constant link.
//!
//! C's `dbParseLink` (`dbStaticLib.c:2280-2356`) runs exactly three tests on the
//! link text: braces → JSON link, `epicsParseDouble` → CONSTANT, otherwise PV
//! link. A quote is not a number, so text that still carries quotes after the
//! `.db` lexer is a PV NAME — quotes included — and the record comes up with a
//! never-connected CA link and UDF=1.
//!
//! The port had an extra arm ahead of the number test that turned `"hello"`
//! into `ParsedLink::Constant("hello")` (and `""` into an unset link), inventing
//! a link type C does not have: the record came up DEFINED holding the text.
//!
//! softIoc (EPICS 7.0.10, linux-x86_64), `dbpr X 2` after `iocInit`:
//!
//! ```text
//! record(ai,"QA"){field(INP,"\"hello\"")}        INP: CA_LINK "hello" NPP NMS  VAL 0   UDF 1
//! record(stringin,"QS"){field(INP,"\"hello\"")}  INP: CA_LINK "hello" NPP NMS  VAL ""  UDF 1
//! record(stringout,"QO"){field(DOL,"\"hi\"")}    DOL: CA_LINK "hi" NPP NMS     VAL ""  UDF 1
//! record(lso,"LQ"){field(DOL,"\"hello\"")}       DOL: CA_LINK "hello" NPP NMS  LEN 0   UDF 1
//! record(ai,"QE"){field(INP,"\"\"")}             INP: CA_LINK "" NPP NMS
//! record(ai,"QN"){field(INP,"")}                 INP: CONSTANT
//! ```

use epics_base_rs::server::record::{ParsedLink, parse_link_v2};

/// The classifier: quoted text is a PV link (an unresolved local link here,
/// which link resolution turns into CA), never a constant.
#[test]
fn quoted_text_classifies_as_a_pv_link() {
    for text in [r#""hello""#, r#""hello world""#, r#""hi""#, r#""""#] {
        let parsed = parse_link_v2(text);
        assert!(
            matches!(parsed, ParsedLink::Db(_)),
            "{text}: C makes it a CA link with the quotes in the name; got {parsed:?}"
        );
        assert_eq!(
            parsed.constant_value(),
            None,
            "{text}: it carries no constant value"
        );
    }
}

/// Only the EMPTY text is the unset CONSTANT link — `""` is not (softIoc:
/// `CA_LINK ""`), which is the boundary the deleted arm blurred.
#[test]
fn only_empty_text_is_an_unset_link() {
    assert_eq!(parse_link_v2(""), ParsedLink::None);
    assert_eq!(parse_link_v2("   "), ParsedLink::None);
    assert!(matches!(parse_link_v2(r#""""#), ParsedLink::Db(_)));
}

/// The number test still owns the CONSTANT arm, and a string-typed record
/// stores the constant's TEXT (`cvt_st_st` is a `strncpy`) — so the one way to
/// get text into a record from a link is a numeric constant's spelling, or the
/// JSON `{const:"…"}` form.
#[test]
fn numeric_text_is_still_a_constant() {
    assert_eq!(
        parse_link_v2("1.50"),
        ParsedLink::Constant("1.50".to_string())
    );
    assert_eq!(
        parse_link_v2(r#"{const:"hello"}"#),
        ParsedLink::Constant("hello".to_string())
    );
}
