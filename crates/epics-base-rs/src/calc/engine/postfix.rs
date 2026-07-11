use super::error::CalcError;
use super::opcodes::{CoreOp, Opcode};
use super::token::{ConstName, FuncName, Token};
use super::{CompiledExpr, ExprKind};

// Operator precedence levels.
//
// NOTE: these numeric levels follow the synApps `sCalcPostfix.c` table, NOT
// epics-base `postfix.c` (which uses priorities 0-8 — see postfix.c:147-180).
// Only the *relative* ordering matters for the compiled output, and it is
// preserved for the epics-base operator subset:
//   || < && < cmp < +/- < */% < power < unary/functions
//
//  2: ||, |, OR, XOR
//  3: &&, &, AND, >>, <<
//  4: >?, <?                          (synApps-only)
//  5: ==, !=, <, <=, >, >=, #
//  6: +, -
//  7: *, /, %
//  8: ^, **
//  9/10: unary operators, functions (in_stack=9, in_coming=10)

#[derive(Debug, Clone)]
enum StackEntry {
    Op {
        token: Token,
        in_stack_pri: u8,
    },
    LParen,
    VarargFunc {
        func: FuncName,
        in_stack_pri: u8,
        nargs: u8,
    },
    CondEnd,
    Store {
        var_idx: u8,
        is_double: bool,
    },
}

impl StackEntry {
    fn in_stack_pri(&self) -> u8 {
        match self {
            StackEntry::Op { in_stack_pri, .. } => *in_stack_pri,
            StackEntry::LParen => 0,
            StackEntry::VarargFunc { in_stack_pri, .. } => *in_stack_pri,
            StackEntry::CondEnd => 0,
            StackEntry::Store { .. } => 1,
        }
    }
}

fn binary_op(token: &Token) -> Option<(u8, u8)> {
    match token {
        Token::OrOr | Token::BitOr | Token::OrKeyword | Token::BitXor => Some((2, 2)),
        Token::AndAnd
        | Token::BitAnd
        | Token::AndKeyword
        | Token::Shr
        | Token::ShrLogical
        | Token::Shl => Some((3, 3)),
        Token::MaxOp | Token::MinOp => Some((4, 4)),
        Token::Eq | Token::Ne | Token::Lt | Token::Le | Token::Gt | Token::Ge => Some((5, 5)),
        Token::Plus | Token::Minus => Some((6, 6)),
        Token::Star | Token::Slash | Token::Percent => Some((7, 7)),
        Token::Caret | Token::DoubleStar => Some((8, 8)),
        _ => None,
    }
}

fn token_to_binary_opcode(token: &Token) -> Opcode {
    let core = match token {
        Token::Plus => CoreOp::Add,
        Token::Minus => CoreOp::Sub,
        Token::Star => CoreOp::Mul,
        Token::Slash => CoreOp::Div,
        Token::Percent => CoreOp::Mod,
        Token::Caret | Token::DoubleStar => CoreOp::Power,
        Token::Eq => CoreOp::Eq,
        Token::Ne => CoreOp::Ne,
        Token::Lt => CoreOp::Lt,
        Token::Le => CoreOp::Le,
        Token::Gt => CoreOp::Gt,
        Token::Ge => CoreOp::Ge,
        Token::AndAnd | Token::AndKeyword => CoreOp::And,
        Token::OrOr | Token::OrKeyword => CoreOp::Or,
        Token::BitAnd => CoreOp::BitAnd,
        Token::BitOr => CoreOp::BitOr,
        Token::BitXor => CoreOp::BitXor,
        Token::Shl => CoreOp::Shl,
        Token::Shr => CoreOp::Shr,
        Token::ShrLogical => CoreOp::ShrLogical,
        Token::MaxOp => CoreOp::MaxVal,
        Token::MinOp => CoreOp::MinVal,
        Token::PipeMinus => {
            return Opcode::String(super::opcodes::StringOp::SubLast);
        }
        _ => unreachable!(),
    };
    Opcode::Core(core)
}

fn is_vararg(func: &FuncName) -> bool {
    matches!(
        func,
        FuncName::Min | FuncName::Max | FuncName::Finite | FuncName::IsNan
    )
}

