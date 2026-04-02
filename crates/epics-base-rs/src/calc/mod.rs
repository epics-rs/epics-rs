pub mod engine;
pub mod math;

pub use engine::error::CalcError;
pub use engine::opcodes::{CoreOp, Opcode};
pub use engine::{CalcResult, CompiledExpr, ExprKind, NumericInputs};

pub use engine::StringInputs;
pub use engine::opcodes::StringOp;
pub use engine::value::StackValue;

pub use engine::ArrayInputs;
pub use engine::array_value::ArrayStackValue;
pub use engine::opcodes::ArrayOp;

/// Compile an infix expression string into a postfix `CompiledExpr`.
pub fn compile(expr: &str) -> CalcResult<CompiledExpr> {
    let tokens = engine::token::tokenize(expr)?;
    engine::postfix::compile(&tokens)
}

/// Evaluate a compiled expression with the given inputs.
pub fn eval(expr: &CompiledExpr, inputs: &mut NumericInputs) -> CalcResult<f64> {
    engine::numeric::eval(expr, inputs)
}

/// Compile and evaluate an expression in one step.
pub fn calc(expr: &str, inputs: &mut NumericInputs) -> CalcResult<f64> {
    let compiled = compile(expr)?;
    eval(&compiled, inputs)
}

pub fn scalc_compile(expr: &str) -> CalcResult<CompiledExpr> {
    let tokens = engine::token::tokenize(expr)?;
    engine::postfix::compile(&tokens)
}

pub fn scalc_eval(expr: &CompiledExpr, inputs: &mut StringInputs) -> CalcResult<StackValue> {
    engine::string::eval(expr, inputs)
}

pub fn scalc(expr: &str, inputs: &mut StringInputs) -> CalcResult<StackValue> {
    let compiled = scalc_compile(expr)?;
    scalc_eval(&compiled, inputs)
}

pub fn acalc_compile(expr: &str) -> CalcResult<CompiledExpr> {
    let tokens = engine::token::tokenize(expr)?;
    engine::postfix::compile(&tokens)
}

pub fn acalc_eval(expr: &CompiledExpr, inputs: &mut ArrayInputs) -> CalcResult<ArrayStackValue> {
    engine::array::eval(expr, inputs)
}

pub fn acalc(expr: &str, inputs: &mut ArrayInputs) -> CalcResult<ArrayStackValue> {
    let compiled = acalc_compile(expr)?;
    acalc_eval(&compiled, inputs)
}
