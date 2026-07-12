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

        // 2-arg string ops (string.rs: Printf/Sscanf/BinRead/BinWrite pop2).
        // C's element table settles this: `runtime_effect` is -1 for PRINTF
        // ($P), SSCANF ($S), BIN_READ ($R, READ) and BIN_WRITE ($W, WRITE)
        // alike (sCalcPostfix.c:173-195) — one net operand consumed, i.e. two
        // popped and one pushed. Only elements with `runtime_effect` 0 are
        // 1-in-1-out.
        FuncName::Printf | FuncName::Sscanf | FuncName::BinRead | FuncName::BinWrite => 2,

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
        //   string 1-arg: Dbl, Str, Len, Byte, TrEsc, Esc, Crc16, ModBus,
        //     Lrc, AModBus, Xor8, AddXor8
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
        FuncName::ANeg => return Opcode::Array(super::opcodes::ArrayOp::ANeg),
        FuncName::APos => return Opcode::Array(super::opcodes::ArrayOp::APos),
        FuncName::DynFetch => return Opcode::Array(super::opcodes::ArrayOp::DynFetch),
        FuncName::DynAFetch => return Opcode::Array(super::opcodes::ArrayOp::DynAFetch),
        FuncName::ALenNoop => return Opcode::Array(super::opcodes::ArrayOp::LenNoop),
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

/// Compile a token stream for ONE engine.
///
/// `tokens` must come from `token::tokenize(_, kind)`: that is where the C
/// compiler's `ELEMENT` table is applied, so every symbol here is one this
/// engine has. The resulting `CompiledExpr` carries only opcodes its evaluator
/// can execute.
/// Compile a token stream. A source with no tokens is NOT special-cased: C only
/// short-circuits the literal empty string (`*psrc == '\0'`), and an expression
/// that merely *lexes* to nothing — `"   "` — walks the normal path and comes
/// out `CALC_ERR_INCOMPLETE`, because `operand_needed` is still set at the end.
/// Compiled C, all three engines: `postfix("   ")` = -1, error 8. The empty
/// SOURCE is handled by each engine's entry point in `calc::mod`, where C puts
/// it.
pub fn compile(tokens: &[Token], kind: ExprKind) -> Result<CompiledExpr, CalcError> {
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
                Token::FetchAval => {
                    output.push(Opcode::Array(super::opcodes::ArrayOp::FetchAval));
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
                        ConstName::S2R => output.push(Opcode::Core(CoreOp::S2R)),
                        ConstName::R2S => output.push(Opcode::Core(CoreOp::R2S)),
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
                        // Runtime effect ZERO, both for `Until` and for
                        // `UntilEnd` — C pushes the UNTIL_END element with an
                        // explicit `runtime_effect = 0` (`sCalcPostfix.c:782`)
                        // because UNTIL_END only PEEKS the condition
                        // (`sCalcPerform.c:1999`, `ps->d == 0`). On the way out
                        // of the loop the condition value stays on the stack —
                        // it is what the expression evaluates to.
                        //
                        // The port subtracted 1 here, on the theory that the
                        // condition is consumed. It is not, and the -1 is what
                        // made C's own `UNTIL A; A:=A+1` look store-terminated.
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

                // C `STORE_OPERATOR` (`sCalcPostfix.c:539-565`): retract the fetch
                // that was already emitted, turn it into a store, and park the store
                // on the operator stack — ONE code path, whether the fetch was a
                // number (FETCH_A..FETCH_P) or a string (FETCH_AA..FETCH_LL). It
                // flushes nothing: a store left on the stack by an earlier `:=` in a
                // chain stays there, which is how `A:=B:=5` reaches the end at depth
                // -1 and is rejected as CALC_ERR_INCOMPLETE rather than underflowing
                // mid-parse.
                Token::Assign => {
                    let (idx, is_double) = match output.last() {
                        Some(Opcode::Core(CoreOp::PushVar(idx))) => (*idx, false),
                        Some(Opcode::Core(CoreOp::PushDoubleVar(idx))) => (*idx, true),
                        _ => return Err(CalcError::BadAssignment),
                    };
                    output.pop();
                    runtime_depth -= 1;
                    stack.push(StackEntry::Store {
                        var_idx: idx,
                        is_double,
                    });
                    operand_needed = true;
                }

                // `expr[i,j]` and `expr{i,j}`. Both delimiters are in all three
                // synApps tables, but they do NOT mean the same thing in both
                // engines: sCalc has `[`=SUBRANGE / `{`=REPLACE
                // (sCalcPostfix.c:215-216) while aCalc has `[`=SUBRANGE /
                // `{`=SUBRANGE_IP, the in-place variant (aCalcPostfix.c:212-213).
                // The port emitted the STRING opcodes for both engines, so the
                // array subrange — whose opcodes already existed — was
                // unreachable.
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
                    output.push(match kind {
                        ExprKind::Array => Opcode::Array(super::opcodes::ArrayOp::ArraySubrange),
                        _ => Opcode::String(super::opcodes::StringOp::Subrange),
                    });
                    runtime_depth -= 2; // consumes subject + 2 args, pushes 1
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
                    output.push(match kind {
                        ExprKind::Array => {
                            Opcode::Array(super::opcodes::ArrayOp::ArraySubrangeInPlace)
                        }
                        _ => Opcode::String(super::opcodes::StringOp::Replace),
                    });
                    runtime_depth -= 2; // consumes subject + 2 args, pushes 1
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

    // End-of-expression well-formedness. ONE rule, the same in all three C
    // compilers (`postfix.c:499-502`, `sCalcPostfix.c:862-870`,
    // `aCalcPostfix.c:790-799`): the program must leave exactly one value on the
    // runtime stack.
    //
    // An assignment has runtime_effect -1 (`sCalcPostfix.c:226`) and pushes
    // nothing, so a store-TERMINATED source ends at depth 0 and is rejected:
    // compiled sCalcPostfix answers CALC_ERR_INCOMPLETE for `A:=5`, `AA:="x"`,
    // `A:=5;B:=6` and `A:=B:=5`, and 0 for `A:=5;A`. An expression that assigns
    // must still say what its value is.
    //
    // The port exempted depth 0 when the last emitted opcode was a store, which
    // accepted every one of those. There is no exemption: depth 1 or Incomplete.
    //
    // A source that produced no opcodes at all lands here too — `operand_needed`
    // is still set, so it is Incomplete, which is what C answers for a
    // whitespace-only expression, the only way to get here (the empty string
    // never reaches the compiler; see `calc::compile`).
    if operand_needed || runtime_depth != 1 {
        return Err(CalcError::Incomplete);
    }

    output.push(Opcode::Core(CoreOp::End));

    // No post-pass gate on which opcodes this engine may emit: `token::ElementTable`
    // is the single owner of that (one table per C compiler, applied while lexing,
    // exactly as C's `get_element` does), and every token it spells now compiles to
    // an opcode this engine's evaluator runs — including `[` and `{`, which are a
    // string slice in sCalc and an array subrange in aCalc.

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