/// Number of operands consumed by a *non-vararg* function.
///
/// Every function emits exactly one result, so its net runtime-stack
/// effect is `1 - func_arity(f)`. Vararg functions (`MIN`, `MAX`,
/// `FINITE`, `ISNAN`) never reach this path — they are tracked through
/// `StackEntry::VarargFunc` with an explicit `nargs` count.
///
/// The arity values are verified against the opcode implementations:
///   - core 1-arg math: `numeric.rs` (`Abs`..`Nint`, `IsInf`, `BitNot`)
///   - core 2-arg math: `numeric.rs` (`Atan2`, `Fmod`)
///   - string ops: `string.rs` (`ToString`..`Xor8Append`, `Printf`,
///     `Sscanf`)
///   - array ops: `array.rs` (`ConstIndex`..`FitMQ`)
fn func_arity(func: &FuncName) -> u8 {
    match func {
        // 0-arg: produce a value from ambient state, consume nothing.
        //   ARndm  -> ArrayOp::ArrayRandom  (array.rs: pushes only)
        //   Ix     -> ArrayOp::ConstIndex   (array.rs: pushes only)
        FuncName::ARndm | FuncName::Ix => 0,

        // 2-arg core math (numeric.rs: Atan2/Fmod pop2).
        FuncName::Atan2 | FuncName::Fmod => 2,

        // 2-arg string ops (string.rs: Printf/Sscanf pop2).
        FuncName::Printf | FuncName::Sscanf => 2,

        // 2-arg array ops.
        //   NSmoo  -> ArrayOp::NSmooth   (array.rs:527 — pop n, pop array)
        //   NDeriv -> ArrayOp::NDeriv    (array.rs:538 — pop n, pop array)
        //   Cat    -> ArrayOp::Cat       (array.rs:553 — pop b, pop a)
        //   FitPoly-> ArrayOp::FitPoly   (array.rs:592 — pop y, pop x)
        //   FitQ   -> ArrayOp::FitQ      (array.rs:611 — pop y, pop x)
        FuncName::NSmoo | FuncName::NDeriv | FuncName::Cat | FuncName::FitPoly | FuncName::FitQ => {
            2
        }

        // 3-arg array ops.
        //   FitMPoly -> ArrayOp::FitMPoly (array.rs:601 — pop mask, y, x)
        //   FitMQ    -> ArrayOp::FitMQ    (array.rs:629 — pop mask, y, x)
        FuncName::FitMPoly | FuncName::FitMQ => 3,

        // Everything else is a 1-arg function: one operand in, one out.
        //   core math: Abs, Sqrt, Sqr, Exp, Log10, LogE, Ln, Sin,
        //     Cos, Tan, Asin, Acos, Atan, Sinh, Cosh, Tanh, Ceil, Floor,
        //     Nint, Int, IsInf, Not
        //   string 1-arg: Dbl, Str, Len, Byte, TrEsc, Esc, BinRead,
        //     BinWrite, Crc16, ModBus, Lrc, AModBus, Xor8, AddXor8
        //   array 1-arg: Avg, Std, FwhmFunc, Sum, AMax, AMin, IxMax,
        //     IxMin, IxZ, IxNz, Arr, AToD, Smoo, Deriv, Cum
        // Vararg funcs (Min, Max, Finite, IsNan) are unreachable here.
        _ => 1,
    }
}

/// True when a function takes no operands and therefore behaves like an
/// operand itself (the next token must be an operator, not an operand).
fn is_nullary_func(func: &FuncName) -> bool {
    func_arity(func) == 0
}

