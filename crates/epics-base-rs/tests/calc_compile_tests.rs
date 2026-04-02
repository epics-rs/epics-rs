use epics_base_rs::calc::compile;
use epics_base_rs::calc::engine::opcodes::{CoreOp, Opcode};

fn opcodes_without_end(expr: &str) -> Vec<Opcode> {
    let compiled = compile(expr).unwrap();
    compiled
        .code
        .into_iter()
        .filter(|op| !matches!(op, Opcode::Core(CoreOp::End)))
        .collect()
}

fn c(op: CoreOp) -> Opcode {
    Opcode::Core(op)
}

#[test]
fn test_simple_add() {
    let ops = opcodes_without_end("A+B");
    assert_eq!(
        ops,
        vec![c(CoreOp::PushVar(0)), c(CoreOp::PushVar(1)), c(CoreOp::Add)]
    );
}

#[test]
fn test_precedence_mul_over_add() {
    let ops = opcodes_without_end("A+B*C");
    assert_eq!(
        ops,
        vec![
            c(CoreOp::PushVar(0)),
            c(CoreOp::PushVar(1)),
            c(CoreOp::PushVar(2)),
            c(CoreOp::Mul),
            c(CoreOp::Add),
        ]
    );
}

#[test]
fn test_parentheses() {
    let ops = opcodes_without_end("(A+B)*C");
    assert_eq!(
        ops,
        vec![
            c(CoreOp::PushVar(0)),
            c(CoreOp::PushVar(1)),
            c(CoreOp::Add),
            c(CoreOp::PushVar(2)),
            c(CoreOp::Mul),
        ]
    );
}

#[test]
fn test_unary_neg() {
    let ops = opcodes_without_end("-A");
    assert_eq!(ops, vec![c(CoreOp::PushVar(0)), c(CoreOp::Neg)]);
}

#[test]
fn test_neg_paren() {
    let ops = opcodes_without_end("-(A+B)");
    assert_eq!(
        ops,
        vec![
            c(CoreOp::PushVar(0)),
            c(CoreOp::PushVar(1)),
            c(CoreOp::Add),
            c(CoreOp::Neg),
        ]
    );
}

#[test]
fn test_function_sin() {
    let ops = opcodes_without_end("SIN(A)");
    assert_eq!(ops, vec![c(CoreOp::PushVar(0)), c(CoreOp::Sin)]);
}

#[test]
fn test_ternary() {
    let ops = opcodes_without_end("A?B:C");
    assert_eq!(
        ops,
        vec![
            c(CoreOp::PushVar(0)),
            c(CoreOp::CondIf),
            c(CoreOp::PushVar(1)),
            c(CoreOp::CondElse),
            c(CoreOp::PushVar(2)),
            c(CoreOp::CondEnd),
        ]
    );
}

#[test]
fn test_constant_literal() {
    let ops = opcodes_without_end("1+2");
    assert_eq!(
        ops,
        vec![
            c(CoreOp::PushConst(1.0)),
            c(CoreOp::PushConst(2.0)),
            c(CoreOp::Add),
        ]
    );
}

#[test]
fn test_double_star_power() {
    let ops = opcodes_without_end("A**B");
    assert_eq!(
        ops,
        vec![
            c(CoreOp::PushVar(0)),
            c(CoreOp::PushVar(1)),
            c(CoreOp::Power)
        ]
    );
}

#[test]
fn test_caret_power() {
    let ops = opcodes_without_end("A^B");
    assert_eq!(
        ops,
        vec![
            c(CoreOp::PushVar(0)),
            c(CoreOp::PushVar(1)),
            c(CoreOp::Power)
        ]
    );
}

#[test]
fn test_comparison_ops() {
    let ops = opcodes_without_end("A==B");
    assert_eq!(
        ops,
        vec![c(CoreOp::PushVar(0)), c(CoreOp::PushVar(1)), c(CoreOp::Eq)]
    );

    let ops = opcodes_without_end("A!=B");
    assert_eq!(
        ops,
        vec![c(CoreOp::PushVar(0)), c(CoreOp::PushVar(1)), c(CoreOp::Ne)]
    );

    let ops = opcodes_without_end("A<=B");
    assert_eq!(
        ops,
        vec![c(CoreOp::PushVar(0)), c(CoreOp::PushVar(1)), c(CoreOp::Le)]
    );

    let ops = opcodes_without_end("A>=B");
    assert_eq!(
        ops,
        vec![c(CoreOp::PushVar(0)), c(CoreOp::PushVar(1)), c(CoreOp::Ge)]
    );
}

#[test]
fn test_logical_ops() {
    let ops = opcodes_without_end("A&&B");
    assert_eq!(
        ops,
        vec![c(CoreOp::PushVar(0)), c(CoreOp::PushVar(1)), c(CoreOp::And)]
    );

    let ops = opcodes_without_end("A||B");
    assert_eq!(
        ops,
        vec![c(CoreOp::PushVar(0)), c(CoreOp::PushVar(1)), c(CoreOp::Or)]
    );
}

