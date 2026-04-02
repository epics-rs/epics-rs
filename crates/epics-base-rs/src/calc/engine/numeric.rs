use super::error::CalcError;
use super::opcodes::{CoreOp, Opcode};
use super::{CompiledExpr, NumericInputs};

pub fn eval(expr: &CompiledExpr, inputs: &mut NumericInputs) -> Result<f64, CalcError> {
    let mut stack: Vec<f64> = Vec::with_capacity(20);
    let code = &expr.code;
    let mut pc = 0;

    while pc < code.len() {
        let op = &code[pc];
        pc += 1;

        match op {
            Opcode::Core(core) => match core {
                CoreOp::End => break,

                // Push operations
                CoreOp::PushConst(v) => stack.push(*v),
                CoreOp::PushVar(idx) => stack.push(inputs.vars[*idx as usize]),
                CoreOp::PushDoubleVar(idx) => {
                    stack.push(inputs.vars[*idx as usize]);
                }

                // Constants
                CoreOp::Pi => stack.push(std::f64::consts::PI),
                CoreOp::D2R => stack.push(std::f64::consts::PI / 180.0),
                CoreOp::R2D => stack.push(180.0 / std::f64::consts::PI),

                // Random
                CoreOp::Random => {
                    stack.push(simple_random());
                }
                CoreOp::NormalRandom => {
                    let u1 = simple_random();
                    let u2 = simple_random();
                    let n = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
                    stack.push(n);
                }

                // Arithmetic
                CoreOp::Add => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(a + b);
                }
                CoreOp::Sub => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(a - b);
                }
                CoreOp::Mul => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(a * b);
                }
                CoreOp::Div => {
                    let (a, b) = pop2(&mut stack)?;
                    if b == 0.0 {
                        stack.push(f64::NAN);
                    } else {
                        stack.push(a / b);
                    }
                }
                CoreOp::Mod => {
                    let (a, b) = pop2(&mut stack)?;
                    if b as i64 == 0 {
                        stack.push(f64::NAN);
                    } else {
                        stack.push(((a as i64) % (b as i64)) as f64);
                    }
                }
                CoreOp::Neg => {
                    let a = pop1(&mut stack)?;
                    stack.push(-a);
                }
                CoreOp::Power => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(a.powf(b));
                }

                // Comparison
                CoreOp::Eq => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(if (a - b).abs() < 1e-11 { 1.0 } else { 0.0 });
                }
                CoreOp::Ne => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(if (a - b).abs() > 1e-11 { 1.0 } else { 0.0 });
                }
                CoreOp::Lt => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(if (b - a) > 1e-11 { 1.0 } else { 0.0 });
                }
                CoreOp::Le => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(if (a - b).abs() < 1e-11 || a < b {
                        1.0
                    } else {
                        0.0
                    });
                }
                CoreOp::Gt => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(if (a - b) > 1e-11 { 1.0 } else { 0.0 });
                }
                CoreOp::Ge => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(if (a - b).abs() < 1e-11 || a > b {
                        1.0
                    } else {
                        0.0
                    });
                }

                // Logical
                CoreOp::And => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(if a != 0.0 && b != 0.0 { 1.0 } else { 0.0 });
                }
                CoreOp::Or => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(if a != 0.0 || b != 0.0 { 1.0 } else { 0.0 });
                }
                CoreOp::Not => {
                    let a = pop1(&mut stack)?;
                    stack.push(if a == 0.0 { 1.0 } else { 0.0 });
                }

                // Bitwise
                CoreOp::BitAnd => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(((a as i64) & (b as i64)) as f64);
                }
                CoreOp::BitOr => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(((a as i64) | (b as i64)) as f64);
                }
                CoreOp::BitXor => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(((a as i64) ^ (b as i64)) as f64);
                }
                CoreOp::BitNot => {
                    let a = pop1(&mut stack)?;
                    stack.push(!(a as i64) as f64);
                }
                CoreOp::Shl => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(((a as i64) << (b as i64)) as f64);
                }
                CoreOp::Shr => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(((a as i64) >> (b as i64)) as f64);
                }

                // Conditional
                CoreOp::CondIf => {
                    let cond = pop1(&mut stack)?;
                    if cond == 0.0 {
                        pc = cond_search(code, pc, true)?;
                    }
                }
                CoreOp::CondElse => {
                    pc = cond_search(code, pc, false)?;
                }
                CoreOp::CondEnd => {
                    // No-op, just a marker
                }

                // Math functions (1 arg)
                CoreOp::Abs => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.abs());
                }
                CoreOp::Sqrt => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.sqrt());
                }
                CoreOp::Exp => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.exp());
                }
                CoreOp::Log10 => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.log10());
                }
                CoreOp::LogE => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.ln());
                }
                CoreOp::Log2 => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.log2());
                }
                CoreOp::Sin => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.sin());
                }
                CoreOp::Cos => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.cos());
                }
                CoreOp::Tan => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.tan());
                }
                CoreOp::Asin => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.asin());
                }
                CoreOp::Acos => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.acos());
                }
                CoreOp::Atan => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.atan());
                }
                CoreOp::Sinh => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.sinh());
                }
                CoreOp::Cosh => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.cosh());
                }
                CoreOp::Tanh => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.tanh());
                }
                CoreOp::Ceil => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.ceil());
                }
                CoreOp::Floor => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.floor());
                }
                CoreOp::Nint => {
                    let a = pop1(&mut stack)?;
                    let rounded = if a >= 0.0 {
                        (a + 0.5) as i64
                    } else {
                        (a - 0.5) as i64
                    };
                    stack.push(rounded as f64);
                }

                // Test functions
                CoreOp::IsNan(nargs) => {
                    let n = *nargs as usize;
                    if stack.len() < n {
                        return Err(CalcError::Underflow);
                    }
                    let mut result = false;
                    for _ in 0..n {
                        let v = stack.pop().unwrap();
                        result = result || v.is_nan();
                    }
                    stack.push(if result { 1.0 } else { 0.0 });
                }
                CoreOp::IsInf => {
                    let a = pop1(&mut stack)?;
                    stack.push(if a.is_infinite() { 1.0 } else { 0.0 });
                }
                CoreOp::Finite(nargs) => {
                    let n = *nargs as usize;
                    if stack.len() < n {
                        return Err(CalcError::Underflow);
                    }
                    let mut result = true;
                    for _ in 0..n {
                        let v = stack.pop().unwrap();
                        result = result && v.is_finite();
                    }
                    stack.push(if result { 1.0 } else { 0.0 });
                }

                // 2-arg functions
                CoreOp::Atan2 => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(b.atan2(a));
                }

                // Vararg min/max
                CoreOp::Max(nargs) => {
                    let n = *nargs as usize;
                    if stack.len() < n {
                        return Err(CalcError::Underflow);
                    }
                    let mut result = stack.pop().unwrap();
                    for _ in 1..n {
                        let v = stack.pop().unwrap();
                        if v > result || result.is_nan() {
                            result = v;
                        }
                    }
                    stack.push(result);
                }
                CoreOp::Min(nargs) => {
                    let n = *nargs as usize;
                    if stack.len() < n {
                        return Err(CalcError::Underflow);
                    }
                    let mut result = stack.pop().unwrap();
                    for _ in 1..n {
                        let v = stack.pop().unwrap();
                        if v < result || result.is_nan() {
                            result = v;
                        }
                    }
                    stack.push(result);
                }

                // Binary max/min operators
                CoreOp::MaxVal => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(if a > b { a } else { b });
                }
                CoreOp::MinVal => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(if a < b { a } else { b });
                }

                // Store
                CoreOp::StoreVar(idx) => {
                    let v = pop1(&mut stack)?;
                    inputs.vars[*idx as usize] = v;
                }
                CoreOp::StoreDoubleVar(idx) => {
                    let v = pop1(&mut stack)?;
                    inputs.vars[*idx as usize] = v;
                }
            },

            // Non-core opcodes are not supported by the numeric evaluator
            #[allow(unreachable_patterns)]
            _ => return Err(CalcError::Internal),
        }
    }

    Ok(stack.last().copied().unwrap_or(0.0))
}

