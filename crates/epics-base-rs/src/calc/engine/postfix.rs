use super::error::CalcError;
use super::opcodes::{CoreOp, Opcode};
use super::token::{ConstName, FuncName, Token};
use super::{CompiledExpr, ExprKind};

// Operator precedence levels (matching sCalcPostfix.c)
//  2: ||, |, OR, XOR
//  3: &&, &, AND, >>, <<
//  4: >?, <?
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
        Token::AndAnd | Token::BitAnd | Token::AndKeyword | Token::Shr | Token::Shl => Some((3, 3)),
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

fn func_to_opcode(func: &FuncName, nargs: u8) -> Opcode {
    let core = match func {
        FuncName::Abs => CoreOp::Abs,
        FuncName::Sqrt | FuncName::Sqr => CoreOp::Sqrt,
        FuncName::Exp => CoreOp::Exp,
        FuncName::Log10 => CoreOp::Log10,
        FuncName::LogE | FuncName::Ln => CoreOp::LogE,
        FuncName::Log2 => CoreOp::Log2,
        FuncName::Sin => CoreOp::Sin,
        FuncName::Cos => CoreOp::Cos,
        FuncName::Tan => CoreOp::Tan,
        FuncName::Asin => CoreOp::Asin,
        FuncName::Acos => CoreOp::Acos,
        FuncName::Atan => CoreOp::Atan,
        FuncName::Atan2 => CoreOp::Atan2,
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

pub fn compile(tokens: &[Token]) -> Result<CompiledExpr, CalcError> {
    if tokens.is_empty() {
        return Ok(CompiledExpr {
            code: vec![Opcode::Core(CoreOp::End)],
            kind: ExprKind::Numeric,
            loop_pairs: Vec::new(),
        });
    }

    let mut output: Vec<Opcode> = Vec::new();
    let mut stack: Vec<StackEntry> = Vec::new();
    let mut operand_needed = true;
    let mut runtime_depth: i32 = 0;
    let mut cond_count: i32 = 0;
    let mut pos = 0;
    #[allow(unused_mut)]
    let mut has_string_ops = false;
    #[allow(unused_mut)]
    let mut has_array_ops = false;
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
                    has_string_ops = true;
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
                    has_string_ops = true;
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
                    // If there's a pending UNTIL, close it
                    if let Some(until_pc) = until_stack.pop() {
                        let end_pc = output.len();
                        output.push(Opcode::Control(super::opcodes::ControlOp::UntilEnd(
                            until_pc,
                        )));
                        // Patch the Until opcode with the end_pc
                        output[until_pc] =
                            Opcode::Control(super::opcodes::ControlOp::Until(end_pc));
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
                    has_string_ops = true;
                    // Mark that we need to emit Subrange on RBracket
                    bracket_depth += 1;
                }

                // Brace replace: expr{find,replace} → Replace
                Token::LBrace => {
                    pop_higher_or_equal(&mut stack, 11, &mut output, &mut runtime_depth);
                    stack.push(StackEntry::LParen);
                    operand_needed = true;
                    has_string_ops = true;
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
                    has_string_ops = true;
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

    if operand_needed && !output.is_empty() {
        return Err(CalcError::Incomplete);
    }

    while let Some(entry) = stack.pop() {
        match entry {
            StackEntry::LParen => return Err(CalcError::ParenOpen),
            _ => {
                flush_stack_entry(&entry, &mut output);
            }
        }
    }

    if cond_count != 0 {
        return Err(CalcError::Conditional);
    }

    output.push(Opcode::Core(CoreOp::End));

    let kind = if has_array_ops {
        ExprKind::Array
    } else if has_string_ops {
        ExprKind::String
    } else {
        ExprKind::Numeric
    };

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
        } => match f {
            FuncName::Atan2 => -1,
            _ => 0,
        },
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