fn func_to_opcode(func: &FuncName, nargs: u8) -> Opcode {
    let core = match func {
        FuncName::Abs => CoreOp::Abs,
        FuncName::Sqrt | FuncName::Sqr => CoreOp::Sqrt,
        FuncName::Exp => CoreOp::Exp,
        FuncName::Log10 => CoreOp::Log10,
        FuncName::LogE | FuncName::Ln => CoreOp::LogE,
        FuncName::Sin => CoreOp::Sin,
        FuncName::Cos => CoreOp::Cos,
        FuncName::Tan => CoreOp::Tan,
        FuncName::Asin => CoreOp::Asin,
        FuncName::Acos => CoreOp::Acos,
        FuncName::Atan => CoreOp::Atan,
        FuncName::Atan2 => CoreOp::Atan2,
        FuncName::Fmod => CoreOp::Fmod,
        FuncName::Sinh => CoreOp::Sinh,
        FuncName::Cosh => CoreOp::Cosh,
        FuncName::Tanh => CoreOp::Tanh,
        FuncName::Ceil => CoreOp::Ceil,
        FuncName::Floor => CoreOp::Floor,
        FuncName::Nint | FuncName::Int => CoreOp::Nint,
        FuncName::IsNan => CoreOp::IsNan(nargs),
        FuncName::IsInf => CoreOp::IsInf,
        FuncName::Finite => CoreOp::Finite(nargs),
        FuncName::Max => CoreOp::Max(nargs),
        FuncName::Min => CoreOp::Min(nargs),
        FuncName::Not => CoreOp::BitNot,
        FuncName::Dbl => return Opcode::String(super::opcodes::StringOp::ToDouble),
        FuncName::Str => return Opcode::String(super::opcodes::StringOp::ToString),
        FuncName::Len => return Opcode::String(super::opcodes::StringOp::Len),
        FuncName::Byte => return Opcode::String(super::opcodes::StringOp::Byte),
        FuncName::TrEsc => return Opcode::String(super::opcodes::StringOp::TrEsc),
        FuncName::Esc => return Opcode::String(super::opcodes::StringOp::Esc),
        FuncName::Printf => return Opcode::String(super::opcodes::StringOp::Printf),
        FuncName::Sscanf => return Opcode::String(super::opcodes::StringOp::Sscanf),
        FuncName::BinRead => return Opcode::String(super::opcodes::StringOp::BinRead),
        FuncName::BinWrite => return Opcode::String(super::opcodes::StringOp::BinWrite),
        FuncName::Crc16 => return Opcode::String(super::opcodes::StringOp::Crc16),
        FuncName::ModBus => return Opcode::String(super::opcodes::StringOp::Crc16Append),
        FuncName::Lrc => return Opcode::String(super::opcodes::StringOp::Lrc),
        FuncName::AModBus => return Opcode::String(super::opcodes::StringOp::LrcAppend),
        FuncName::Xor8 => return Opcode::String(super::opcodes::StringOp::Xor8),
        FuncName::AddXor8 => return Opcode::String(super::opcodes::StringOp::Xor8Append),
        FuncName::Avg => return Opcode::Array(super::opcodes::ArrayOp::Average),
        FuncName::Std => return Opcode::Array(super::opcodes::ArrayOp::StdDev),
        FuncName::FwhmFunc => return Opcode::Array(super::opcodes::ArrayOp::Fwhm),
        FuncName::Sum => return Opcode::Array(super::opcodes::ArrayOp::ArraySum),
        FuncName::AMax => return Opcode::Array(super::opcodes::ArrayOp::ArrayMax),
        FuncName::AMin => return Opcode::Array(super::opcodes::ArrayOp::ArrayMin),
        FuncName::IxMax => return Opcode::Array(super::opcodes::ArrayOp::IndexMax),
        FuncName::IxMin => return Opcode::Array(super::opcodes::ArrayOp::IndexMin),
        FuncName::IxZ => return Opcode::Array(super::opcodes::ArrayOp::IndexZero),
        FuncName::IxNz => return Opcode::Array(super::opcodes::ArrayOp::IndexNonZero),
        FuncName::Arr => return Opcode::Array(super::opcodes::ArrayOp::ToArray),
        FuncName::Ix => return Opcode::Array(super::opcodes::ArrayOp::ConstIndex),
        FuncName::AToD => return Opcode::Array(super::opcodes::ArrayOp::ToDouble),
        FuncName::Smoo => return Opcode::Array(super::opcodes::ArrayOp::Smooth),
        FuncName::NSmoo => return Opcode::Array(super::opcodes::ArrayOp::NSmooth),
        FuncName::Deriv => return Opcode::Array(super::opcodes::ArrayOp::Deriv),
        FuncName::NDeriv => return Opcode::Array(super::opcodes::ArrayOp::NDeriv),
        FuncName::FitPoly => return Opcode::Array(super::opcodes::ArrayOp::FitPoly),
        FuncName::FitMPoly => return Opcode::Array(super::opcodes::ArrayOp::FitMPoly),
        FuncName::FitQ => return Opcode::Array(super::opcodes::ArrayOp::FitQ),
        FuncName::FitMQ => return Opcode::Array(super::opcodes::ArrayOp::FitMQ),
        FuncName::Cum => return Opcode::Array(super::opcodes::ArrayOp::Cum),
        FuncName::Cat => return Opcode::Array(super::opcodes::ArrayOp::Cat),
        FuncName::ARndm => return Opcode::Array(super::opcodes::ArrayOp::ArrayRandom),
    };
    Opcode::Core(core)
}

fn flush_stack_entry(entry: &StackEntry, output: &mut Vec<Opcode>) {
    match entry {
        StackEntry::Op {
            token: Token::Minus,
            in_stack_pri: 9,
            ..
        } => {
            output.push(Opcode::Core(CoreOp::Neg));
        }
        StackEntry::Op {
            token: Token::Bang, ..
        } => {
            output.push(Opcode::Core(CoreOp::Not));
        }
        StackEntry::Op {
            token: Token::Tilde,
            ..
        } => {
            output.push(Opcode::Core(CoreOp::BitNot));
        }
        StackEntry::Op {
            token: Token::Func(f),
            ..
        } => {
            output.push(func_to_opcode(f, 1));
        }
        StackEntry::Op { token, .. } => {
            output.push(token_to_binary_opcode(token));
        }
        StackEntry::VarargFunc { func, nargs, .. } => {
            output.push(func_to_opcode(func, *nargs));
        }
        StackEntry::CondEnd => {
            output.push(Opcode::Core(CoreOp::CondEnd));
        }
        StackEntry::Store { var_idx, is_double } => {
            if *is_double {
                output.push(Opcode::Core(CoreOp::StoreDoubleVar(*var_idx)));
            } else {
                output.push(Opcode::Core(CoreOp::StoreVar(*var_idx)));
            }
        }
        StackEntry::LParen => {}
    }
}

