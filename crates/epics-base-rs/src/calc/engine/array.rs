use super::array_value::ArrayStackValue::Double;
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

    // C's `status` (`aCalcPerform.c:422`) — a deferred failure flag, NOT an early
    // return. An operator that trips it (the array SQRT/LOG domain guard) sets it
    // and execution CONTINUES to the end of the expression; only then does
    // aCalcPerform bail (`:1602-1605`) without writing p_dresult/p_aresult.
    //
    // The deferral is observable: aCalc's store opcodes write straight into the
    // record's A..P / AA..LL fields, so a store sequenced AFTER the failing
    // operator still lands in C. Returning early from the operator's arm would
    // silently skip it.
    let mut status: Option<CalcError> = None;

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

                // Comparison (element-wise for arrays). aCalc compares EXACTLY —
                // C's operators are the bare C ones in all three operand shapes
                // (`aCalcPerform.c:1345-1350` array/array, :1370-1375
                // array/scalar, :1397-1402 scalar/scalar) and aCalcPerform has
                // no epsilon anywhere. The 1e-11 these arms used to apply is
                // sCalc's `SMALL` (`sCalcPerform.c:46`), which belongs to the
                // string engine and nowhere else.
                CoreOp::Eq => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| f64::from(u8::from(x == y)))?);
                }
                CoreOp::Ne => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| f64::from(u8::from(x != y)))?);
                }
                CoreOp::Lt => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| f64::from(u8::from(x < y)))?);
                }
                CoreOp::Le => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| f64::from(u8::from(x <= y)))?);
                }
                CoreOp::Gt => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| f64::from(u8::from(x > y)))?);
                }
                CoreOp::Ge => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| f64::from(u8::from(x >= y)))?);
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
                    stack.push(domain_guarded(a, f64::sqrt, &mut status));
                }
                CoreOp::Exp => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.map(|x| x.exp()));
                }
                CoreOp::Log10 => {
                    let a = pop1(&mut stack)?;
                    stack.push(domain_guarded(a, f64::log10, &mut status));
                }
                CoreOp::LogE => {
                    let a = pop1(&mut stack)?;
                    stack.push(domain_guarded(a, f64::ln, &mut status));
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

                // ISNAN and FINITE are aCalc's two VARARG predicates
                // (`aCalcPerform.c:1114-1146`), and each folds over EVERY ELEMENT
                // of EVERY argument — an array argument is not collapsed to its
                // a[0]:
                //   FINITE = AND of finite() over all elements of all args
                //   ISNAN  = OR  of isnan()  over all elements of all args
                // Both reduce to a plain double (`toDouble(ps); ps->d = j`).
                CoreOp::IsNan(nargs) => {
                    let args = popn(&mut stack, *nargs as usize)?;
                    let any_nan = args
                        .iter()
                        .flat_map(ArrayStackValue::elements)
                        .any(f64::is_nan);
                    stack.push(ArrayStackValue::Double(f64::from(u8::from(any_nan))));
                }
                CoreOp::Finite(nargs) => {
                    let args = popn(&mut stack, *nargs as usize)?;
                    let all_finite = args
                        .iter()
                        .flat_map(ArrayStackValue::elements)
                        .all(f64::is_finite);
                    stack.push(ArrayStackValue::Double(f64::from(u8::from(all_finite))));
                }
                // ISINF is NOT a reduction: it is one of aCalc's element-wise unary
                // operators (`aCalcPerform.c:826` in the isArray branch, :1085 in
                // the scalar one), so an array operand yields an ARRAY result with
                // the predicate applied per element.
                CoreOp::IsInf => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.map(|x| f64::from(u8::from(x.is_infinite()))));
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
                    stack.push(unary(v, |a| Double(stats::average(a)), PASS_THROUGH));
                }
                ArrayOp::StdDev => {
                    let v = pop1(&mut stack)?;
                    stack.push(unary(v, |a| Double(stats::std_dev(a)), ZERO));
                }
                ArrayOp::Fwhm => {
                    let v = pop1(&mut stack)?;
                    stack.push(unary(v, |a| Double(stats::fwhm(a)), ZERO));
                }
                ArrayOp::ArraySum => {
                    let v = pop1(&mut stack)?;
                    stack.push(unary(v, |a| Double(a.iter().sum()), PASS_THROUGH));
                }
                ArrayOp::ArrayMax => {
                    let v = pop1(&mut stack)?;
                    stack.push(unary(
                        v,
                        |a| Double(extremum(a).map_or(0.0, |(v, _)| v)),
                        PASS_THROUGH,
                    ));
                }
                ArrayOp::ArrayMin => {
                    let v = pop1(&mut stack)?;
                    stack.push(unary(
                        v,
                        |a| Double(extremum_by(a, |x, best| x < best).map_or(0.0, |(v, _)| v)),
                        PASS_THROUGH,
                    ));
                }
                ArrayOp::IndexMax => {
                    let v = pop1(&mut stack)?;
                    stack.push(unary(
                        v,
                        |a| Double(extremum(a).map_or(0.0, |(_, i)| i as f64)),
                        ZERO,
                    ));
                }
                ArrayOp::IndexMin => {
                    let v = pop1(&mut stack)?;
                    stack.push(unary(
                        v,
                        |a| {
                            Double(
                                extremum_by(a, |x, best| x < best).map_or(0.0, |(_, i)| i as f64),
                            )
                        },
                        ZERO,
                    ));
                }
                ArrayOp::IndexZero => {
                    let v = pop1(&mut stack)?;
                    // C `:1090` — a scalar IS its own element 0, so it "contains a
                    // zero" exactly when it is (near-)zero itself.
                    stack.push(unary(
                        v,
                        |a| Double(index_zero_crossing(a)),
                        |d| if d.abs() < SMALL { 0.0 } else { -1.0 },
                    ));
                }
                ArrayOp::IndexNonZero => {
                    let v = pop1(&mut stack)?;
                    // C `aCalcPerform.c:893-898` thresholds at SMALL — `fabs(a[i])
                    // > SMALL` — it is not an exact `!= 0.0`. Below 1e-9 an element
                    // counts as zero and is skipped. (aCalc's other zero tests —
                    // logical AND/OR/NOT, the conditional, the DIV/MOD zero divisor
                    // — really are C's exact truthiness, so SMALL stays here.)
                    // Scalar: C `:1091`, the mirror image of IXZ's.
                    stack.push(unary(
                        v,
                        |a| {
                            Double(
                                a.iter()
                                    .position(|&x| x.abs() > SMALL)
                                    .map_or(-1.0, |i| i as f64),
                            )
                        },
                        |d| if d.abs() > SMALL { 0.0 } else { -1.0 },
                    ));
                }

                ArrayOp::Smooth => {
                    let v = pop1(&mut stack)?;
                    stack.push(unary(
                        v,
                        |a| ArrayStackValue::Array(stats::smooth(a)),
                        PASS_THROUGH,
                    ));
                }
                ArrayOp::NSmooth => {
                    let n = pop1_f64(&mut stack)? as usize;
                    let v = pop1(&mut stack)?;
                    // NSMOOTH is NOT in C's unary switch — it is its own case
                    // (`aCalcPerform.c:579-591`) and it indexes `ps->a[]` with no
                    // `toArray` first, so a scalar operand dereferences a NULL array
                    // in C. There is no C behaviour to match; refusing is the
                    // deliberate deviation.
                    let arr = v.as_array()?;
                    stack.push(ArrayStackValue::Array(stats::nsmooth(arr, n)));
                }
                ArrayOp::Deriv => {
                    let v = pop1(&mut stack)?;
                    stack.push(unary(
                        v,
                        |a| ArrayStackValue::Array(derivative::deriv(a)),
                        ZERO,
                    ));
                }
                ArrayOp::NDeriv => {
                    let n = pop1_f64(&mut stack)? as usize;
                    let v = pop1(&mut stack)?;
                    // NDERIV is NOT in C's unary switch either (`:594-618`), and its
                    // own case PROMOTES a scalar with `toArray(ps,1)` rather than
                    // answering 0. Refusing here is a divergence from that promotion,
                    // reported separately — it is not R10-4's family.
                    let arr = v.as_array()?;
                    stack.push(ArrayStackValue::Array(derivative::nderiv(arr, n)));
                }
                ArrayOp::Cum => {
                    let v = pop1(&mut stack)?;
                    stack.push(unary(
                        v,
                        |a| {
                            let mut result = a.to_vec();
                            for i in 1..result.len() {
                                result[i] += result[i - 1];
                            }
                            ArrayStackValue::Array(result)
                        },
                        PASS_THROUGH,
                    ));
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

    // C `:1602-1605`: the deferred failure is consumed HERE, after the whole
    // expression has run (and after its stores have landed), and it suppresses the
    // result write entirely.
    if let Some(err) = status {
        return Err(err);
    }

    Ok(stack
        .last()
        .cloned()
        .unwrap_or(ArrayStackValue::Double(0.0)))
}

