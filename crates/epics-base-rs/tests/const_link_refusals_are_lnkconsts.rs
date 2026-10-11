//! A `{const:…}` link is parsed by yajl over the whole text with the value
//! handed to `lnkConst`'s callbacks; what they or yajl refuse never loads.
//! Each report below is what softIoc R7.0.10 printed for the same text.

use epics_base_rs::error::CaError;
use epics_base_rs::server::record::{LinkFieldType, check_json_link_text};

fn report(text: &str) -> Option<String> {
    match check_json_link_text(text, LinkFieldType::In) {
        Err(CaError::BadLink(refusal)) => refusal.report(),
        other => panic!("{text} was not refused as a link: {other:?}"),
    }
}

#[test]
fn lnkconst_refuses_what_c_refuses() {
    let mixed = Some("lnkConst: Mixed data types in array\n".to_string());
    let embedded = Some("lnkConst: Embedded array value\n".to_string());
    assert_eq!(report(r#"{const:[1,"a"]}"#), mixed);
    assert_eq!(report(r#"{const:["a",1]}"#), mixed);
    assert_eq!(report(r#"{const:[true,"a"]}"#), mixed);
    assert_eq!(report("{const:[[1]]}"), embedded);
    assert_eq!(report("{const:[1,[2]]}"), embedded);
    // `parse_null` and `parse_start_map` are NULL: a stop with no word.
    assert_eq!(report("{const:null}"), None);
    assert_eq!(report("{const:{a:1}}"), None);
    assert_eq!(
        report("{const:1,x:2}").as_deref(),
        Some("dbJLinkInit: Illegal second link key 'x'\n")
    );
}

#[test]
fn a_yajl_error_prints_its_block_against_the_whole_link() {
    assert_eq!(
        report("{const:99999999999999999999}").as_deref(),
        Some(
            "dbJLinkInit: parse error: integer overflow\n\
             \x20                                {const:99999999999999999999}\n\
             \x20                    (right here) ------^\n\n"
        )
    );
    assert_eq!(
        report("{const:[1,99999999999999999999]}").as_deref(),
        Some(
            "dbJLinkInit: parse error: integer overflow\n\
             \x20                             {const:[1,99999999999999999999]}\n\
             \x20                    (right here) ------^\n\n"
        )
    );
    assert_eq!(
        report("{const:1e999}").as_deref(),
        Some(
            "dbJLinkInit: parse error: numeric (floating point) overflow\n\
             \x20                                {const:1e999}\n\
             \x20                    (right here) ------^\n\n"
        )
    );
}

#[test]
fn lnkconst_takes_what_c_takes() {
    for text in [
        "{const:[1.5,2]}",
        "{const:true}",
        r#"{const:"abc"}"#,
        "{const:[]}",
        "{const:[1,]}",
        "{const:'a'}",
        "{const:-5}",
        r#"{const:["a","b"]}"#,
    ] {
        assert!(
            check_json_link_text(text, LinkFieldType::In).is_ok(),
            "{text} refused"
        );
    }
}

#[test]
fn a_link_type_name_matches_case_sensitively_like_dbfindlinksup() {
    assert_eq!(
        report("{CONST:5}").as_deref(),
        Some("dbJLinkInit: Link type 'CONST' not found\n")
    );
    assert_eq!(
        report(r#"{Calc:{expr:"1"}}"#).as_deref(),
        Some("dbJLinkInit: Link type 'Calc' not found\n")
    );
}