/// Is `tok` a symbol of base `postfix.c`'s ELEMENT table (postfix.c:73-179)?
///
/// C has three *separate* compilers, each with its own `ELEMENT` table:
/// `postfix.c` (calc/calcout/swait), `sCalcPostfix.c` (sCalc) and
/// `aCalcPostfix.c` (aCalc). The table IS the lexer: `get_element` looks a
/// symbol up in it, and a symbol that is not there is never lexed. The infix
/// text is then left unconsumed and `postfix()` returns `CALC_ERR_SYNTAX` — a
/// COMPILE error (`CLCV != 0`, reported at record init / CALC-field put), not a
/// runtime one.
///
/// The Rust port shares ONE tokenizer across the three engines, so the table
/// difference has to be reapplied. It is reapplied *here*, on the token stream,
/// because that is the only level at which it survives: sCalc and aCalc map
/// `INT` and `NINT` to the very same `NINT` opcode (sCalcPostfix.c:150,
/// aCalcPostfix.c:152), so by the time the stream is opcodes there is nothing
/// left to distinguish base's `NINT` (in its table) from `INT` (not in it).
///
/// This is an ALLOWLIST, not a list of exceptions: a new token in the shared
/// tokenizer is refused by the numeric engine until someone shows it in the C
/// table. The old exception-list shape is exactly what let `INT` and `LOG2`
/// through the numeric engine — everything not named was accepted.
///
/// Verified against the real compiler: `postfix.c` + `calcPerform.c` built
/// standalone from epics-base and asked. Every symbol below compiles; every
/// symbol in the false arms answers `CALC_ERR_SYNTAX` (11).
fn token_in_base_table(tok: &Token) -> bool {
    match tok {
        // Literals and operands: "." "0".."9" "0X" INF NAN (LITERAL_OPERAND),
        // "A".."U" (OPERAND FETCH_A..FETCH_U — CALCPERFORM_NARGS is 21),
        // RNDM, VAL, PI, D2R, R2D.
        Token::Number(_) | Token::Var(_) | Token::Rndm | Token::FetchVal | Token::Const(_) => true,

        // Operators, all present in postfix.c:74-179.
        Token::Plus
        | Token::Minus
        | Token::Star
        | Token::Slash
        | Token::Percent
        | Token::Caret
        | Token::DoubleStar
        | Token::Eq
        | Token::Ne
        | Token::Lt
        | Token::Le
        | Token::Gt
        | Token::Ge
        | Token::AndAnd
        | Token::OrOr
        | Token::BitAnd
        | Token::BitOr
        | Token::BitXor
        | Token::Tilde
        | Token::Shl
        | Token::Shr
        | Token::ShrLogical
        | Token::Bang
        | Token::Question
        | Token::Colon
        | Token::LParen
        | Token::RParen
        | Token::Comma
        | Token::Semicolon
        | Token::Assign
        | Token::AndKeyword
        | Token::OrKeyword => true,

        Token::Func(f) => func_in_base_table(f),

        // NOT in postfix.c's table, each one a synApps sCalc/aCalc extension:
        //   AA..UU  — sCalc string args / aCalc array args (base has single
        //             letters only)
        //   SVAL    — sCalcPostfix.c:188; pushes `psresult`, which numeric
        //             `calcPerform` does not even take
        //   NRNDM, `>?`, `<?` — synApps operator-table extensions
        //   UNTIL   — sCalc/aCalc loop keyword
        //   string literals, `[i:j]`, `{find,replace}`, `|-` — sCalc/aCalc
        Token::DoubleVar(_)
        | Token::FetchSval
        | Token::Nrndm
        | Token::MaxOp
        | Token::MinOp
        | Token::UntilKeyword
        | Token::StringLiteral(_)
        | Token::LBracket
        | Token::RBracket
        | Token::LBrace
        | Token::RBrace
        | Token::PipeMinus => false,
    }
}

/// The function/operator symbols of base `postfix.c`'s table (postfix.c:90-143).
fn func_in_base_table(f: &FuncName) -> bool {
    match f {
        FuncName::Abs
        | FuncName::Sqrt
        | FuncName::Sqr
        | FuncName::Exp
        | FuncName::Log10
        | FuncName::LogE
        | FuncName::Ln
        | FuncName::Sin
        | FuncName::Cos
        | FuncName::Tan
        | FuncName::Asin
        | FuncName::Acos
        | FuncName::Atan
        | FuncName::Atan2
        | FuncName::Fmod
        | FuncName::Sinh
        | FuncName::Cosh
        | FuncName::Tanh
        | FuncName::Ceil
        | FuncName::Floor
        | FuncName::Nint
        | FuncName::IsNan
        | FuncName::IsInf
        | FuncName::Finite
        | FuncName::Max
        | FuncName::Min
        | FuncName::Not => true,

        // `INT` is sCalc/aCalc only (sCalcPostfix.c:150, aCalcPostfix.c:152 —
        // where it is an alias of NINT, i.e. it ROUNDS). base's lexer splits
        // `INT(A)` into the operands I, N, T and answers CALC_ERR_SYNTAX.
        FuncName::Int => false,

        // Everything else is a string (sCalc) or array (aCalc) function.
        _ => false,
    }
}

