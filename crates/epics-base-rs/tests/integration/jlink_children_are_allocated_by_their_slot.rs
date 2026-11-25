//! `dbjl_start_map` hands a child link the field type its parent's
//! `parse_start_map` asked for, and `dbjl_map_key` looks the child's type up
//! and allocates it like a top-level one (`dbJLink.c:193-301`); the child's
//! own callbacks then read its value. Each report is what softIoc R7.0.10.1
//! (8f5015b66) printed for the same text.

use epics_base_rs::error::CaError;
use epics_base_rs::server::record::{LinkFieldType, check_json_link_text};

fn report(text: &str, ftype: LinkFieldType) -> Option<String> {
    match check_json_link_text(text, ftype) {
        Err(CaError::BadLink(refusal)) => refusal.report(),
        other => panic!("{text} was not refused as a link: {other:?}"),
    }
}

#[test]
fn a_calc_out_child_is_an_output_link() {
    assert_eq!(
        report(r#"{calc:{expr:"A",out:{const:1}}}"#, LinkFieldType::Out).as_deref(),
        Some(
            "lnkConst: Only works with input links\n\
             dbJLinkInit: Link type 'const' allocation failed. \n"
        )
    );
}

#[test]
fn a_child_is_read_by_its_own_callbacks() {
    assert_eq!(
        report(
            r#"{calc:{expr:"A",args:[{const:[1,"a"]}]}}"#,
            LinkFieldType::In
        )
        .as_deref(),
        Some("lnkConst: Mixed data types in array\n")
    );
    assert_eq!(
        report(r#"{calc:{expr:"A",args:[{state:1}]}}"#, LinkFieldType::In),
        None
    );
    assert_eq!(
        report(
            r#"{calc:{expr:"A",args:[{const:null}]}}"#,
            LinkFieldType::In
        ),
        None
    );
}

#[test]
fn a_child_type_is_looked_up_like_a_top_level_one() {
    assert_eq!(
        report(r#"{calc:{expr:"A",args:[{nope:1}]}}"#, LinkFieldType::In).as_deref(),
        Some("dbJLinkInit: Link type 'nope' not found\n")
    );
}

#[test]
fn children_c_takes_load() {
    for (text, ftype) in [
        (r#"{calc:{expr:"A",args:[{state:"s"}]}}"#, LinkFieldType::In),
        (
            r#"{calc:{expr:"A",args:[{calc:{expr:"1"}}]}}"#,
            LinkFieldType::In,
        ),
        (
            r#"{calc:{expr:"A",args:[{const:1},{const:2}]}}"#,
            LinkFieldType::In,
        ),
    ] {
        assert!(check_json_link_text(text, ftype).is_ok(), "{text} refused");
    }
}
