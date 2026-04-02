use super::array_value::{ArrayStackValue, zip_map};
use super::error::CalcError;
use super::opcodes::{ArrayOp, CoreOp, Opcode};
use super::{ArrayInputs, CompiledExpr};
use crate::calc::math::{derivative, fitting, stats};

pub fn eval(expr: &CompiledExpr, inputs: &mut ArrayInputs) -> Result<ArrayStackValue, CalcError> {
    let mut stack: Vec<ArrayStackValue> = Vec::with_capacity(20);
    let code = &expr.code;
    let mut pc = 0;

    while pc < code.len() {
        let op = &code[pc];
        pc += 1;

        match op {
            Opcode::Core(core) => match core {
                CoreOp::End => break,

                CoreOp::PushConst(v) => stack.push(ArrayStackValue::Double(*v)),
                CoreOp::PushVar(idx) => {
                    stack.push(ArrayStackValue::Double(inputs.num_vars[*idx as usize]));
                }
                CoreOp::PushDoubleVar(idx) => {
                    // In array evaluator, double vars are array vars
                    let arr = inputs.arrays[*idx as usize].clone();
                    if arr.is_empty() {
                        stack.push(ArrayStackValue::Double(0.0));
                    } else {
                        stack.push(ArrayStackValue::Array(arr));
                    }
                }

                CoreOp::Pi => stack.push(ArrayStackValue::Double(std::f64::consts::PI)),
                CoreOp::D2R => stack.push(ArrayStackValue::Double(std::f64::consts::PI / 180.0)),
                CoreOp::R2D => stack.push(ArrayStackValue::Double(180.0 / std::f64::consts::PI)),

                CoreOp::Random => stack.push(ArrayStackValue::Double(simple_random())),
                CoreOp::NormalRandom => {
                    let u1 = simple_random();
                    let u2 = simple_random();
                    let n = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
                    stack.push(ArrayStackValue::Double(n));
                }

                // Type-aware arithmetic via zip_map
                CoreOp::Add => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| x + y)?);
                }
                CoreOp::Sub => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| x - y)?);
                }
                CoreOp::Mul => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| x * y)?);
                }
                CoreOp::Div => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(
                        a,
                        b,
                        |x, y| if y == 0.0 { f64::NAN } else { x / y },
                    )?);
                }
                CoreOp::Mod => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| {
                        if y as i64 == 0 {
                            f64::NAN
                        } else {
                            ((x as i64) % (y as i64)) as f64
                        }
                    })?);
                }
                CoreOp::Neg => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.map(|x| -x));
                }
                CoreOp::Power => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| x.powf(y))?);
                }

                // Comparison (element-wise for arrays)
                CoreOp::Eq => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| {
                        if (x - y).abs() < 1e-11 { 1.0 } else { 0.0 }
                    })?);
                }
                CoreOp::Ne => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| {
                        if (x - y).abs() > 1e-11 { 1.0 } else { 0.0 }
                    })?);
                }
                CoreOp::Lt => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(
                        a,
                        b,
                        |x, y| if (y - x) > 1e-11 { 1.0 } else { 0.0 },
                    )?);
                }
                CoreOp::Le => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| {
                        if (x - y).abs() < 1e-11 || x < y {
                            1.0
                        } else {
                            0.0
                        }
                    })?);
                }
                CoreOp::Gt => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(
                        a,
                        b,
                        |x, y| if (x - y) > 1e-11 { 1.0 } else { 0.0 },
                    )?);
                }
                CoreOp::Ge => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| {
                        if (x - y).abs() < 1e-11 || x > y {
                            1.0
                        } else {
                            0.0
                        }
                    })?);
                }

                // Logical
                CoreOp::And => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(
                        a,
                        b,
                        |x, y| if x != 0.0 && y != 0.0 { 1.0 } else { 0.0 },
                    )?);
                }
                CoreOp::Or => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(
                        a,
                        b,
                        |x, y| if x != 0.0 || y != 0.0 { 1.0 } else { 0.0 },
                    )?);
                }
                CoreOp::Not => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.map(|x| if x == 0.0 { 1.0 } else { 0.0 }));
                }

                // Bitwise (element-wise)
                CoreOp::BitAnd => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| ((x as i64) & (y as i64)) as f64)?);
                }
                CoreOp::BitOr => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| ((x as i64) | (y as i64)) as f64)?);
                }
                CoreOp::BitXor => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| ((x as i64) ^ (y as i64)) as f64)?);
                }
                CoreOp::BitNot => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.map(|x| !(x as i64) as f64));
                }
                CoreOp::Shl => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| ((x as i64) << (y as i64)) as f64)?);
                }
                CoreOp::Shr => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| ((x as i64) >> (y as i64)) as f64)?);
                }

                // Conditional
                CoreOp::CondIf => {
                    let cond = pop1_f64(&mut stack)?;
                    if cond == 0.0 {
                        pc = cond_search(code, pc, true)?;
                    }
                }
                CoreOp::CondElse => {
                    pc = cond_search(code, pc, false)?;
                }
                CoreOp::CondEnd => {}

                // Unary math functions (element-wise)
                CoreOp::Abs => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.map(|x| x.abs()));
                }
                CoreOp::Sqrt => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.map(|x| x.sqrt()));
                }
                CoreOp::Exp => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.map(|x| x.exp()));
                }
                CoreOp::Log10 => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.map(|x| x.log10()));
                }
                CoreOp::LogE => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.map(|x| x.ln()));
                }
                CoreOp::Log2 => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.map(|x| x.log2()));
                }
                CoreOp::Sin => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.map(|x| x.sin()));
                }
                CoreOp::Cos => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.map(|x| x.cos()));
                }
                CoreOp::Tan => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.map(|x| x.tan()));
                }
                CoreOp::Asin => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.map(|x| x.asin()));
                }
                CoreOp::Acos => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.map(|x| x.acos()));
                }
                CoreOp::Atan => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.map(|x| x.atan()));
                }
                CoreOp::Sinh => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.map(|x| x.sinh()));
                }
                CoreOp::Cosh => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.map(|x| x.cosh()));
                }
                CoreOp::Tanh => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.map(|x| x.tanh()));
                }
                CoreOp::Ceil => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.map(|x| x.ceil()));
                }
                CoreOp::Floor => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.map(|x| x.floor()));
                }
                CoreOp::Nint => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.map(|x| {
                        let rounded = if x >= 0.0 {
                            (x + 0.5) as i64
                        } else {
                            (x - 0.5) as i64
                        };
                        rounded as f64
                    }));
                }

                CoreOp::IsNan(nargs) => {
                    let n = *nargs as usize;
                    if stack.len() < n {
                        return Err(CalcError::Underflow);
                    }
                    let mut result = false;
                    for _ in 0..n {
                        let v = pop1_f64(&mut stack)?;
                        result = result || v.is_nan();
                    }
                    stack.push(ArrayStackValue::Double(if result { 1.0 } else { 0.0 }));
                }
                CoreOp::IsInf => {
                    let a = pop1_f64(&mut stack)?;
                    stack.push(ArrayStackValue::Double(if a.is_infinite() {
                        1.0
                    } else {
                        0.0
                    }));
                }
                CoreOp::Finite(nargs) => {
                    let n = *nargs as usize;
                    if stack.len() < n {
                        return Err(CalcError::Underflow);
                    }
                    let mut result = true;
                    for _ in 0..n {
                        let v = pop1_f64(&mut stack)?;
                        result = result && v.is_finite();
                    }
                    stack.push(ArrayStackValue::Double(if result { 1.0 } else { 0.0 }));
                }

                CoreOp::Atan2 => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| y.atan2(x))?);
                }

                CoreOp::Max(nargs) => {
                    let n = *nargs as usize;
                    if stack.len() < n {
                        return Err(CalcError::Underflow);
                    }
                    let mut result = pop1_f64(&mut stack)?;
                    for _ in 1..n {
                        let v = pop1_f64(&mut stack)?;
                        if v > result || result.is_nan() {
                            result = v;
                        }
                    }
                    stack.push(ArrayStackValue::Double(result));
                }
                CoreOp::Min(nargs) => {
                    let n = *nargs as usize;
                    if stack.len() < n {
                        return Err(CalcError::Underflow);
                    }
                    let mut result = pop1_f64(&mut stack)?;
                    for _ in 1..n {
                        let v = pop1_f64(&mut stack)?;
                        if v < result || result.is_nan() {
                            result = v;
                        }
                    }
                    stack.push(ArrayStackValue::Double(result));
                }
                CoreOp::MaxVal => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    stack.push(ArrayStackValue::Double(if a > b { a } else { b }));
                }
                CoreOp::MinVal => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    stack.push(ArrayStackValue::Double(if a < b { a } else { b }));
                }

                CoreOp::StoreVar(idx) => {
                    let v = pop1_f64(&mut stack)?;
                    inputs.num_vars[*idx as usize] = v;
                }
                CoreOp::StoreDoubleVar(idx) => {
                    let v = pop1(&mut stack)?;
                    match v {
                        ArrayStackValue::Array(arr) => {
                            inputs.arrays[*idx as usize] = arr;
                        }
                        ArrayStackValue::Double(d) => {
                            inputs.num_vars[*idx as usize] = d;
                        }
                    }
                }
            },

            Opcode::Array(aop) => match aop {
                ArrayOp::ConstIndex => {
                    let arr: Vec<f64> = (0..inputs.array_size).map(|i| i as f64).collect();
                    stack.push(ArrayStackValue::Array(arr));
                }
                ArrayOp::ToArray => {
                    let v = pop1_f64(&mut stack)?;
                    stack.push(ArrayStackValue::Array(vec![v; inputs.array_size]));
                }
                ArrayOp::ToDouble => {
                    let v = pop1(&mut stack)?;
                    stack.push(ArrayStackValue::Double(v.as_f64()?));
                }
                ArrayOp::Average => {
                    let v = pop1(&mut stack)?;
                    let arr = match &v {
                        ArrayStackValue::Array(a) => a.as_slice(),
                        ArrayStackValue::Double(_) => return Err(CalcError::TypeMismatch),
                    };
                    stack.push(ArrayStackValue::Double(stats::average(arr)));
                }
                ArrayOp::StdDev => {
                    let v = pop1(&mut stack)?;
                    let arr = match &v {
                        ArrayStackValue::Array(a) => a.as_slice(),
                        ArrayStackValue::Double(_) => return Err(CalcError::TypeMismatch),
                    };
                    stack.push(ArrayStackValue::Double(stats::std_dev(arr)));
                }
                ArrayOp::Fwhm => {
                    let v = pop1(&mut stack)?;
                    let arr = match &v {
                        ArrayStackValue::Array(a) => a.as_slice(),
                        ArrayStackValue::Double(_) => return Err(CalcError::TypeMismatch),
                    };
                    stack.push(ArrayStackValue::Double(stats::fwhm(arr)));
                }
                ArrayOp::ArraySum => {
                    let v = pop1(&mut stack)?;
                    let arr = match &v {
                        ArrayStackValue::Array(a) => a.as_slice(),
                        ArrayStackValue::Double(_) => return Err(CalcError::TypeMismatch),
                    };
                    stack.push(ArrayStackValue::Double(arr.iter().sum()));
                }
                ArrayOp::ArrayMax => {
                    let v = pop1(&mut stack)?;
                    let arr = match &v {
                        ArrayStackValue::Array(a) => a.as_slice(),
                        ArrayStackValue::Double(_) => return Err(CalcError::TypeMismatch),
                    };
                    let max = arr.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                    stack.push(ArrayStackValue::Double(if arr.is_empty() {
                        0.0
                    } else {
                        max
                    }));
                }
                ArrayOp::ArrayMin => {
                    let v = pop1(&mut stack)?;
                    let arr = match &v {
                        ArrayStackValue::Array(a) => a.as_slice(),
                        ArrayStackValue::Double(_) => return Err(CalcError::TypeMismatch),
                    };
                    let min = arr.iter().cloned().fold(f64::INFINITY, f64::min);
                    stack.push(ArrayStackValue::Double(if arr.is_empty() {
                        0.0
                    } else {
                        min
                    }));
                }
                ArrayOp::IndexMax => {
                    let v = pop1(&mut stack)?;
                    let arr = v.as_array()?;
                    let idx = arr
                        .iter()
                        .enumerate()
                        .max_by(|(_, a), (_, b)| {
                            a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
                        })
                        .map(|(i, _)| i as f64)
                        .unwrap_or(0.0);
                    stack.push(ArrayStackValue::Double(idx));
                }
                ArrayOp::IndexMin => {
                    let v = pop1(&mut stack)?;
                    let arr = v.as_array()?;
                    let idx = arr
                        .iter()
                        .enumerate()
                        .min_by(|(_, a), (_, b)| {
                            a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
                        })
                        .map(|(i, _)| i as f64)
                        .unwrap_or(0.0);
                    stack.push(ArrayStackValue::Double(idx));
                }
                ArrayOp::IndexZero => {
                    let v = pop1(&mut stack)?;
                    let arr = v.as_array()?;
                    let idx = arr
                        .iter()
                        .position(|&x| x == 0.0)
                        .map(|i| i as f64)
                        .unwrap_or(-1.0);
                    stack.push(ArrayStackValue::Double(idx));
                }
                ArrayOp::IndexNonZero => {
                    let v = pop1(&mut stack)?;
                    let arr = v.as_array()?;
                    let idx = arr
                        .iter()
                        .position(|&x| x != 0.0)
                        .map(|i| i as f64)
                        .unwrap_or(-1.0);
                    stack.push(ArrayStackValue::Double(idx));
                }

                ArrayOp::Smooth => {
                    let v = pop1(&mut stack)?;
                    let arr = v.as_array()?;
                    stack.push(ArrayStackValue::Array(stats::smooth(arr)));
                }
                ArrayOp::NSmooth => {
                    let n = pop1_f64(&mut stack)? as usize;
                    let v = pop1(&mut stack)?;
                    let arr = v.as_array()?;
                    stack.push(ArrayStackValue::Array(stats::nsmooth(arr, n)));
                }
                ArrayOp::Deriv => {
                    let v = pop1(&mut stack)?;
                    let arr = v.as_array()?;
                    stack.push(ArrayStackValue::Array(derivative::deriv(arr)));
                }
                ArrayOp::NDeriv => {
                    let n = pop1_f64(&mut stack)? as usize;
                    let v = pop1(&mut stack)?;
                    let arr = v.as_array()?;
                    stack.push(ArrayStackValue::Array(derivative::nderiv(arr, n)));
                }
                ArrayOp::Cum => {
                    let v = pop1(&mut stack)?;
                    let arr = v.as_array()?;
                    let mut result = arr.to_vec();
                    for i in 1..result.len() {
                        result[i] += result[i - 1];
                    }
                    stack.push(ArrayStackValue::Array(result));
                }
                ArrayOp::Cat => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    let mut result = match a {
                        ArrayStackValue::Array(arr) => arr,
                        ArrayStackValue::Double(d) => vec![d],
                    };
                    match b {
                        ArrayStackValue::Array(arr) => result.extend(arr),
                        ArrayStackValue::Double(d) => result.push(d),
                    }
                    stack.push(ArrayStackValue::Array(result));
                }
                ArrayOp::ArrayRandom => {
                    let arr: Vec<f64> = (0..inputs.array_size).map(|_| simple_random()).collect();
                    stack.push(ArrayStackValue::Array(arr));
                }
                ArrayOp::ArraySubrange => {
                    let end_val = pop1_f64(&mut stack)? as i64;
                    let start_val = pop1_f64(&mut stack)? as i64;
                    let v = pop1(&mut stack)?;
                    let arr = v.as_array()?;
                    let len = arr.len() as i64;
                    let start = start_val.max(0).min(len) as usize;
                    let end = end_val.max(0).min(len) as usize;
                    let end = end.max(start);
                    stack.push(ArrayStackValue::Array(arr[start..end].to_vec()));
                }
                ArrayOp::ArraySubrangeInPlace => {
                    let end_val = pop1_f64(&mut stack)? as i64;
                    let start_val = pop1_f64(&mut stack)? as i64;
                    let v = pop1(&mut stack)?;
                    let arr = v.as_array()?;
                    let len = arr.len() as i64;
                    let start = start_val.max(0).min(len) as usize;
                    let end = end_val.max(0).min(len) as usize;
                    let end = end.max(start);
                    stack.push(ArrayStackValue::Array(arr[start..end].to_vec()));
                }
                ArrayOp::FitPoly => {
                    let y = pop1(&mut stack)?;
                    let x = pop1(&mut stack)?;
                    let xa = x.as_array()?;
                    let ya = y.as_array()?;
                    let (a0, a1, a2) = fitting::fitpoly(xa, ya, None);
                    // Return as array [a0, a1, a2]
                    stack.push(ArrayStackValue::Array(vec![a0, a1, a2]));
                }
                ArrayOp::FitMPoly => {
                    let mask = pop1(&mut stack)?;
                    let y = pop1(&mut stack)?;
                    let x = pop1(&mut stack)?;
                    let xa = x.as_array()?;
                    let ya = y.as_array()?;
                    let ma = mask.as_array()?;
                    let (a0, a1, a2) = fitting::fitpoly(xa, ya, Some(ma));
                    stack.push(ArrayStackValue::Array(vec![a0, a1, a2]));
                }
                ArrayOp::FitQ => {
                    // Like FitPoly but returns quality metric
                    let y = pop1(&mut stack)?;
                    let x = pop1(&mut stack)?;
                    let xa = x.as_array()?;
                    let ya = y.as_array()?;
                    let (a0, a1, a2) = fitting::fitpoly(xa, ya, None);
                    // Compute residual sum of squares
                    let rss: f64 = xa
                        .iter()
                        .zip(ya.iter())
                        .map(|(&xi, &yi)| {
                            let pred = a0 + a1 * xi + a2 * xi * xi;
                            (yi - pred).powi(2)
                        })
                        .sum();
                    stack.push(ArrayStackValue::Array(vec![a0, a1, a2, rss]));
                }
                ArrayOp::FitMQ => {
                    let mask = pop1(&mut stack)?;
                    let y = pop1(&mut stack)?;
                    let x = pop1(&mut stack)?;
                    let xa = x.as_array()?;
                    let ya = y.as_array()?;
                    let ma = mask.as_array()?;
                    let (a0, a1, a2) = fitting::fitpoly(xa, ya, Some(ma));
                    let rss: f64 = xa
                        .iter()
                        .zip(ya.iter())
                        .zip(ma.iter())
                        .filter(|&((_, _), &m)| m != 0.0)
                        .map(|((&xi, &yi), _)| {
                            let pred = a0 + a1 * xi + a2 * xi * xi;
                            (yi - pred).powi(2)
                        })
                        .sum();
                    stack.push(ArrayStackValue::Array(vec![a0, a1, a2, rss]));
                }
            },

            #[allow(unreachable_patterns)]
            _ => return Err(CalcError::Internal),
        }
    }

    Ok(stack
        .last()
        .cloned()
        .unwrap_or(ArrayStackValue::Double(0.0)))
}

fn pop1(stack: &mut Vec<ArrayStackValue>) -> Result<ArrayStackValue, CalcError> {
    stack.pop().ok_or(CalcError::Underflow)
}

fn pop1_f64(stack: &mut Vec<ArrayStackValue>) -> Result<f64, CalcError> {
    let v = stack.pop().ok_or(CalcError::Underflow)?;
    v.as_f64()
}

fn pop2_f64(stack: &mut Vec<ArrayStackValue>) -> Result<(f64, f64), CalcError> {
    let b = pop1_f64(stack)?;
    let a = pop1_f64(stack)?;
    Ok((a, b))
}

fn cond_search(code: &[Opcode], start: usize, find_else: bool) -> Result<usize, CalcError> {
    let mut depth = 0;
    let mut pc = start;
    while pc < code.len() {
        match &code[pc] {
            Opcode::Core(CoreOp::CondIf) => depth += 1,
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