/// Is `op` an opcode the C compiler for `kind` can emit?
///
/// The token allowlist above owns base's table. This owns the sCalc/aCalc
/// split, which is only visible once an opcode has been chosen: `[` is a
/// substring in sCalc and an array index in aCalc, so the two engines share
/// tokens but never share ops.
fn opcode_in_grammar(kind: &ExprKind, op: &Opcode) -> bool {
    match op {
        // String ops (string literals, STR/DBL/LEN/BYTE/PRINTF/SSCANF/ESC/
        // CRC16/…, `[i:j]`, `{find,replace}`, `|-`) exist only in
        // sCalcPostfix.c's table.
        Opcode::String(_) => matches!(kind, ExprKind::String),
        // Array ops (IX/ARR/AVG/SUM/DERIV/FITPOLY/…) only in aCalcPostfix.c.
        Opcode::Array(_) => matches!(kind, ExprKind::Array),
        // `SVAL` (FETCH_SVAL) is sCalcPostfix.c:188 alone — it pushes the
        // previous *string* result, which neither `calcPerform` nor
        // `aCalcPerform` even takes. aCalc's table has no such element.
        Opcode::Core(CoreOp::FetchSval) => matches!(kind, ExprKind::String),
        Opcode::Control(_) | Opcode::Core(_) => true,
    }
}