/// aCalc's three domain-guarded unary operators — `SQRT`/`SQR`, `LOG`, `LN`.
/// Their two branches do NOT agree, and the asymmetry is the contract:
///
/// - **scalar** (`aCalcPerform.c:1044-1072`): a negative operand becomes 0 and
///   the evaluation continues with **no error at all** — C only prints a line.
///   `SQRT(-4)` is 0 with a healthy record.
/// - **array** (`:775-812`): every negative ELEMENT becomes 0 **and** `status` is
///   set to -1, so aCalcPerform ultimately returns -1 without writing
///   p_dresult/p_aresult — the record keeps its previous VAL/AVAL and raises
///   CALC_ALARM/INVALID.
///
/// Neither branch ever yields NaN, which is what a bare `sqrt`/`log` gives and
/// what the port used to produce. Note the guard is `< 0`, so `LOG(0)` is not
/// caught here: it yields -inf, and the record's own non-finite check owns that.
///
/// This is aCalc's rule only. base `calcPerform` takes the bare sqrt/log (NaN,
/// no error) and sCalc returns -1 immediately (`sCalcPerform.c:521-541`); both
/// are already faithful in `numeric.rs` and `string.rs`.
fn domain_guarded(
    v: ArrayStackValue,
    f: fn(f64) -> f64,
    status: &mut Option<CalcError>,
) -> ArrayStackValue {
    match v {
        ArrayStackValue::Double(d) => Double(if d < 0.0 { 0.0 } else { f(d) }),
        ArrayStackValue::Array(a) => {
            let out = a
                .into_iter()
                .map(|x| {
                    if x < 0.0 {
                        *status = Some(CalcError::DomainError);
                        0.0
                    } else {
                        f(x)
                    }
                })
                .collect();
            ArrayStackValue::Array(out)
        }
    }
}