fn pop1(stack: &mut Vec<f64>) -> Result<f64, CalcError> {
    stack.pop().ok_or(CalcError::Underflow)
}

fn pop2(stack: &mut Vec<f64>) -> Result<(f64, f64), CalcError> {
    let b = stack.pop().ok_or(CalcError::Underflow)?;
    let a = stack.pop().ok_or(CalcError::Underflow)?;
    Ok((a, b))
}

fn cond_search(code: &[Opcode], start: usize, find_else: bool) -> Result<usize, CalcError> {
    let mut depth = 0;
    let mut pc = start;

    while pc < code.len() {
        match &code[pc] {
            Opcode::Core(CoreOp::CondIf) => {
                depth += 1;
            }
            Opcode::Core(CoreOp::CondElse) => {
                if depth == 0 && find_else {
                    return Ok(pc + 1);
                }
            }
            Opcode::Core(CoreOp::CondEnd) => {
                if depth == 0 && !find_else {
                    return Ok(pc + 1);
                }
                if depth > 0 {
                    depth -= 1;
                }
            }
            Opcode::Core(CoreOp::End) => break,
            _ => {}
        }
        pc += 1;
    }

    Err(CalcError::Conditional)
}

fn simple_random() -> f64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEED: AtomicU64 = AtomicU64::new(0);

    let mut s = SEED.load(Ordering::Relaxed);
    if s == 0 {
        s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
    }
    s = s
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    SEED.store(s, Ordering::Relaxed);
    ((s >> 11) as f64) / ((1u64 << 53) as f64) + f64::MIN_POSITIVE
}