/// Compile a token stream for ONE engine.
///
/// `kind` selects the C compiler being emulated (`postfix` / `sCalcPostfix` /
/// `aCalcPostfix`) and is enforced, not inferred: a token outside that
/// engine's grammar is rejected here with `CalcError::Syntax`, C's
/// `CALC_ERR_SYNTAX`. The resulting `CompiledExpr` therefore carries only
/// opcodes its evaluator can execute — the previous shared-table compile let
/// `SVAL` or `PRINTF` into a numeric `calc.CALC` and failed at first process
/// with `CalcError::Internal` instead of at load.
pub fn compile(tokens: &[Token], kind: ExprKind) -> Result<CompiledExpr, CalcError> {
    // C's `get_element` fails on a symbol that is not in this compiler's table,
    // before any parsing happens. Base's table is the strict subset, so this is
    // where the numeric engine refuses `INT`, `SVAL`, `AA`, `UNTIL`, `>?` …
    if matches!(kind, ExprKind::Numeric) && !tokens.iter().all(token_in_base_table) {
        return Err(CalcError::Syntax);
    }

    if tokens.is_empty() {
        return Ok(CompiledExpr {
            code: vec![Opcode::Core(CoreOp::End)],
            kind,
            loop_pairs: Vec::new(),
        });
    }

    let mut output: Vec<Opcode> = Vec::new();
    let mut stack: Vec<StackEntry> = Vec::new();
    let mut operand_needed = true;
    let mut runtime_depth: i32 = 0;
    let mut cond_count: i32 = 0;
    let mut pos = 0;
    let mut bracket_depth: i32 = 0;
    let mut brace_depth: i32 = 0;
    let mut until_stack: Vec<usize> = Vec::new();

    while pos < tokens.len() {
        let token = &tokens[pos];
        pos += 1;

        if operand_needed {
            match token {
                Token::Number(v) => {
                    output.push(Opcode::Core(CoreOp::PushConst(*v)));
                    runtime_depth += 1;
                    operand_needed = false;
                }
                Token::Var(idx) => {
                    output.push(Opcode::Core(CoreOp::PushVar(*idx)));
                    runtime_depth += 1;
                    operand_needed = false;
                }
                Token::DoubleVar(idx) => {
                    output.push(Opcode::Core(CoreOp::PushDoubleVar(*idx)));
                    runtime_depth += 1;
                    operand_needed = false;
                }
                Token::FetchVal => {
                    output.push(Opcode::Core(CoreOp::FetchVal));
                    runtime_depth += 1;
                    operand_needed = false;
                }
                Token::FetchSval => {
                    output.push(Opcode::Core(CoreOp::FetchSval));
                    runtime_depth += 1;
                    operand_needed = false;
                    // C sCalcPostfix.c:452 lists FETCH_SVAL among the opcodes
                    // that mark the postfix USES_STRING.
                }
                Token::Rndm => {
                    output.push(Opcode::Core(CoreOp::Random));
                    runtime_depth += 1;
                    operand_needed = false;
                }
                Token::Nrndm => {
                    output.push(Opcode::Core(CoreOp::NormalRandom));
                    runtime_depth += 1;
                    operand_needed = false;
                }
                Token::Const(c) => {
                    match c {
                        ConstName::Pi => output.push(Opcode::Core(CoreOp::Pi)),
                        ConstName::D2R => output.push(Opcode::Core(CoreOp::D2R)),
                        ConstName::R2D => output.push(Opcode::Core(CoreOp::R2D)),
                    }
                    runtime_depth += 1;
                    operand_needed = false;
                }

                Token::StringLiteral(s) => {
                    output.push(Opcode::String(super::opcodes::StringOp::PushString(
                        s.clone(),
                    )));
                    runtime_depth += 1;
                    operand_needed = false;
                }

                // Unary operators
                Token::Minus => {
                    pop_higher_or_equal(&mut stack, 10, &mut output, &mut runtime_depth);
                    stack.push(StackEntry::Op {
                        token: Token::Minus,
                        in_stack_pri: 9,
                    });
                }
                Token::Bang => {
                    pop_higher_or_equal(&mut stack, 10, &mut output, &mut runtime_depth);
                    stack.push(StackEntry::Op {
                        token: Token::Bang,
                        in_stack_pri: 9,
                    });
                }
                Token::Tilde => {
                    pop_higher_or_equal(&mut stack, 10, &mut output, &mut runtime_depth);
                    stack.push(StackEntry::Op {
                        token: Token::Tilde,
                        in_stack_pri: 9,
                    });
                }

                Token::LParen => {
                    stack.push(StackEntry::LParen);
                }

                Token::UntilKeyword => {
                    // UNTIL marks the start of a loop.
                    // Record the current output position as the loop start.
                    // Emit placeholder Until opcode (will be patched).
                    let until_pc = output.len();
                    output.push(Opcode::Control(
                        super::opcodes::ControlOp::Until(0), // placeholder
                    ));
                    until_stack.push(until_pc);
                    // operand_needed remains true (body follows)
                }

                Token::Func(func) => {
                    pop_higher_or_equal(&mut stack, 10, &mut output, &mut runtime_depth);
                    if is_vararg(func) {
                        stack.push(StackEntry::VarargFunc {
                            func: func.clone(),
                            in_stack_pri: 9,
                            nargs: 1,
                        });
                    } else {
                        stack.push(StackEntry::Op {
                            token: token.clone(),
                            in_stack_pri: 9,
                        });
                    }
                    // A nullary function (ARNDM, IX) supplies its own
                    // operand: it consumes nothing and pushes one value.
                    // The next token must therefore be an operator, like
                    // any other operand. Functions with arity >= 1 still
                    // need an operand to follow.
                    if !is_vararg(func) && is_nullary_func(func) {
                        operand_needed = false;
                    }
                }

                _ => return Err(CalcError::Syntax),
            }
        } else {
            match token {
                t if binary_op(t).is_some() => {
                    let (isp, icp) = binary_op(t).unwrap();
                    pop_higher_or_equal(&mut stack, icp, &mut output, &mut runtime_depth);
                    stack.push(StackEntry::Op {
                        token: t.clone(),
                        in_stack_pri: isp,
                    });
                    operand_needed = true;
                }

                Token::RParen => {
                    loop {
                        match stack.last() {
                            None => return Err(CalcError::ParenNotOpen),
                            Some(StackEntry::LParen) => {
                                stack.pop();
                                break;
                            }
                            _ => {
                                let entry = stack.pop().unwrap();
                                runtime_depth += stack_effect(&entry);
                                flush_stack_entry(&entry, &mut output);
                            }
                        }
                    }
                    if let Some(StackEntry::VarargFunc { .. }) = stack.last() {
                        let entry = stack.pop().unwrap();
                        runtime_depth += stack_effect(&entry);
                        flush_stack_entry(&entry, &mut output);
                    } else if let Some(StackEntry::Op {
                        token: Token::Func(_),
                        ..
                    }) = stack.last()
                    {
                        let entry = stack.pop().unwrap();
                        runtime_depth += stack_effect(&entry);
                        flush_stack_entry(&entry, &mut output);
                    }
                }

                Token::Comma => {
                    loop {
                        match stack.last() {
                            None => return Err(CalcError::BadSeparator),
                            Some(StackEntry::LParen) => break,
                            _ => {
                                let entry = stack.pop().unwrap();
                                runtime_depth += stack_effect(&entry);
                                flush_stack_entry(&entry, &mut output);
                            }
                        }
                    }
                    let lparen_idx = stack.len() - 1;
                    if lparen_idx > 0 {
                        if let StackEntry::VarargFunc { nargs, .. } = &mut stack[lparen_idx - 1] {
                            *nargs += 1;
                        }
                    }
                    operand_needed = true;
                }

                Token::Question => {
                    pop_higher_strict(&mut stack, 0, &mut output, &mut runtime_depth);
                    output.push(Opcode::Core(CoreOp::CondIf));
                    runtime_depth -= 1;
                    cond_count += 1;
                    operand_needed = true;
                }

                Token::Colon => {
                    pop_higher_strict(&mut stack, 0, &mut output, &mut runtime_depth);
                    output.push(Opcode::Core(CoreOp::CondElse));
                    runtime_depth -= 1;
                    cond_count -= 1;
                    if cond_count < 0 {
                        return Err(CalcError::Conditional);
                    }
                    stack.push(StackEntry::CondEnd);
                    operand_needed = true;
                }

                Token::Semicolon => {
                    while let Some(entry) = stack.last() {
                        if matches!(entry, StackEntry::LParen) {
                            break;
                        }
                        let entry = stack.pop().unwrap();
                        runtime_depth += stack_effect(&entry);
                        flush_stack_entry(&entry, &mut output);
                    }
                    // If there's a pending UNTIL, close it.
                    if let Some(until_pc) = until_stack.pop() {
                        let end_pc = output.len();
                        output.push(Opcode::Control(super::opcodes::ControlOp::UntilEnd(
                            until_pc,
                        )));
                        // Patch the Until opcode with the end_pc
                        output[until_pc] =
                            Opcode::Control(super::opcodes::ControlOp::Until(end_pc));
                        // Runtime effect: the `Until` marker is a no-op (0),
                        // but `UntilEnd` pops the loop condition (see
                        // string.rs evaluator: `pop1_f64`) and pushes
                        // nothing — a net runtime depth delta of -1.
                        runtime_depth -= 1;
                    }
                    // C postfix.c:452-455 — at a `;` terminator the net runtime
                    // depth must not exceed 1.
                    if cond_count != 0 {
                        return Err(CalcError::Conditional);
                    }
                    if runtime_depth > 1 {
                        return Err(CalcError::TooMany);
                    }
                    operand_needed = true;
                }

                Token::Assign => {
                    match output.last() {
                        Some(Opcode::Core(CoreOp::PushVar(idx))) => {
                            let idx = *idx;
                            output.pop();
                            runtime_depth -= 1;
                            while let Some(entry) = stack.last() {
                                if matches!(entry, StackEntry::LParen) {
                                    break;
                                }
                                if entry.in_stack_pri() >= 1 {
                                    let entry = stack.pop().unwrap();
                                    runtime_depth += stack_effect(&entry);
                                    flush_stack_entry(&entry, &mut output);
                                } else {
                                    break;
                                }
                            }
                            stack.push(StackEntry::Store {
                                var_idx: idx,
                                is_double: false,
                            });
                        }
                        Some(Opcode::Core(CoreOp::PushDoubleVar(idx))) => {
                            let idx = *idx;
                            output.pop();
                            runtime_depth -= 1;
                            stack.push(StackEntry::Store {
                                var_idx: idx,
                                is_double: true,
                            });
                        }
                        _ => return Err(CalcError::BadAssignment),
                    }
                    operand_needed = true;
                }

                // Bracket subrange: expr[start,end] → Subrange
                Token::LBracket => {
                    // Flush pending operators
                    pop_higher_or_equal(&mut stack, 11, &mut output, &mut runtime_depth);
                    stack.push(StackEntry::LParen); // reuse LParen mechanics
                    operand_needed = true;
                    // Mark that we need to emit Subrange on RBracket
                    bracket_depth += 1;
                }

                // Brace replace: expr{find,replace} → Replace
                Token::LBrace => {
                    pop_higher_or_equal(&mut stack, 11, &mut output, &mut runtime_depth);
                    stack.push(StackEntry::LParen);
                    operand_needed = true;
                    brace_depth += 1;
                }

                Token::RBracket => {
                    if bracket_depth == 0 {
                        return Err(CalcError::BracketNotOpen);
                    }
                    bracket_depth -= 1;
                    // Pop until matching LParen
                    loop {
                        match stack.last() {
                            None => return Err(CalcError::BracketNotOpen),
                            Some(StackEntry::LParen) => {
                                stack.pop();
                                break;
                            }
                            _ => {
                                let entry = stack.pop().unwrap();
                                runtime_depth += stack_effect(&entry);
                                flush_stack_entry(&entry, &mut output);
                            }
                        }
                    }
                    output.push(Opcode::String(super::opcodes::StringOp::Subrange));
                    runtime_depth -= 2; // consumes string + 2 args, pushes 1
                }

                Token::RBrace => {
                    if brace_depth == 0 {
                        return Err(CalcError::BraceNotOpen);
                    }
                    brace_depth -= 1;
                    loop {
                        match stack.last() {
                            None => return Err(CalcError::BraceNotOpen),
                            Some(StackEntry::LParen) => {
                                stack.pop();
                                break;
                            }
                            _ => {
                                let entry = stack.pop().unwrap();
                                runtime_depth += stack_effect(&entry);
                                flush_stack_entry(&entry, &mut output);
                            }
                        }
                    }
                    output.push(Opcode::String(super::opcodes::StringOp::Replace));
                    runtime_depth -= 2; // consumes string + 2 args, pushes 1
                }

                Token::PipeMinus => {
                    pop_higher_or_equal(&mut stack, 6, &mut output, &mut runtime_depth);
                    stack.push(StackEntry::Op {
                        token: Token::PipeMinus,
                        in_stack_pri: 6,
                    });
                    operand_needed = true;
                }

                _ => return Err(CalcError::Syntax),
            }
        }

        if runtime_depth < 0 {
            return Err(CalcError::Underflow);
        }
        if runtime_depth >= 30 {
            return Err(CalcError::Overflow);
        }
    }

    // C postfix.c flushes the residual operator stack to the output and
    // accumulates their runtime effect before the final well-formedness check.
    while let Some(entry) = stack.pop() {
        match entry {
            StackEntry::LParen => return Err(CalcError::ParenOpen),
            _ => {
                runtime_depth += stack_effect(&entry);
                flush_stack_entry(&entry, &mut output);
            }
        }
    }

    if cond_count != 0 {
        return Err(CalcError::Conditional);
    }

    // End-of-expression well-formedness.
    //
    // epics-base `postfix.c:499-502` requires a net runtime depth of exactly
    // 1 (one value left to fetch). This Rust engine is a synApps superset:
    // sCalc/aCalc store-assignment (`A:=5`, `BB:=AA`, `AA:="x"`) is a valid
    // side-effect-only construct that legitimately terminates with the
    // runtime stack at depth 0. The strict `== 1` rule therefore cannot be
    // applied globally. (`UNTIL <cond>; <body>` value-producing forms still
    // end at depth 1 — the `UntilEnd` opcode consumes the loop condition.)
    //
    // The rule:
    //   - depth 1                       -> value-producing expression, OK.
    //   - depth 0 AND the final emitted
    //     opcode is a store             -> store-terminated expression, OK.
    //   - depth 0 otherwise             -> an operand was consumed without a
    //                                      result (e.g. `CAT(AA)` with too
    //                                      few args) -> Incomplete.
    //   - depth > 1                     -> residual values (`1 2`, `A;B`)
    //                                      -> Incomplete (TooMany already
    //                                      fired at any `;`).
    // The empty-program case (`output` is empty before the End sentinel) is
    // a deliberate Rust special case and is exempt.
    let ends_with_store = matches!(
        output.last(),
        Some(Opcode::Core(CoreOp::StoreVar(_))) | Some(Opcode::Core(CoreOp::StoreDoubleVar(_)))
    );
    let depth_ok = runtime_depth == 1 || (runtime_depth == 0 && ends_with_store);
    if !output.is_empty() && (operand_needed || !depth_ok) {
        return Err(CalcError::Incomplete);
    }

    output.push(Opcode::Core(CoreOp::End));

    // C's per-engine ELEMENT table, reapplied to the shared tokenizer's output:
    // a token this engine's C compiler cannot lex is a compile error, not a
    // runtime one.
    if output.iter().any(|op| !opcode_in_grammar(&kind, op)) {
        return Err(CalcError::Syntax);
    }

    Ok(CompiledExpr {
        code: output,
        kind,
        loop_pairs: Vec::new(),
    })
}

