pub mod engine;
pub mod math;

pub use engine::error::{CalcError, calc_error_str};
pub use engine::opcodes::{CoreOp, Opcode};
pub use engine::{CALC_NARGS, CalcResult, CompiledExpr, ExprKind, NumericInputs};

pub use engine::StringInputs;
pub use engine::opcodes::StringOp;
pub use engine::value::StackValue;

pub use engine::ArrayInputs;
pub use engine::array_value::ArrayStackValue;
pub use engine::opcodes::ArrayOp;

/// Compile an infix expression for the **numeric** engine — C `postfix()`
/// (`postfix.c`), used by calc, calcout and swait.
///
/// Symbols outside `postfix.c`'s element table (`SVAL`, string literals and
/// sCalc string functions, aCalc array functions, `UNTIL`, `AA`..`LL`, `>?`,
/// `<?`, `NRNDM`, `INT`) are `CalcError::Syntax` — C's `CALC_ERR_SYNTAX`,
/// raised at compile time (`CLCV != 0`), not at first evaluation.
pub fn compile(expr: &str) -> CalcResult<CompiledExpr> {
    let tokens = engine::token::tokenize(expr, ExprKind::Numeric)?;
    engine::postfix::compile(&tokens, ExprKind::Numeric)
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

/// Compile an infix expression for the **string** engine — synApps
/// `sCalcPostfix()`, used by scalcout. Its element table is its own: no `FMOD`,
/// no `>>>`, operands `A`..`P` / `AA`..`LL`, plus the string symbols base has
/// never had.
pub fn scalc_compile(expr: &str) -> CalcResult<CompiledExpr> {
    let tokens = engine::token::tokenize(expr, ExprKind::String)?;
    engine::postfix::compile(&tokens, ExprKind::String)
}

pub fn scalc_eval(expr: &CompiledExpr, inputs: &mut StringInputs) -> CalcResult<StackValue> {
    engine::string::eval(expr, inputs)
}

pub fn scalc(expr: &str, inputs: &mut StringInputs) -> CalcResult<StackValue> {
    let compiled = scalc_compile(expr)?;
    scalc_eval(&compiled, inputs)
}

/// Compile an infix expression for the **array** engine — synApps
/// `aCalcPostfix()`, used by acalcout. Its element table is its own: no `FMOD`,
/// no `>>>`, no `INF`/`NAN` literal and no `SVAL`, operands `A`..`P` /
/// `AA`..`LL`, plus the array symbols.
pub fn acalc_compile(expr: &str) -> CalcResult<CompiledExpr> {
    let tokens = engine::token::tokenize(expr, ExprKind::Array)?;
    engine::postfix::compile(&tokens, ExprKind::Array)
}

pub fn acalc_eval(expr: &CompiledExpr, inputs: &mut ArrayInputs) -> CalcResult<ArrayStackValue> {
    engine::array::eval(expr, inputs)
}

pub fn acalc(expr: &str, inputs: &mut ArrayInputs) -> CalcResult<ArrayStackValue> {
    let compiled = acalc_compile(expr)?;
    acalc_eval(&compiled, inputs)
}