#[test]
fn test_bitwise_ops() {
    let ops = opcodes_without_end("A&B");
    assert_eq!(
        ops,
        vec![
            c(CoreOp::PushVar(0)),
            c(CoreOp::PushVar(1)),
            c(CoreOp::BitAnd)
        ]
    );

    let ops = opcodes_without_end("A|B");
    assert_eq!(
        ops,
        vec![
            c(CoreOp::PushVar(0)),
            c(CoreOp::PushVar(1)),
            c(CoreOp::BitOr)
        ]
    );
}

#[test]
fn test_vararg_min() {
    let ops = opcodes_without_end("MIN(A,B,C)");
    assert_eq!(
        ops,
        vec![
            c(CoreOp::PushVar(0)),
            c(CoreOp::PushVar(1)),
            c(CoreOp::PushVar(2)),
            c(CoreOp::Min(3)),
        ]
    );
}

#[test]
fn test_vararg_max() {
    let ops = opcodes_without_end("MAX(A,B)");
    assert_eq!(
        ops,
        vec![
            c(CoreOp::PushVar(0)),
            c(CoreOp::PushVar(1)),
            c(CoreOp::Max(2)),
        ]
    );
}

#[test]
fn test_assign() {
    let ops = opcodes_without_end("A:=5");
    assert_eq!(
        ops,
        vec![c(CoreOp::PushConst(5.0)), c(CoreOp::StoreVar(0)),]
    );
}

#[test]
fn test_semicolon() {
    let ops = opcodes_without_end("A:=5;A+1");
    assert_eq!(
        ops,
        vec![
            c(CoreOp::PushConst(5.0)),
            c(CoreOp::StoreVar(0)),
            c(CoreOp::PushVar(0)),
            c(CoreOp::PushConst(1.0)),
            c(CoreOp::Add),
        ]
    );
}

#[test]
fn test_bang_not() {
    let ops = opcodes_without_end("!A");
    assert_eq!(ops, vec![c(CoreOp::PushVar(0)), c(CoreOp::Not)]);
}

#[test]
fn test_tilde_bitnot() {
    let ops = opcodes_without_end("~A");
    assert_eq!(ops, vec![c(CoreOp::PushVar(0)), c(CoreOp::BitNot)]);
}

#[test]
fn test_max_min_operators() {
    let ops = opcodes_without_end("A>?B");
    assert_eq!(
        ops,
        vec![
            c(CoreOp::PushVar(0)),
            c(CoreOp::PushVar(1)),
            c(CoreOp::MaxVal)
        ]
    );

    let ops = opcodes_without_end("A<?B");
    assert_eq!(
        ops,
        vec![
            c(CoreOp::PushVar(0)),
            c(CoreOp::PushVar(1)),
            c(CoreOp::MinVal)
        ]
    );
}

#[test]
fn test_constants() {
    let ops = opcodes_without_end("PI");
    assert_eq!(ops, vec![c(CoreOp::Pi)]);

    let ops = opcodes_without_end("D2R");
    assert_eq!(ops, vec![c(CoreOp::D2R)]);

    let ops = opcodes_without_end("R2D");
    assert_eq!(ops, vec![c(CoreOp::R2D)]);
}

#[test]
fn test_random() {
    let ops = opcodes_without_end("RNDM");
    assert_eq!(ops, vec![c(CoreOp::Random)]);

    let ops = opcodes_without_end("NRNDM");
    assert_eq!(ops, vec![c(CoreOp::NormalRandom)]);
}

#[test]
fn test_atan2() {
    let ops = opcodes_without_end("ATAN2(A,B)");
    assert_eq!(
        ops,
        vec![
            c(CoreOp::PushVar(0)),
            c(CoreOp::PushVar(1)),
            c(CoreOp::Atan2),
        ]
    );
}

#[test]
fn test_shift_ops() {
    let ops = opcodes_without_end("A>>B");
    assert_eq!(
        ops,
        vec![c(CoreOp::PushVar(0)), c(CoreOp::PushVar(1)), c(CoreOp::Shr)]
    );

    let ops = opcodes_without_end("A<<B");
    assert_eq!(
        ops,
        vec![c(CoreOp::PushVar(0)), c(CoreOp::PushVar(1)), c(CoreOp::Shl)]
    );
}

#[test]
fn test_complex_precedence() {
    let ops = opcodes_without_end("A+B*C^D");
    assert_eq!(
        ops,
        vec![
            c(CoreOp::PushVar(0)),
            c(CoreOp::PushVar(1)),
            c(CoreOp::PushVar(2)),
            c(CoreOp::PushVar(3)),
            c(CoreOp::Power),
            c(CoreOp::Mul),
            c(CoreOp::Add),
        ]
    );
}

#[test]
fn test_nested_functions() {
    let ops = opcodes_without_end("SIN(COS(A))");
    assert_eq!(
        ops,
        vec![c(CoreOp::PushVar(0)), c(CoreOp::Cos), c(CoreOp::Sin),]
    );
}

#[test]
fn test_double_var() {
    let ops = opcodes_without_end("AA+BB");
    assert_eq!(
        ops,
        vec![
            c(CoreOp::PushDoubleVar(0)),
            c(CoreOp::PushDoubleVar(1)),
            c(CoreOp::Add),
        ]
    );
}
