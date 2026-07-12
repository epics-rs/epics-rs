use super::array_value::ArrayStackValue::Double;
use super::array_value::{ArrayCell, ArrayStackValue, zip_map};
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
    // C `aCalcPerform.c:312-314` — `if (*postfix == END_EXPRESSION) return(-1);`,
    // ahead of even the value-stack allocation. Same contract as the other two
    // engines: an empty or failed compile is a program that fails every run.
    if expr.is_empty() {
        return Err(CalcError::EmptyProgram);
    }

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
                    // In array evaluator, double vars are array vars. C's fresh
                    // push clears the window (`INC`: `ps->numEl = -1`,
                    // `aCalcPerform.c:88`), which is what `ArrayCell::new` gives.
                    let arr = inputs.arrays[*idx as usize].clone();
                    if arr.is_empty() {
                        stack.push(ArrayStackValue::Double(0.0));
                    } else {
                        stack.push(ArrayStackValue::Array(ArrayCell::new(
                            arr,
                            inputs.array_size,
                        )));
                    }
                }

                CoreOp::Pi => stack.push(ArrayStackValue::Double(std::f64::consts::PI)),
                CoreOp::D2R => stack.push(ArrayStackValue::Double(std::f64::consts::PI / 180.0)),
                CoreOp::R2D => stack.push(ArrayStackValue::Double(180.0 / std::f64::consts::PI)),
                // C `CONST_S2R` / `CONST_R2S` (`aCalcPerform.c:559-569`):
                // arcseconds <-> radians, `PI/(180*3600)` and its reciprocal.
                CoreOp::S2R => stack.push(ArrayStackValue::Double(
                    std::f64::consts::PI / (180.0 * 3600.0),
                )),
                CoreOp::R2S => stack.push(ArrayStackValue::Double(
                    (180.0 * 3600.0) / std::f64::consts::PI,
                )),

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
                    stack.push(zip_map(a, b, |x, y| x + y));
                }
                CoreOp::Sub => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| x - y));
                }
                CoreOp::Mul => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| x * y));
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
                    ));
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
                    }));
                }
                CoreOp::Neg => {
                    let a = pop1(&mut stack)?;
                    stack.push(a.map(|x| -x));
                }
                CoreOp::Power => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| x.powf(y)));
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
                    stack.push(zip_map(a, b, |x, y| f64::from(u8::from(x == y))));
                }
                CoreOp::Ne => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| f64::from(u8::from(x != y))));
                }
                CoreOp::Lt => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| f64::from(u8::from(x < y))));
                }
                CoreOp::Le => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| f64::from(u8::from(x <= y))));
                }
                CoreOp::Gt => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| f64::from(u8::from(x > y))));
                }
                CoreOp::Ge => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| f64::from(u8::from(x >= y))));
                }

                // Logical
                CoreOp::And => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(
                        a,
                        b,
                        |x, y| if x != 0.0 && y != 0.0 { 1.0 } else { 0.0 },
                    ));
                }
                CoreOp::Or => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(
                        a,
                        b,
                        |x, y| if x != 0.0 || y != 0.0 { 1.0 } else { 0.0 },
                    ));
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
                    stack.push(zip_map(a, b, |x, y| (c_int(x) & c_int(y)) as f64));
                }
                CoreOp::BitOr => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| (c_int(x) | c_int(y)) as f64));
                }
                CoreOp::BitXor => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| (c_int(x) ^ c_int(y)) as f64));
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
                        ArrayStackValue::Array(mut cell) => {
                            // C negates the count for `<<` (:1431) — a left
                            // shift moves elements DOWN in index. No
                            // `calcFirstLast` in this case (`:1416-1459`): the
                            // move runs over the whole buffer and leaves the
                            // window alone.
                            shift_elements(cell.buf_mut(), if left_shift { -count } else { count });
                            stack.push(ArrayStackValue::Array(cell));
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
                    stack.push(zip_map(a, b, |x, y| (d2ui(x) >> (d2ui(y) & 31)) as f64));
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
                    stack.push(zip_map(a, b, |x, y| y.atan2(x)));
                }
                CoreOp::Fmod => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(zip_map(a, b, |x, y| x % y));
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
                        ArrayStackValue::Array(cell) => {
                            inputs.arrays[*idx as usize] = cell.into_buf();
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
                    stack.push(ArrayStackValue::array(arr));
                }
                ArrayOp::ToArray => {
                    let v = pop1_f64(&mut stack)?;
                    stack.push(ArrayStackValue::array(vec![v; inputs.array_size]));
                }
                ArrayOp::ToDouble => {
                    let v = pop1(&mut stack)?;
                    stack.push(ArrayStackValue::Double(v.as_f64()?));
                }
                ArrayOp::Average => {
                    let v = pop1(&mut stack)?;
                    // C `:929-932`: `d = a[firstEl]; for (i=firstEl+1..lastEl) d +=
                    // a[i]; ps->d = d/(1+lastEl-firstEl)`. The seed is read even when
                    // the window is EMPTY (the read is still in bounds, the loop just
                    // does not run), and the divisor is then 0 — so C answers a[0]/0.
                    // That is why the divisor is `span()` and not `window().len()`.
                    stack.push(unary(
                        v,
                        |c| Double(window_sum(&c) / c.span() as f64),
                        PASS_THROUGH,
                    ));
                }
                ArrayOp::StdDev => {
                    let v = pop1(&mut stack)?;
                    // C `:934-946` — the same seeded sum, then `sqrt(sum(err^2)/(n-1))`
                    // over the window, falling back to `sqrt(sum(err^2))` when the
                    // window holds one element or none.
                    stack.push(unary(
                        v,
                        |c| {
                            let n = c.span();
                            let mean = window_sum(&c) / n as f64;
                            let e: f64 = c.window().iter().map(|x| (x - mean).powi(2)).sum();
                            Double(if n > 1 {
                                (e / (n - 1) as f64).sqrt()
                            } else {
                                e.sqrt()
                            })
                        },
                        ZERO,
                    ));
                }
                ArrayOp::Fwhm => {
                    let v = pop1(&mut stack)?;
                    stack.push(unary(v, |c| Double(stats::fwhm(c.window())), ZERO));
                }
                ArrayOp::ArraySum => {
                    let v = pop1(&mut stack)?;
                    // C `:991-997` seeds `d = 0.0`, unlike AVERAGE — so an empty
                    // window sums to 0 rather than to a[0].
                    stack.push(unary(v, |c| Double(c.window().iter().sum()), PASS_THROUGH));
                }
                ArrayOp::ArrayMax => {
                    let v = pop1(&mut stack)?;
                    stack.push(unary(
                        v,
                        |c| Double(extremum(&c, |x, best| x > best).0),
                        PASS_THROUGH,
                    ));
                }
                ArrayOp::ArrayMin => {
                    let v = pop1(&mut stack)?;
                    stack.push(unary(
                        v,
                        |c| Double(extremum(&c, |x, best| x < best).0),
                        PASS_THROUGH,
                    ));
                }
                ArrayOp::IndexMax => {
                    let v = pop1(&mut stack)?;
                    stack.push(unary(
                        v,
                        |c| Double(extremum(&c, |x, best| x > best).1 as f64),
                        ZERO,
                    ));
                }
                ArrayOp::IndexMin => {
                    let v = pop1(&mut stack)?;
                    stack.push(unary(
                        v,
                        |c| Double(extremum(&c, |x, best| x < best).1 as f64),
                        ZERO,
                    ));
                }
                ArrayOp::IndexZero => {
                    let v = pop1(&mut stack)?;
                    // C `:1090` — a scalar IS its own element 0, so it "contains a
                    // zero" exactly when it is (near-)zero itself.
                    stack.push(unary(
                        v,
                        |c| Double(index_zero_crossing(c.window())),
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
                        |c| {
                            Double(
                                c.window()
                                    .iter()
                                    .position(|&x| x.abs() > SMALL)
                                    .map_or(-1.0, |i| i as f64),
                            )
                        },
                        |d| if d.abs() > SMALL { 0.0 } else { -1.0 },
                    ));
                }

                ArrayOp::Smooth => {
                    let v = pop1(&mut stack)?;
                    // C `:968-975` smooths IN PLACE inside the window and touches
                    // nothing outside it — not even to zero it, which is what NSMOOTH
                    // and DERIV do.
                    stack.push(unary(
                        v,
                        |mut c| {
                            let smoothed = stats::smooth(c.window());
                            c.window_mut().copy_from_slice(&smoothed);
                            ArrayStackValue::Array(c)
                        },
                        PASS_THROUGH,
                    ));
                }
                ArrayOp::NSmooth => {
                    let n = pop1_f64(&mut stack)? as usize;
                    let v = pop1(&mut stack)?;
                    // NSMOOTH is NOT in C's unary switch — it is its own case
                    // (`aCalcPerform.c:579-592`) and it indexes `ps->a[]` with no
                    // `toArray` first, so a scalar operand dereferences a NULL array
                    // in C. There is no C behaviour to match; refusing is the
                    // deliberate deviation.
                    //
                    // It also does NOT honour the operand's window, and that is not an
                    // oversight to tidy away: C calls `calcFirstLast(ps,...)` BEFORE
                    // `DEC(ps)` (`:580-582`) — i.e. on the npts SCALAR, whose numEl is
                    // always the -1 sentinel — so first/last come out as
                    // 0..arraySize-1 whatever window the array carries. Compiled C,
                    // arraySize 7, AA=[1,2,3,40,5,6,7]:
                    //   SMOO(AA[1,5])    -> [2,3,17.5,5,6,0,0]        (window only)
                    //   NSMOO(AA[1,5],1) -> [2,3,17.5,13.5625,6,0,0]  (whole buffer)
                    let mut cell = v.as_cell()?.clone();
                    let smoothed = stats::nsmooth(cell.buf(), n);
                    cell.buf_mut().copy_from_slice(&smoothed);
                    stack.push(ArrayStackValue::Array(cell));
                }
                ArrayOp::Deriv => {
                    let v = pop1(&mut stack)?;
                    // C `:976-989`: the derivative is taken over the window and written
                    // back into it, and everything OUTSIDE the window is zeroed
                    // (`:985-987`).
                    stack.push(unary(
                        v,
                        |mut c| {
                            let d = derivative::deriv(c.window());
                            c.window_mut().copy_from_slice(&d);
                            c.clear_outside_window();
                            ArrayStackValue::Array(c)
                        },
                        ZERO,
                    ));
                }
                ArrayOp::NDeriv => {
                    let n = pop1_f64(&mut stack)? as usize;
                    let v = pop1(&mut stack)?;
                    // NDERIV is NOT in C's unary switch either (`:594-617`), and its
                    // own case PROMOTES a scalar with `toArray(ps,1)` rather than
                    // answering 0. Refusing here is a divergence from that promotion,
                    // reported separately — it is not R10-4's family.
                    //
                    // Unlike NSMOOTH it DOES honour the window: its `calcFirstLast`
                    // runs after `DEC(ps)`, on the array itself (`:600`).
                    let mut cell = v.as_cell()?.clone();
                    let d = derivative::nderiv(cell.window(), n);
                    cell.window_mut().copy_from_slice(&d);
                    cell.clear_outside_window();
                    stack.push(ArrayStackValue::Array(cell));
                }
                ArrayOp::Cum => {
                    let v = pop1(&mut stack)?;
                    // C `:787` — no `calcFirstLast`, so CUM runs over the whole buffer
                    // whatever the window says.
                    stack.push(unary(
                        v,
                        |mut c| {
                            let buf = c.buf_mut();
                            for i in 1..buf.len() {
                                buf[i] += buf[i - 1];
                            }
                            ArrayStackValue::Array(c)
                        },
                        PASS_THROUGH,
                    ));
                }
                ArrayOp::Cat => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    match (a, b) {
                        // C `:1383-1391` (array, double): write the scalar at
                        // `lastEl+1` and grow the window by one — but ONLY if there is
                        // room left in the `arraySize` buffer. A left operand with no
                        // window has `lastEl = arraySize-1`, so the test fails and CAT
                        // is a no-op: compiled C, `CAT(AA,9)` on a full AA is AA.
                        (ArrayStackValue::Array(mut cell), ArrayStackValue::Double(d)) => {
                            let at = cell.span();
                            if at >= 0 && at < cell.buf().len() as i64 {
                                cell.buf_mut()[at as usize] = d;
                                cell.set_num_el(at + 1);
                            }
                            stack.push(ArrayStackValue::Array(cell));
                        }
                        // C `:1359-1365` (array, array), reached after `toArray(ps,1)`
                        // has promoted a scalar LEFT operand: copy the right operand's
                        // WINDOW in after the left one's, stopping at the buffer end,
                        // and set numEl to how far it got. C assigns numEl
                        // unconditionally here, so even a no-copy CAT replaces the
                        // "no window" sentinel with the concrete arraySize.
                        (a, ArrayStackValue::Array(right)) => {
                            let mut cell = a.into_cell(inputs.array_size);
                            let mut i = cell.span().max(0) as usize;
                            for &v in right.window() {
                                if i >= cell.buf().len() {
                                    break;
                                }
                                cell.buf_mut()[i] = v;
                                i += 1;
                            }
                            cell.set_num_el(i as i64);
                            stack.push(ArrayStackValue::Array(cell));
                        }
                        // (double, double) keeps the port's old shape here; C's
                        // `case CAT: break;` (`:1411`) is R11-7's subject.
                        (ArrayStackValue::Double(x), ArrayStackValue::Double(y)) => {
                            stack.push(ArrayStackValue::Array(ArrayCell::new(
                                vec![x, y],
                                inputs.array_size,
                            )));
                        }
                    }
                }
                ArrayOp::ArrayRandom => {
                    let arr: Vec<f64> = (0..inputs.array_size).map(|_| simple_random()).collect();
                    stack.push(ArrayStackValue::array(arr));
                }
                ArrayOp::ArraySubrange | ArrayOp::ArraySubrangeInPlace => {
                    // C `aCalcPerform.c:1519-1548`. Both bounds are INCLUSIVE, a
                    // negative bound counts back from the end, and the result is
                    // still a full `arraySize` buffer: `[` shifts the selected
                    // elements down to index 0 and zero-fills the tail, `{` leaves
                    // them where they are and zeroes everything outside.
                    //
                    // Both then set the WINDOW, which is the entire point of the
                    // operator: `numEl = 1+j-i` for `[`, `numEl = j+1` for `{`
                    // (`:1537-1544`). Compiled C, AA=[10,20,30,40,50,60]:
                    // `AVG(AA[1,3])` is 30 (the three selected elements) while
                    // `AVG(AA{1,3})` is 22.5 (0..3, the zeroed head included).
                    let n = inputs.array_size as i64;
                    let (i, j) = pop_subrange_bounds(&mut stack, n)?;
                    let mut cell = pop1(&mut stack)?.into_cell(inputs.array_size);
                    if matches!(aop, ArrayOp::ArraySubrange) {
                        let src = cell.buf().to_vec();
                        let buf = cell.buf_mut();
                        let mut k = 0usize;
                        for s in i..=j {
                            // C's `j` is clamped to arraySize, not arraySize-1, so its
                            // copy loop can read one element PAST the buffer
                            // (`:1536,:1540`). Stopping is the port's deviation from
                            // that out-of-bounds read; the window below still takes
                            // C's count.
                            match (buf.get_mut(k), src.get(s as usize)) {
                                (Some(dst), Some(&v)) => *dst = v,
                                _ => break,
                            }
                            k += 1;
                        }
                        buf[k..].fill(0.0);
                        cell.set_num_el(1 + j - i);
                    } else {
                        // C: zero `[0,i)` and `(j,arraySize)`, keep the rest in place.
                        for (k, v) in cell.buf_mut().iter_mut().enumerate() {
                            let k = k as i64;
                            if k < i || k > j {
                                *v = 0.0;
                            }
                        }
                        cell.set_num_el(j + 1);
                    }
                    stack.push(ArrayStackValue::Array(cell));
                }
                ArrayOp::ANeg => {
                    // C `:772` (array) / `:1046` (scalar) — ANEG zeroes the
                    // NEGATIVE elements and keeps the rest.
                    let v = pop1(&mut stack)?;
                    stack.push(v.map(|x| if x < 0.0 { 0.0 } else { x }));
                }
                ArrayOp::APos => {
                    // C `:773` (array) / `:1047` (scalar) — APOS zeroes the
                    // POSITIVE elements.
                    let v = pop1(&mut stack)?;
                    stack.push(v.map(|x| if x > 0.0 { 0.0 } else { x }));
                }
                ArrayOp::FetchAval => {
                    // C `FETCH_AVAL` (`:534-539`) — push `p_aresult`, the record's
                    // previous array result. The array counterpart of `VAL`.
                    stack.push(ArrayStackValue::Array(ArrayCell::new(
                        inputs.prev_aval.clone(),
                        inputs.array_size,
                    )));
                }
                ArrayOp::DynFetch => {
                    // C `A_FETCH` (`:1461-1477`) — `@x` is the scalar argument x
                    // INDEXES (`@1` is B). C rounds the index with myNINT and
                    // answers 0 (with a console message) when it is out of range.
                    let idx = my_nint(pop1_f64(&mut stack)?);
                    let v = usize::try_from(idx)
                        .ok()
                        .and_then(|i| inputs.num_vars.get(i).copied())
                        .unwrap_or(0.0);
                    stack.push(ArrayStackValue::Double(v));
                }
                ArrayOp::DynAFetch => {
                    // C `A_AFETCH` (`:1479-1494`) — `@@x` is the ARRAY argument x
                    // indexes (`@@1` is BB). Out of range, and an argument the
                    // record never allocated, are both an all-zero array — and the
                    // result is an array either way (C `toArray(ps,0)`).
                    let idx = my_nint(pop1_f64(&mut stack)?);
                    let arr = usize::try_from(idx)
                        .ok()
                        .and_then(|i| inputs.arrays.get(i))
                        .cloned()
                        .unwrap_or_default();
                    stack.push(ArrayStackValue::Array(ArrayCell::new(
                        arr,
                        inputs.array_size,
                    )));
                }
                ArrayOp::LenNoop => {
                    // C has no `case LEN` and no `default:` in aCalcPerform's
                    // switch (`aCalcPostfix.c:199`: "Array length not
                    // implemented"), so the opcode falls through and the operand
                    // stays on the stack untouched. Compiled C: `LEN(AA)` is AA.
                }
                ArrayOp::FitPoly => {
                    let y = pop1(&mut stack)?;
                    let x = pop1(&mut stack)?;
                    let xa = x.as_cell()?.window().to_vec();
                    let ya = y.as_cell()?.window().to_vec();
                    let (a0, a1, a2) = fitting::fitpoly(&xa, &ya, None);
                    // Return as array [a0, a1, a2]
                    stack.push(ArrayStackValue::array(vec![a0, a1, a2]));
                }
                ArrayOp::FitMPoly => {
                    let mask = pop1(&mut stack)?;
                    let y = pop1(&mut stack)?;
                    let x = pop1(&mut stack)?;
                    let xa = x.as_cell()?.window().to_vec();
                    let ya = y.as_cell()?.window().to_vec();
                    let ma = mask.as_cell()?.window().to_vec();
                    let (a0, a1, a2) = fitting::fitpoly(&xa, &ya, Some(&ma));
                    stack.push(ArrayStackValue::array(vec![a0, a1, a2]));
                }
                ArrayOp::FitQ => {
                    // Like FitPoly but returns quality metric
                    let y = pop1(&mut stack)?;
                    let x = pop1(&mut stack)?;
                    let xa = x.as_cell()?.window().to_vec();
                    let ya = y.as_cell()?.window().to_vec();
                    let (a0, a1, a2) = fitting::fitpoly(&xa, &ya, None);
                    // Compute residual sum of squares
                    let rss: f64 = xa
                        .iter()
                        .zip(ya.iter())
                        .map(|(&xi, &yi)| {
                            let pred = a0 + a1 * xi + a2 * xi * xi;
                            (yi - pred).powi(2)
                        })
                        .sum();
                    stack.push(ArrayStackValue::array(vec![a0, a1, a2, rss]));
                }
                ArrayOp::FitMQ => {
                    let mask = pop1(&mut stack)?;
                    let y = pop1(&mut stack)?;
                    let x = pop1(&mut stack)?;
                    let xa = x.as_cell()?.window().to_vec();
                    let ya = y.as_cell()?.window().to_vec();
                    let ma = mask.as_cell()?.window().to_vec();
                    let (a0, a1, a2) = fitting::fitpoly(&xa, &ya, Some(&ma));
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
                    stack.push(ArrayStackValue::array(vec![a0, a1, a2, rss]));
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
        ArrayStackValue::Array(mut cell) => {
            // Element-wise, so the whole buffer — C's loop is `for (i=0;
            // i<arraySize; i++)` (`:775-812`), with no `calcFirstLast`.
            for x in cell.buf_mut() {
                if *x < 0.0 {
                    *status = Some(CalcError::DomainError);
                    *x = 0.0;
                } else {
                    *x = f(*x);
                }
            }
            ArrayStackValue::Array(cell)
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
///
/// The array branch takes the whole [`ArrayCell`], not a bare slice, precisely so
/// each operator must SAY whether it means the buffer (`buf()`, element-wise) or
/// the active window (`window()`, every reduction). Handing it a slice is what let
/// the reductions silently fold over the zero fill.
fn unary(
    v: ArrayStackValue,
    on_array: impl FnOnce(ArrayCell) -> ArrayStackValue,
    on_scalar: impl FnOnce(f64) -> f64,
) -> ArrayStackValue {
    match v {
        ArrayStackValue::Array(c) => on_array(c),
        ArrayStackValue::Double(d) => ArrayStackValue::Double(on_scalar(d)),
    }
}

/// C's seeded window sum, shared by AVERAGE and STD_DEV (`:929`, `:936`):
///
/// ```c
/// for (i=firstEl+1, d=ps->a[firstEl]; i<=lastEl; i++) {d += ps->a[i];}
/// ```
///
/// The seed `a[firstEl]` is read unconditionally, so an EMPTY window still sums to
/// `a[0]` rather than to 0 — the divergence from `window().iter().sum()` that
/// keeps `AVG(AA[2,1])` a NaN (a[0]/0) instead of a 0/0-free 0.
fn window_sum(cell: &ArrayCell) -> f64 {
    let seed = cell.buf().first().copied().unwrap_or(0.0);
    seed + cell.window().iter().skip(1).sum::<f64>()
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
/// The seed is `a[firstEl]` — the first element of the BUFFER, since `firstEl` is
/// always 0 — and C reads it even when the window is empty (the read is in bounds;
/// only the loop is skipped). So `AMAX` of an empty window is `a[0]` and `IXMAX`
/// is `firstEl`, not a zero conjured for the empty case.
///
/// Returns `(value, index)` of the winner; the index is absolute, which is again
/// `firstEl == 0`'s doing.
fn extremum(cell: &ArrayCell, better: impl Fn(f64, f64) -> bool) -> (f64, usize) {
    let mut best = (cell.buf().first().copied().unwrap_or(0.0), 0usize);
    for (i, &x) in cell.window().iter().enumerate().skip(1) {
        if better(x, best.0) {
            best = (x, i);
        }
    }
    best
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

/// Pop the two bounds of an aCalc subrange. C `toDouble`s both
/// (`aCalcPerform.c:1526,1530`) — so an ARRAY bound collapses to its first
/// element — and casts each with a truncating `(int)`, not `myNINT`. The rest of
/// the rule is [`super::subrange_bounds`], shared with sCalc.
fn pop_subrange_bounds(
    stack: &mut Vec<ArrayStackValue>,
    array_size: i64,
) -> Result<(i64, i64), CalcError> {
    let j = pop1_f64(stack)? as i64;
    let i = pop1_f64(stack)? as i64;
    Ok(super::subrange_bounds(i, j, array_size))
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
