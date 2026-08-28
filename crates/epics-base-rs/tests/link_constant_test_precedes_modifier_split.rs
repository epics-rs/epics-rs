//! R16-78: the constant test runs on the WHOLE link string, before the
//! modifier split.
//!
//! C `dbParseLink` (`dbStaticLib.c:2346-2360`) tests `epicsParseDouble` on the
//! whole stripped string with a NULL `units` argument — trailing text is
//! rejected (`S_stdlib_extraneous`) — then the bracket test (which requires a
//! trailing `]`), and only then does it look for a space to isolate the link
//! modifiers. So `"5 PP"` is a PV_LINK to a record named `5`, not the constant
//! 5. Verified on softIoc (EPICS 7, linux-x86_64):
//!
//! ```text
//! record(calc,"C1") { field(INPA,"5 PP") field(SDIS,"3 NPP") field(DISV,"3") }
//!   INPA: CA_LINK 5 PP NMS    SDIS: CA_LINK 3 NPP NMS    DISA: 0
//!   (after processing: STAT LINK, SEVR INVALID — the broken link alarms)
//! record(calc,"C4") { field(INPA,"5") }  ->  INPA: CONSTANT 5
//! ```
//!
//! The port used to split modifiers first and test only the head, so
//! `"3 NPP"` on SDIS became `Constant("3")` — with `DISV=3` that disables the
//! record permanently — and `"5 PP"` became `Constant("5")`, masking a broken
//! link as a healthy value.

use epics_base_rs::server::record::{
    LinkProcessPolicy, MonitorSwitch, ParsedLink, parse_forward_link_v2, parse_link_v2,
    parse_output_link_v2,
};

fn db_target(link: &str) -> String {
    match parse_link_v2(link) {
        ParsedLink::Db(l) => {
            let t = l.target();
            format!("{}.{}", t.record, t.field)
        }
        ParsedLink::Ca(l) => l.pv,
        other => panic!("{link:?} classified as {other:?}, expected a PV link"),
    }
}

#[test]
fn link_modifier_split_runs_after_the_constant_test() {
    // A bare number IS the constant.
    assert!(matches!(parse_link_v2("5"), ParsedLink::Constant(c) if c == "5"));
    assert!(matches!(parse_link_v2("  -3.25e2  "), ParsedLink::Constant(c) if c == "-3.25e2"));

    // A number followed by anything is a PV link to a record with that name.
    assert_eq!(db_target("5 PP"), "5.VAL");
    assert_eq!(db_target("3 NPP"), "3.VAL");
    assert_eq!(db_target("1 2 3"), "1.VAL");
    assert_eq!(db_target("7 MS"), "7.VAL");

    // The modifiers still parse off the tail of such a link.
    match parse_link_v2("5 PP MS") {
        ParsedLink::Db(l) => {
            assert_eq!(l.pvname(), "5");
            assert_eq!(
                l.policy,
                LinkProcessPolicy::ProcessPassive,
                "PP must survive the reorder"
            );
            assert_eq!(
                l.monitor_switch,
                MonitorSwitch::Maximize,
                "MS must survive the reorder"
            );
        }
        other => panic!("expected a DB link, got {other:?}"),
    }
    // `CA` still forces the CA route (the constant test failed first).
    assert!(matches!(parse_link_v2("5 CA"), ParsedLink::Ca(l) if l.pv == "5"));

    // The bracket test needs a TRAILING `]`, so a bracketed literal with
    // modifiers is a link in C too — do not "fix" this into a constant.
    assert_eq!(db_target("[1,2,3] PP"), "[1,2,3].VAL");
    assert!(matches!(parse_link_v2("[1,2,3]"), ParsedLink::Constant(_)));
}

/// The classifier is shared by all three link-field types, so the same reorder
/// must hold for OUT and FLNK: C writes to (or forwards to) record `5`, it does
/// not silently drop the write into a constant.
#[test]
fn out_and_fwd_links_take_the_same_classification() {
    assert!(matches!(parse_output_link_v2("5 PP"), ParsedLink::Db(l) if l.pvname() == "5"));
    assert!(matches!(parse_output_link_v2("5"), ParsedLink::Constant(_)));
    assert!(matches!(parse_forward_link_v2("5 PP"), ParsedLink::Db(l) if l.pvname() == "5"));
    assert!(matches!(
        parse_forward_link_v2("5"),
        ParsedLink::Constant(_)
    ));
}
