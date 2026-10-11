//! `lnkCalcIf`'s parse callbacks (`lnkCalc.c:122-370`) refuse a link by
//! key, by the token that arrives in each parser state, and in
//! `lnkCalc_end_map` by what an input or output link still lacks. Each
//! report is what softIoc R7.0.10.1 (8f5015b66) printed for the same text.

use epics_base_rs::error::CaError;
use epics_base_rs::server::record::{LinkFieldType, check_json_link_text};

fn report(text: &str, ftype: LinkFieldType) -> Option<String> {
    match check_json_link_text(text, ftype) {
        Err(CaError::BadLink(refusal)) => refusal.report(),
        other => panic!("{text} was not refused as a link: {other:?}"),
    }
}

fn input(text: &str) -> Option<String> {
    report(text, LinkFieldType::In)
}

#[test]
fn an_output_calc_needs_its_out_key() {
    let none = Some("lnkCalc: No output link ('out' key)\n".to_string());
    assert_eq!(report(r#"{calc:{expr:"A"}}"#, LinkFieldType::Out), none);
    assert_eq!(
        report(
            r#"{calc:{expr:"A",out:{calc:{expr:"B"}}}}"#,
            LinkFieldType::Out
        ),
        none
    );
}

#[test]
fn an_input_calc_needs_its_expr_before_a_child_map_closes() {
    let none = Some("lnkCalc: No expression ('expr' key)\n".to_string());
    assert_eq!(input(r#"{calc:{args:[{const:1}],expr:"A"}}"#), none);
    assert_eq!(input(r#"{calc:{expr:"A",args:[{calc:{args:[1]}}]}}"#), none);
}

#[test]
fn keys_are_refused_by_name_state_and_repeat() {
    assert_eq!(
        input(r#"{calc:{expr:"A",out:{state:"s"}}}"#).as_deref(),
        Some("lnkCalc: Unknown key \"out\"\n")
    );
    assert_eq!(
        input(r#"{calc:{expr:"A",args:[1,2],args:[3]}}"#).as_deref(),
        Some("lnkCalc: Unknown key \"args\"\n")
    );
    assert_eq!(
        input(r#"{calc:{expr:"A",bogus:1}}"#).as_deref(),
        Some("lnkCalc: Unknown key \"bogus\"\n")
    );
    assert_eq!(
        input(r#"{calc:{expr:"A",args:[{const:1,x:2}]}}"#).as_deref(),
        Some("lnkCalc: Unknown key \"x\"\n")
    );
}

#[test]
fn values_are_refused_by_the_parser_state() {
    assert_eq!(
        input(r#"{calc:{expr:"A+"}}"#).as_deref(),
        Some("lnkCalc: Error in calc expression, Incomplete expression, operand missing\n")
    );
    assert_eq!(
        input(r#"{calc:{expr:"A",prec:2.5}}"#).as_deref(),
        Some("lnkCalc: Unexpected double 2.5\n")
    );
    assert_eq!(
        input(r#"{calc:{expr:"A",time:"z"}}"#).as_deref(),
        Some("lnkCalc: Bad 'time' parameter \"z\"\n")
    );
    assert_eq!(
        input(r#"{calc:{expr:"A",units:{a:1}}}"#).as_deref(),
        Some("lnkCalc: Unexpected map\n")
    );
    assert_eq!(
        input(
            r#"{calc:{expr:"A",args:[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22]}}"#
        )
        .as_deref(),
        Some("lnkCalc: Too many input args, limit is 21\n")
    );
    // `parse_boolean` is NULL in `lnkCalcIf`: a stop with no word.
    assert_eq!(input(r#"{calc:{expr:"A",args:[true]}}"#), None);
    assert_eq!(input("{calc:true}"), None);
}

#[test]
fn an_output_calc_with_its_out_needs_no_expr() {
    assert!(
        check_json_link_text(r#"{calc:{expr:"A",out:{state:"s"}}}"#, LinkFieldType::Out).is_ok()
    );
}