fn stack_effect(entry: &StackEntry) -> i32 {
    match entry {
        StackEntry::Op {
            token: Token::Minus,
            in_stack_pri: 9,
            ..
        } => 0,
        StackEntry::Op {
            token: Token::Bang, ..
        } => 0,
        StackEntry::Op {
            token: Token::Tilde,
            ..
        } => 0,
        StackEntry::Op {
            token: Token::Func(f),
            ..
        } => {
            // A non-vararg function consumes `func_arity(f)` operands and
            // pushes exactly one result.
            1 - func_arity(f) as i32
        }
        StackEntry::Op { .. } => -1,
        StackEntry::VarargFunc { nargs, .. } => 1 - (*nargs as i32),
        StackEntry::CondEnd => 0,
        StackEntry::Store { .. } => -1,
        StackEntry::LParen => 0,
    }
}

fn pop_higher_or_equal(
    stack: &mut Vec<StackEntry>,
    incoming_pri: u8,
    output: &mut Vec<Opcode>,
    runtime_depth: &mut i32,
) {
    while let Some(entry) = stack.last() {
        if matches!(entry, StackEntry::LParen) {
            break;
        }
        if entry.in_stack_pri() >= incoming_pri {
            let entry = stack.pop().unwrap();
            *runtime_depth += stack_effect(&entry);
            flush_stack_entry(&entry, output);
        } else {
            break;
        }
    }
}

fn pop_higher_strict(
    stack: &mut Vec<StackEntry>,
    incoming_pri: u8,
    output: &mut Vec<Opcode>,
    runtime_depth: &mut i32,
) {
    while let Some(entry) = stack.last() {
        if matches!(entry, StackEntry::LParen) {
            break;
        }
        if entry.in_stack_pri() > incoming_pri {
            let entry = stack.pop().unwrap();
            *runtime_depth += stack_effect(&entry);
            flush_stack_entry(&entry, output);
        } else {
            break;
        }
    }
}
