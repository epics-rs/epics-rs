use super::array_value::{ArrayStackValue, zip_map};
use super::cast::{c_int, c_long, d2ui};
use super::error::CalcError;
use super::opcodes::{ArrayOp, CoreOp, Opcode};
use super::{ArrayInputs, CompiledExpr};
use crate::calc::math::{derivative, fitting, stats};

/// C `myMAXFLOAT` (`aCalcPerform.c:49`): `((float)1e+35)`, widened back to a
/// double when it lands in the stack cell — so it is the f32-rounded value,
/// not `1e35` exactly. aCalc's MODULO uses it for a zero divisor where base
/// uses NaN and sCalc returns an error.
const MY_MAXFLOAT: f64 = 1e35f32 as f64;

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
                CoreOp::FetchVal => {
                    // C FETCH_VAL pushes *presult (the record's previous result).
                    stack.push(ArrayStackValue::Double(inputs.prev_val));
                }
                CoreOp::FetchSval => {
                    // aCalc has no SVAL: `aCalcPostfix`'s element table never
                    // emits FETCH_SVAL and `aCalcPerform` has no string result
                    // to push. Reachable only through the port's shared
                    // tokenizer, and rejected like the other string-only opcodes.
                    return Err(CalcError::Internal);
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
                    // aCalc, not base (`aCalcPerform.c:636-643`, :659-667,
                    // :690-696): a zero divisor is `myMAXFLOAT` in all three
                    // operand shapes — array/array, array/scalar and
                    // scalar/scalar — never NaN and never an error. (C's
                    // scalar/array shape promotes the scalar with
                    // `toArray(ps,1)` and then runs the array/array loop, so
                    // the per-element test below covers it too.)
                    stack.push(zip_map(
                        a,
                        b,
                        |x, y| if y == 0.0 { MY_MAXFLOAT } else { x / y },
                    )?);
                }
                CoreOp::Mod => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    // aCalc, not base (`aCalcPerform.c:645-652`, :669-677,
                    // :697-703): plain `(int)` casts, and a zero divisor is
                    // neither NaN nor an error — it is `myMAXFLOAT`.
                    stack.push(zip_map(a, b, |x, y| {
                        let den = c_int(y);
                        if den == 0 {
                            MY_MAXFLOAT
                        } else {
                            c_int(x).wrapping_rem(den) as f64
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

                // Bitwise (element-wise). aCalc has no `d2i`: every operand
                // takes a plain `(int)` cast (`aCalcPerform.c:907`,
                // :1355-1357, :1380-1382, :1407-1409, :1424-1427). The shift
                // count is unmasked in C — x86-64 `shl`/`sar` mask it to 5
                // bits for a 32-bit operand, which is the observable.
                CoreOp::BitAnd => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| (c_int(x) & c_int(y)) as f64)?);
                }
                CoreOp::BitOr => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| (c_int(x) | c_int(y)) as f64)?);
                }
                CoreOp::BitXor => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| (c_int(x) ^ c_int(y)) as f64)?);
                }
                CoreOp::BitNot => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.map(|x| !c_int(x) as f64));
                }
                // `<<`/`>>` are ONE arm in C (`aCalcPerform.c:1416-1459`) and
                // the LEFT operand's type picks the whole meaning:
                //   scalar left  -> a bit shift by the `(int)` count (:1421-1427)
                //   array  left  -> a POSITIONAL move of the elements, NOT a
                //                   bitwise anything (:1428-1458)
                // The count is collapsed to a double either way (`toDouble(ps1)`,
                // :1420 — an array count becomes its `a[0]`, `to_double` :121).
                CoreOp::Shl | CoreOp::Shr => {
                    let left_shift = matches!(core, CoreOp::Shl);
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    let count = b.as_f64()?;
                    match a {
                        ArrayStackValue::Double(x) => {
                            let n = c_int(count) & 31;
                            let v = if left_shift {
                                c_int(x) << n
                            } else {
                                c_int(x) >> n
                            };
                            stack.push(ArrayStackValue::Double(v as f64));
                        }
                        ArrayStackValue::Array(mut arr) => {
                            // C negates the count for `<<` (:1431) — a left
                            // shift moves elements DOWN in index.
                            shift_elements(&mut arr, if left_shift { -count } else { count });
                            stack.push(ArrayStackValue::Array(arr));
                        }
                    }
                }
                // `>>>` (RIGHT_SHIFT_LOGIC) is a BASE opcode; aCalcPostfix has
                // no such element, so no aCalc expression can contain one and
                // there is no aCalc semantics to match. Base's is kept for the
                // shared `CoreOp`; the grammar is what must refuse it.
                CoreOp::ShrLogical => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| (d2ui(x) >> (d2ui(y) & 31)) as f64)?);
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
                    // C `aCalcPerform.c:827-830` (array) and :1085 (scalar):
                    //   (double)(long)(x >= 0 ? x+0.5 : x-0.5)
                    // `(long)`, not base's `(epicsInt32)`.
                    stack.push(a.map(|x| {
                        let pre = if x >= 0.0 { x + 0.5 } else { x - 0.5 };
                        c_long(pre) as f64
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
                CoreOp::Fmod => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| x % y)?);
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

/// C `myNINT` (`aCalcPerform.c:50`): `(int)(a >= 0 ? a+0.5 : a-0.5)` — a
/// truncating cast, so it rounds half away from zero.
fn my_nint(a: f64) -> i32 {
    (if a >= 0.0 { a + 0.5 } else { a - 0.5 }) as i32
}

/// C `SMALL` (`aCalcPerform.c:56`).
const SMALL: f64 = 1e-9;

/// The ARRAY form of aCalc's `<<`/`>>` (`aCalcPerform.c:1428-1458`): move the
/// elements by `e` positions (`e` is already negated by the caller for `<<`),
/// zero-filling the vacated end, and — because `e` is a DOUBLE — linearly
/// interpolate the fractional remainder. `e > 0` moves elements toward higher
/// indices.
///
/// The interpolation is done in place, and C reads neighbours that the same
/// pass has already overwritten (the `+=` walks the array in one direction and
/// looks back the way it came for the extrapolated end point), so this walks in
/// exactly C's order rather than reading from a saved copy.
fn shift_elements(a: &mut [f64], e: f64) {
    let n = a.len();
    if n == 0 {
        return;
    }
    let j = my_nint(e);
    if j > 0 {
        let j = j as usize;
        if j >= n {
            a.fill(0.0);
        } else {
            for i in (j..n).rev() {
                a[i] = a[i - j];
            }
            a[..j].fill(0.0);
        }
    } else if j < 0 {
        let k = j.unsigned_abs() as usize;
        if k >= n {
            a.fill(0.0);
        } else {
            for i in 0..(n - k) {
                a[i] = a[i + k];
            }
            a[(n - k)..].fill(0.0);
        }
    }

    let d = (e - f64::from(j)).abs();
    if d <= SMALL {
        return;
    }
    // A single element has no neighbour to interpolate against; C would index
    // a[-1]/a[1] out of its own array here, so there is no behaviour to match.
    if n < 2 {
        return;
    }
    if e < f64::from(j) {
        for i in 0..n - 1 {
            a[i] += d * (a[i + 1] - a[i]);
        }
        // C `:1449` extrapolates the last point from the ALREADY-updated a[n-2].
        a[n - 1] += d * (a[n - 1] - a[n - 2]);
    } else {
        for i in (1..n).rev() {
            a[i] += d * (a[i - 1] - a[i]);
        }
        // C `:1455`, mirror image: a[1] here has already been updated.
        a[0] += d * (a[0] - a[1]);
    }
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

/// Forward-scan for a matching conditional opcode, mirroring C `cond_search`
/// (calcPerform.c:520-557): `count` starts at 1, the target opcode decrements
/// it (return when 0), `COND_IF` increments it.
fn cond_search(code: &[Opcode], start: usize, find_else: bool) -> Result<usize, CalcError> {
    let mut count: i32 = 1;
    let mut pc = start;
    while pc < code.len() {
        let op = &code[pc];
        if matches!(op, Opcode::Core(CoreOp::End)) {
            break;
        }
        let is_match = match op {
            Opcode::Core(CoreOp::CondElse) => find_else,
            Opcode::Core(CoreOp::CondEnd) => !find_else,
            _ => false,
        };
        if is_match {
            count -= 1;
            if count == 0 {
                return Ok(pc + 1);
            }
        }
        if matches!(op, Opcode::Core(CoreOp::CondIf)) {
            count += 1;
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
    // C calcRandom() returns (double)rand()/RAND_MAX — a closed [0,1] range.
    (s >> 11) as f64 / ((1u64 << 53) - 1) as f64
}