/// C's unary array-operator dispatch (`aCalcPerform.c:769-1101`) is ONE operator
/// with TWO branches, chosen by the operand's shape:
///
/// ```c
/// if (isArray(ps)) { switch (op) { ... } } else { switch (op) { ... } }
/// ```
///
/// The scalar branch is not a degenerate case to be refused — C defines an answer
/// for **every** operator in that switch. `AVG(5)` is 5 and `STD(5)` is 0; there
/// is no type error anywhere in it. Routing all of them through this helper makes
/// the scalar answer a mandatory argument, so an operator cannot be added back
/// with an `as_array()?` that turns a legal expression into CALC_ALARM.
///
/// Only operators that C really lists in that switch belong here. `NSMOOTH`,
/// `NDERIV`, `CAT`, `SUBRANGE` and the `FIT*` family are separate C cases with
/// their own scalar handling and do NOT use this.
fn unary(
    v: ArrayStackValue,
    on_array: impl FnOnce(&[f64]) -> ArrayStackValue,
    on_scalar: impl FnOnce(f64) -> f64,
) -> ArrayStackValue {
    match v {
        ArrayStackValue::Array(a) => on_array(&a),
        ArrayStackValue::Double(d) => ArrayStackValue::Double(on_scalar(d)),
    }
}

