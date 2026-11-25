use epics_base_rs::calc::{CalcError, compile};

#[test]
fn test_unmatched_close_paren() {
    let result = compile("A+B)");
    assert!(matches!(result, Err(CalcError::ParenNotOpen)));
}

#[test]
fn test_unmatched_open_paren() {
    let result = compile("(A+B");
    assert!(matches!(result, Err(CalcError::ParenOpen)));
}

#[test]
fn test_missing_operand() {
    let result = compile("A+");
    assert!(matches!(result, Err(CalcError::Incomplete)));
}

#[test]
fn test_unknown_identifier() {
    let result = compile("UNKNOWN(A)");
    assert!(result.is_err());
}

#[test]
fn test_unbalanced_ternary_no_colon() {
    let result = compile("A?B");
    assert!(matches!(result, Err(CalcError::Conditional)));
}

#[test]
fn test_extra_colon() {
    let result = compile("A:B");
    assert!(result.is_err());
}

#[test]
fn test_bad_assignment() {
    // Can't assign to a literal
    let result = compile("5:=A");
    assert!(matches!(result, Err(CalcError::BadAssignment)));
}

#[test]
fn test_double_operator() {
    let result = compile("A++B");
    // Second + is unary plus which we don't have, so it should fail
    // Actually in our implementation '+' in operand position is unknown
    assert!(result.is_err());
}