/// C `case AVERAGE: break;` — the scalar branch leaves `ps->d` untouched, so the
/// scalar IS the answer. Shared by AVERAGE, SMOOTH, ARRSUM, CUM, AMAX and AMIN.
const PASS_THROUGH: fn(f64) -> f64 = |d| d;

/// C `case STD_DEV: ps->d = 0;` — shared by STD_DEV, FWHM, DERIV, IXMAX and IXMIN.
const ZERO: fn(f64) -> f64 = |_| 0.0;

/// The single shape behind all four aCalc extremum reductions — `AMAX`, `AMIN`,
/// `IXMAX`, `IXMIN` (`aCalcPerform.c:836-861`). C seeds both the running value
/// and the running index from the FIRST element and then advances only on a
/// STRICT comparison:
///
/// ```c
/// for (i=firstEl+1, j=firstEl, d=ps->a[firstEl]; i<=lastEl; i++) {
///     if (ps->a[i] > d) { d = ps->a[i]; j = i; }
/// }
/// ```
///
/// Two observables fall out of that shape, and neither survives an
/// `Iterator::max_by` / `fold(NEG_INFINITY, f64::max)` rewrite:
///
/// - **Ties keep the FIRST winner.** `IXMAX([5,3,5])` is 0, not 2.
/// - **A NaN seed sticks.** Every comparison against NaN is false, so
///   `AMAX([NaN,3,5])` is NaN and `IXMAX` is 0 — where `f64::max` would
///   discard the NaN and answer 5. (A NaN *later* in the array is skipped by
///   the same false comparison, so it never wins.)
///
/// Returns `(value, index)` of the winner, or `None` for an empty array — C
/// cannot reach that case (`arraySize >= 1`), and the callers answer 0 as the
/// port's own empty-array convention.
fn extremum(arr: &[f64]) -> Option<(f64, usize)> {
    extremum_by(arr, |x, best| x > best)
}

fn extremum_by(arr: &[f64], better: impl Fn(f64, f64) -> bool) -> Option<(f64, usize)> {
    let mut best = (*arr.first()?, 0usize);
    for (i, &x) in arr.iter().enumerate().skip(1) {
        if better(x, best.0) {
            best = (x, i);
        }
    }
    Some(best)
}

/// aCalc `IXZ` — the **real (fractional) index of the first zero crossing**
/// (`aCalcPerform.c:879-892`), NOT the index of the first exactly-zero element.
/// The exact-zero reading is C's `#if 0` dead code (`:866-877`).
///
/// ```c
/// for (i=firstEl+1, j=-1, d=0.; i<=lastEl; i++) {
///     if ((ps->a[i]>0) != (ps->a[firstEl]>0)) {
///         j = i-1;
///         d = fabs(ps->a[j])/fabs(ps->a[j]-ps->a[j+1]);
///         break;
///     }
/// }
/// ps->d = j+d;
/// ```
///
/// Two details the shape depends on: the sign test is against the FIRST element,
/// not the previous one, and it is `> 0` — so an exact 0 is grouped with the
/// negatives. `j` and `j+1` therefore always straddle the crossing with
/// different `>0` verdicts, which is why the denominator cannot be zero.
///
/// No crossing answers `-1` (`j=-1`, `d=0`) — including for the common case of
/// an all-positive waveform, where the old exact-zero reading also answered -1
/// but for the wrong reason.
fn index_zero_crossing(arr: &[f64]) -> f64 {
    let Some(&first) = arr.first() else {
        return -1.0;
    };
    for i in 1..arr.len() {
        if (arr[i] > 0.0) != (first > 0.0) {
            let j = i - 1;
            let d = arr[j].abs() / (arr[j] - arr[i]).abs();
            return j as f64 + d;
        }
    }
    -1.0
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

/// Pop the `n` arguments of a VARARG operator, keeping their shapes intact.
fn popn(stack: &mut Vec<ArrayStackValue>, n: usize) -> Result<Vec<ArrayStackValue>, CalcError> {
    if stack.len() < n {
        return Err(CalcError::Underflow);
    }
    Ok(stack.split_off(stack.len() - n))
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
