use super::array_value::ArrayStackValue::Double;
use super::array_value::{ArrayCell, ArrayStackValue, zip_map};
use super::cast::{c_int, c_long, d2ui, imod, my_nint, nint};
use super::error::CalcError;
use super::opcodes::{ArrayOp, ControlOp, CoreOp, Opcode};
use super::random::local_random;
use super::{ArrayInputs, CompiledExpr};
use crate::calc::math::{derivative, fitting, stats};

/// C `myMAXFLOAT` (`aCalcPerform.c:49`): `((float)1e+35)`, widened back to a
/// double when it lands in the stack cell — so it is the f32-rounded value,
/// not `1e35` exactly. aCalc's MODULO uses it for a zero divisor where base
/// uses NaN and sCalc returns an error.
const MY_MAXFLOAT: f64 = 1e35f32 as f64;

/// C `volatile int aCalcLoopMax = 1000` (`aCalcPerform.c:70`), exported to the ioc
/// shell with `epicsExportAddress(int, aCalcLoopMax)` — a settable global, not a
/// constant, and a SEPARATE one from `sCalcLoopMax`. It bounds the TOTAL number of
/// `UNTIL_END` arrivals in one perform, across every UNTIL in the program, and
/// running out is not an error: C stops looping and carries on (`:1571`).
static ACALC_LOOP_MAX: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1000);

/// Read `aCalcLoopMax`.
pub fn acalc_loop_max() -> i32 {
    ACALC_LOOP_MAX.load(std::sync::atomic::Ordering::Relaxed)
}

/// Write `aCalcLoopMax` — the ioc-shell `var aCalcLoopMax, <n>`.
pub fn set_acalc_loop_max(n: i32) {
    ACALC_LOOP_MAX.store(n, std::sync::atomic::Ordering::Relaxed);
}

/// C's `stackElement` (`aCalcPerform.c:74-80`) as the evaluator sees it: the value
/// — which is itself a buffer plus a window, see [`ArrayCell`] — plus the third
/// field, the PROVENANCE:
///
/// ```c
/// int sourceDouble; /* number of double argument from which this stack element was copied */
/// ```
///
/// Only two operators write it — `FETCH_A..FETCH_P` (`:434`) and `A_FETCH`, i.e.
/// `@x` (`:1475`) — and `INC` clears it on every fresh push (`:94`). Only two read
/// it: FITQ and FITMQ, which is the whole reason it exists — they store the fitted
/// coefficients back into the SCALAR ARGUMENTS the caller named, and provenance is
/// how they learn which arguments those were (`:1201-1207`, `:1245-1251`).
///
/// Deliberate deviation: in C `sourceDouble` also survives the in-place operators
/// (every operator mutates its left operand's cell), so `FITQ(AA, C+0, ...)` still
/// stores the constant term into C. Here provenance is dropped by every operator,
/// so only a bare `A..P` or `@x` names a target — which is what an expression that
/// means to name one writes.
#[derive(Debug, Clone)]
struct Cell {
    v: ArrayStackValue,
    src: Option<usize>,
}

impl From<ArrayStackValue> for Cell {
    /// C's `INC`: a fresh cell has no provenance.
    fn from(v: ArrayStackValue) -> Self {
        Cell { v, src: None }
    }
}

pub fn eval(expr: &CompiledExpr, inputs: &mut ArrayInputs) -> Result<ArrayStackValue, CalcError> {
    // C `aCalcPerform.c:312-314` — `if (*postfix == END_EXPRESSION) return(-1);`,
    // ahead of even the value-stack allocation. Same contract as the other two
    // engines: an empty or failed compile is a program that fails every run.
    if expr.is_empty() {
        return Err(CalcError::EmptyProgram);
    }

    // C's static UNTIL pre-scan (`:355-390`), which runs before any opcode does and
    // fails the perform outright when the postfix holds more than nine of them.
    expr.check_until_ceiling()?;

    // C `:326` — `*amask = 0`, before a single opcode runs. The mask means "array
    // fields THIS run stored into", so the engine that runs it is its only owner: no
    // caller may pre-set it, and every caller may trust it afterwards.
    inputs.amask = 0;

    let mut stack: Vec<Cell> = Vec::with_capacity(20);
    let code = &expr.code;
    let mut pc = 0;

    let mut status = Status::default();

    // C's `until_scratch[MAX_UNTIL_OP]` (`aCalcPerform.c:307`): for each UNTIL, the
    // stack pointer it last saw. `loopsDone` (`:311`) is shared by all of them.
    let mut until_marks: Vec<(usize, usize)> = Vec::new();
    let mut loops_done: i32 = 0;

    while pc < code.len() {
        let op = &code[pc];
        pc += 1;

        match op {
            Opcode::Core(core) => match core {
                CoreOp::End => break,

                CoreOp::PushConst(v) => push(&mut stack, Double(*v)),
                CoreOp::PushVar(idx) => {
                    // C `FETCH_A..FETCH_P` (`:429-438`) — the ONE place that gives a
                    // cell its `sourceDouble`, which is what lets FITQ/FITMQ store a
                    // coefficient back into the argument the caller named.
                    //
                    // Bounded by the CALLER's `num_dArgs`: an arg the caller never
                    // supplied pushes 0 and names NOTHING (`:436`, the else branch,
                    // does not set `sourceDouble`).
                    let i = *idx as usize;
                    match inputs.num_arg(i) {
                        Some(v) => push_src(&mut stack, Double(v), Some(i)),
                        None => push(&mut stack, Double(0.0)),
                    }
                }
                CoreOp::PushDoubleVar(idx) => {
                    // In the array evaluator, double vars are array vars. C's
                    // `FETCH_AA..FETCH_LL` (`aCalcPerform.c:440-454`) opens with
                    // `INC(ps); toArray(ps,0);` — so the cell is an `arraySize`
                    // ARRAY *before* a single element is read, and it stays one
                    // whatever the record holds:
                    //
                    // ```c
                    // INC(ps); toArray(ps,0); ps->a[0] = 0.;
                    // if (num_aArgs > i) {
                    //     if (pp_aArg[i]) { for (i=0;i<arraySize;i++) ps->a[i] = pp_aArg[i][i]; }
                    //     else            { for (i=0;i<arraySize;i++) ps->a[i] = 0.0; }
                    // }
                    // ```
                    //
                    // An array field the record never allocated — C's `pp_aArg[i] ==
                    // NULL` — is an arraySize buffer of ZEROS, NOT the scalar 0. The
                    // port keyed off an empty `Vec` and pushed `Double(0.0)`, which
                    // gave the operators above it a scalar operand and so the wrong
                    // BRANCH of every shape-sensitive one. Compiled C, arraySize 6,
                    // with pp_aArg[0] = NULL:
                    //
                    //   IXZ(AA)  -> -1   (the array branch finds no zero CROSSING)
                    //                    port: 0, from the scalar branch's `|d| < SMALL`
                    //   FWHM(AA) -> 5    (the array branch's no-crossing fallback, lastEl)
                    //                    port: 0, from the scalar branch's ZERO
                    //
                    // `ArrayCell::new` resizes to `array_size`, so it IS C's
                    // `toArray(ps,0)` plus the zero fill. Going through it
                    // unconditionally is what removes the dual meaning: an empty
                    // `Vec` in `inputs.arrays` no longer means "a scalar", it means
                    // exactly what C means by a NULL record array. `@@x`
                    // (`ArrayOp::DynAFetch`) and `AVAL` (`ArrayOp::FetchAval`)
                    // already fetch this way; this makes the named fetch agree.
                    //
                    // C's fresh push also clears the window (`INC`: `ps->numEl = -1`,
                    // `:93`), which `ArrayCell::new` gives, and leaves `sourceDouble`
                    // at -1 — only the SCALAR fetches set it.
                    //
                    // The `if (num_aArgs > i)` guard is the same statement one level
                    // up: an arg the CALLER never supplied is not distinguishable
                    // from one the record never allocated — both are the zero buffer.
                    let arr = inputs.array_arg(*idx as usize).cloned().unwrap_or_default();
                    push(
                        &mut stack,
                        ArrayStackValue::Array(ArrayCell::new(arr, inputs.array_size)),
                    );
                }

                CoreOp::Pi => push(&mut stack, Double(std::f64::consts::PI)),
                CoreOp::D2R => push(&mut stack, Double(std::f64::consts::PI / 180.0)),
                CoreOp::R2D => push(&mut stack, Double(180.0 / std::f64::consts::PI)),
                // C `CONST_S2R` / `CONST_R2S` (`aCalcPerform.c:559-569`):
                // arcseconds <-> radians, `PI/(180*3600)` and its reciprocal.
                CoreOp::S2R => push(&mut stack, Double(std::f64::consts::PI / (180.0 * 3600.0))),
                CoreOp::R2S => push(&mut stack, Double((180.0 * 3600.0) / std::f64::consts::PI)),

                CoreOp::Random => push(&mut stack, Double(local_random())),
                CoreOp::NormalRandom => {
                    let u1 = local_random();
                    let u2 = local_random();
                    let n = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
                    push(&mut stack, Double(n));
                }
                CoreOp::FetchVal => {
                    // C FETCH_VAL pushes *presult (the record's previous result).
                    push(&mut stack, Double(inputs.prev_val));
                }
                CoreOp::FetchSval => {
                    // aCalc has no SVAL: `aCalcPostfix`'s element table never
                    // emits FETCH_SVAL and `aCalcPerform` has no string result
                    // to push. Reachable only through the port's shared
                    // tokenizer, and rejected like the other string-only opcodes.
                    return Err(CalcError::Internal);
                }

                // Type-aware arithmetic via zip_map
                CoreOp::Add => binary(&mut stack, |a, b| zip_map(a, b, |x, y| x + y))?,
                CoreOp::Sub => binary(&mut stack, |a, b| zip_map(a, b, |x, y| x - y))?,
                CoreOp::Mul => binary(&mut stack, |a, b| zip_map(a, b, |x, y| x * y))?,
                CoreOp::Div => {
                    // aCalc, not base (`aCalcPerform.c:636-643`, :659-667,
                    // :690-696): a zero divisor is `myMAXFLOAT` in all three
                    // operand shapes — array/array, array/scalar and
                    // scalar/scalar — never NaN and never an error. (C's
                    // scalar/array shape promotes the scalar with
                    // `toArray(ps,1)` and then runs the array/array loop, so
                    // the per-element test below covers it too.)
                    binary(&mut stack, |a, b| {
                        zip_map(a, b, |x, y| if y == 0.0 { MY_MAXFLOAT } else { x / y })
                    })?
                }
                CoreOp::Mod => {
                    // aCalc, not base (`aCalcPerform.c:645-652`, :669-677,
                    // :697-703): a zero divisor is neither NaN nor an error — it
                    // is `myMAXFLOAT`. That rule is C's and is kept. aCalc's
                    // dialect narrowing is `(int)` (32-bit), so `cast::imod` is
                    // given `c_int`; `cast::imod` owns the -1 guard. CBUG-A2.
                    binary(&mut stack, |a, b| {
                        zip_map(a, b, |x, y| {
                            imod(x, y, |v| i64::from(c_int(v))).unwrap_or(MY_MAXFLOAT)
                        })
                    })?
                }
                CoreOp::Neg => unary_op(&mut stack, |a| a.map(|x| -x))?,
                // POWER is NOT one of C's two-arg array operators. It is its own
                // case (`aCalcPerform.c:1306-1317`), and it is the only binary
                // operator in aCalc that does not pair element-wise:
                //
                // ```c
                // case POWER:
                //     ps1 = ps; DEC(ps);
                //     toDouble(ps1);                            /* <- the EXPONENT collapses */
                //     if (isArray(ps)) { for (i=0; i<arraySize; i++) ps->a[i] = pow(ps->a[i], ps1->d); }
                //     else             { ps->d = pow(ps->d, ps1->d); }
                // ```
                //
                // So the exponent is always a SCALAR — an array exponent becomes its
                // a[0] and the rest of it is never read — and the LEFT operand alone
                // decides the result's shape. Compiled C, arraySize 6:
                //
                //   AA=[1,2,3,4,5,6]            `2**AA`  -> the SCALAR 2 (= pow(2, AA[0]))
                //   AA=[1,2,3,4,5,6]            `AA**2`  -> [1,4,9,16,25,36]
                //   AA=[1..6], CC=[3,1,1,1,1,1] `AA**CC` -> [1,8,27,64,125,216], i.e. AA**3
                //
                // `zip_map` promoted the left operand instead, so `2**AA` came out as
                // the array [2,4,8,16,32,64]: both the VALUE and the SHAPE of the
                // result diverged, and a record's VAL and AVAL diverged with them.
                //
                // `map` is C's in-place element-wise write, so the left operand's
                // window survives — compiled C, AA=[1,5,2,8,3,9]: `(AA[1,3])**2` is
                // [25,4,64,0,0,0] and `AVG((AA[1,3])**2)` is 31, the 3-element
                // window's own average.
                CoreOp::Power => {
                    let exp = pop(&mut stack)?.v.as_f64()?;
                    unary_op(&mut stack, |a| a.map(|x| x.powf(exp)))?
                }

                // Comparison (element-wise for arrays). aCalc compares EXACTLY —
                // C's operators are the bare C ones in all three operand shapes
                // (`aCalcPerform.c:1345-1350` array/array, :1370-1375
                // array/scalar, :1397-1402 scalar/scalar) and aCalcPerform has
                // no epsilon anywhere. The 1e-11 these arms used to apply is
                // sCalc's `SMALL` (`sCalcPerform.c:46`), which belongs to the
                // string engine and nowhere else.
                CoreOp::Eq => binary(&mut stack, |a, b| {
                    zip_map(a, b, |x, y| f64::from(u8::from(x == y)))
                })?,
                CoreOp::Ne => binary(&mut stack, |a, b| {
                    zip_map(a, b, |x, y| f64::from(u8::from(x != y)))
                })?,
                CoreOp::Lt => binary(&mut stack, |a, b| {
                    zip_map(a, b, |x, y| f64::from(u8::from(x < y)))
                })?,
                CoreOp::Le => binary(&mut stack, |a, b| {
                    zip_map(a, b, |x, y| f64::from(u8::from(x <= y)))
                })?,
                CoreOp::Gt => binary(&mut stack, |a, b| {
                    zip_map(a, b, |x, y| f64::from(u8::from(x > y)))
                })?,
                CoreOp::Ge => binary(&mut stack, |a, b| {
                    zip_map(a, b, |x, y| f64::from(u8::from(x >= y)))
                })?,

                // Logical
                CoreOp::And => binary(&mut stack, |a, b| {
                    zip_map(a, b, |x, y| if x != 0.0 && y != 0.0 { 1.0 } else { 0.0 })
                })?,
                CoreOp::Or => binary(&mut stack, |a, b| {
                    zip_map(a, b, |x, y| if x != 0.0 || y != 0.0 { 1.0 } else { 0.0 })
                })?,
                CoreOp::Not => {
                    unary_op(&mut stack, |a| a.map(|x| if x == 0.0 { 1.0 } else { 0.0 }))?
                }

                // Bitwise (element-wise). aCalc has no `d2i`: every operand
                // takes a plain `(int)` cast (`aCalcPerform.c:907`,
                // :1355-1357, :1380-1382, :1407-1409, :1424-1427). The shift
                // count is unmasked in C — x86-64 `shl`/`sar` mask it to 5
                // bits for a 32-bit operand, which is the observable.
                CoreOp::BitAnd => binary(&mut stack, |a, b| {
                    zip_map(a, b, |x, y| (c_int(x) & c_int(y)) as f64)
                })?,
                CoreOp::BitOr => binary(&mut stack, |a, b| {
                    zip_map(a, b, |x, y| (c_int(x) | c_int(y)) as f64)
                })?,
                CoreOp::BitXor => binary(&mut stack, |a, b| {
                    zip_map(a, b, |x, y| (c_int(x) ^ c_int(y)) as f64)
                })?,
                CoreOp::BitNot => unary_op(&mut stack, |a| a.map(|x| !c_int(x) as f64))?,
                // `<<`/`>>` are ONE arm in C (`aCalcPerform.c:1416-1459`) and
                // the LEFT operand's type picks the whole meaning:
                //   scalar left  -> a bit shift by the `(int)` count (:1421-1427)
                //   array  left  -> a POSITIONAL move of the elements, NOT a
                //                   bitwise anything (:1428-1458)
                // The count is collapsed to a double either way (`toDouble(ps1)`,
                // :1420 — an array count becomes its `a[0]`, `to_double` :121).
                CoreOp::Shl | CoreOp::Shr => {
                    let left_shift = matches!(core, CoreOp::Shl);
                    let b = pop(&mut stack)?;
                    let count = b.v.as_f64()?;
                    unary_op(&mut stack, |a| match a {
                        Double(x) => {
                            let n = c_int(count) & 31;
                            let v = if left_shift {
                                c_int(x) << n
                            } else {
                                c_int(x) >> n
                            };
                            Double(v as f64)
                        }
                        ArrayStackValue::Array(mut cell) => {
                            // C negates the count for `<<` (:1431) — a left
                            // shift moves elements DOWN in index. No
                            // `calcFirstLast` in this case (`:1416-1459`): the
                            // move runs over the whole buffer and leaves the
                            // window alone.
                            shift_elements(cell.buf_mut(), if left_shift { -count } else { count });
                            ArrayStackValue::Array(cell)
                        }
                    })?
                }
                // `>>>` (RIGHT_SHIFT_LOGIC) is a BASE opcode; aCalcPostfix has
                // no such element, so no aCalc expression can contain one and
                // there is no aCalc semantics to match. Base's is kept for the
                // shared `CoreOp`; the grammar is what must refuse it.
                CoreOp::ShrLogical => binary(&mut stack, |a, b| {
                    zip_map(a, b, |x, y| (d2ui(x) >> (d2ui(y) & 31)) as f64)
                })?,

                // Conditional
                CoreOp::CondIf => {
                    let cond = pop_f64(&mut stack)?;
                    if cond == 0.0 {
                        pc = cond_search(code, pc, true)?;
                    }
                }
                CoreOp::CondElse => {
                    pc = cond_search(code, pc, false)?;
                }
                CoreOp::CondEnd => {}

                // Unary math functions (element-wise)
                // C `aCalcPerform.c:771` (array branch) / `:1040` (scalar) — a
                // conditional negate, NOT `fabs`, and element-wise over the whole
                // buffer. See [`super::abs_val`].
                CoreOp::Abs => unary_op(&mut stack, |a| a.map(super::abs_val))?,
                CoreOp::Sqrt => {
                    unary_op(&mut stack, |a| domain_guarded(a, f64::sqrt, &mut status))?
                }
                CoreOp::Exp => unary_op(&mut stack, |a| a.map(f64::exp))?,
                CoreOp::Log10 => {
                    unary_op(&mut stack, |a| domain_guarded(a, f64::log10, &mut status))?
                }
                CoreOp::LogE => unary_op(&mut stack, |a| domain_guarded(a, f64::ln, &mut status))?,
                CoreOp::Sin => unary_op(&mut stack, |a| a.map(f64::sin))?,
                CoreOp::Cos => unary_op(&mut stack, |a| a.map(f64::cos))?,
                CoreOp::Tan => unary_op(&mut stack, |a| a.map(f64::tan))?,
                CoreOp::Asin => unary_op(&mut stack, |a| a.map(f64::asin))?,
                CoreOp::Acos => unary_op(&mut stack, |a| a.map(f64::acos))?,
                CoreOp::Atan => unary_op(&mut stack, |a| a.map(f64::atan))?,
                CoreOp::Sinh => unary_op(&mut stack, |a| a.map(f64::sinh))?,
                CoreOp::Cosh => unary_op(&mut stack, |a| a.map(f64::cosh))?,
                CoreOp::Tanh => unary_op(&mut stack, |a| a.map(f64::tanh))?,
                CoreOp::Ceil => unary_op(&mut stack, |a| a.map(f64::ceil))?,
                CoreOp::Floor => unary_op(&mut stack, |a| a.map(f64::floor))?,
                CoreOp::Nint => {
                    // aCalc `aCalcPerform.c:839` (array) and :1096 (scalar) narrow
                    // NINT with a plain `(long)` (64-bit) — WIDER than aCalc's
                    // `(int)` MODULO above (an internal synApps inconsistency), so
                    // `cast::nint` is given `c_long`, and `NINT(3e9) = 3e9`. CBUG-A2.
                    unary_op(&mut stack, |a| a.map(|x| nint(x, c_long)))?
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
                    push(&mut stack, Double(f64::from(u8::from(any_nan))));
                }
                CoreOp::Finite(nargs) => {
                    let args = popn(&mut stack, *nargs as usize)?;
                    let all_finite = args
                        .iter()
                        .flat_map(ArrayStackValue::elements)
                        .all(f64::is_finite);
                    push(&mut stack, Double(f64::from(u8::from(all_finite))));
                }
                // ISINF is NOT a reduction: it is one of aCalc's element-wise unary
                // operators (`aCalcPerform.c:826` in the isArray branch, :1085 in
                // the scalar one), so an array operand yields an ARRAY result with
                // the operator applied per element.
                CoreOp::IsInf => unary_op(&mut stack, |a| a.map(super::isinf))?,

                CoreOp::Atan2 => binary(&mut stack, |a, b| zip_map(a, b, |x, y| y.atan2(x)))?,
                CoreOp::Fmod => binary(&mut stack, |a, b| zip_map(a, b, |x, y| x % y))?,

                // The vararg `MAX()`/`MIN()` — C `:1155-1191`. See [`vararg_extremum`].
                CoreOp::Max(nargs) => vararg_extremum(
                    &mut stack,
                    *nargs as usize,
                    inputs.array_size,
                    Extremum::Max,
                )?,
                CoreOp::Min(nargs) => vararg_extremum(
                    &mut stack,
                    *nargs as usize,
                    inputs.array_size,
                    Extremum::Min,
                )?,
                // `>?` and `<?` are ordinary members of C's two-arg dispatch
                // (`aCalcPerform.c:1326-1327` lists MAX_VAL/MIN_VAL alongside the
                // comparisons), so an array operand on EITHER side yields an
                // element-wise ARRAY. The port collapsed both operands to their
                // a[0] and answered a scalar, which a record then broadcast into
                // AVAL. [`max_val`]/[`min_val`] carry the one formula C uses in all
                // three operand shapes; `zip_map` is C's promotion.
                CoreOp::MaxVal => binary(&mut stack, |a, b| zip_map(a, b, max_val))?,
                CoreOp::MinVal => binary(&mut stack, |a, b| zip_map(a, b, min_val))?,

                CoreOp::StoreVar(idx) => {
                    // C `STORE_A..STORE_P` (`:456-464`) — `p_dArg[i] = ps->d`, a write
                    // straight into the record's A..P field, under `if (num_dArgs >
                    // (op - STORE_A))`. Past the caller's count the value is popped
                    // and dropped.
                    let v = pop_f64(&mut stack)?;
                    if let Some(slot) = inputs.num_arg_mut(*idx as usize) {
                        *slot = v;
                    }
                }
                CoreOp::StoreDoubleVar(idx) => {
                    // C `STORE_AA..STORE_LL` (`:466-491`): the WHOLE arraySize buffer
                    // is written into the record's array field, and the field is then
                    // MARKED — `*amask |= 1<<i` — which is how the record learns to
                    // post it.
                    //
                    // A scalar value is BROADCAST across the buffer (`pd[j] = ps->d`,
                    // `:485`) and not run through `toArray`, so — unlike everywhere
                    // else in aCalc — a NaN scalar stores NaN rather than zeros.
                    // The port used to write the scalar into the SCALAR variable of the
                    // same index instead, so `AA := 5` set A.
                    //
                    // Store and mask both sit under `if (num_aArgs > i)` — hence
                    // [`ArrayInputs::store_array`], which owns the pair.
                    let i = *idx as usize;
                    let v = pop(&mut stack)?.v;
                    let buf = match v {
                        ArrayStackValue::Array(cell) => cell.into_buf(),
                        Double(d) => vec![d; inputs.array_size],
                    };
                    inputs.store_array(i, buf);
                }
            },

            Opcode::Array(aop) => match aop {
                ArrayOp::ConstIndex => {
                    let arr: Vec<f64> = (0..inputs.array_size).map(|i| i as f64).collect();
                    push(&mut stack, ArrayStackValue::array(arr));
                }
                ArrayOp::ToArray => {
                    // C `:1516-1518` — `ARR(x)` is exactly `toArray(ps,1)`, the same
                    // promotion every other operator uses, applied in place:
                    //
                    //   * an ARRAY operand is left alone. `toArray` tests `isDouble`
                    //     first (`:145`), so the buffer, the window and the provenance
                    //     all survive. Compiled C, AA=[1,2,3,4]: `ARR(AA)` is
                    //     [1,2,3,4] and `AVG(ARR(AA[1,2]))` is 2.5, the window's own
                    //     average. The port popped the operand as an f64, which
                    //     collapsed the array to its a[0] and rebroadcast THAT — so
                    //     `ARR(AA)` came out [1,1,1,1] and the window was lost.
                    //   * a SCALAR operand is broadcast with the NaN->0 fill
                    //     (`to_array` `:135-137`). Compiled C, arraySize 4, A=NaN:
                    //     `ARR(A)` is [0,0,0,0]. The port copied the NaN verbatim, and
                    //     a record then published an all-NaN AVAL.
                    //
                    // `into_cell` IS that promotion, so both properties come from the
                    // one rule rather than from this arm re-deriving them.
                    let array_size = inputs.array_size;
                    unary_op(&mut stack, |v| {
                        ArrayStackValue::Array(v.into_cell(array_size))
                    })?
                }
                ArrayOp::ToDouble => {
                    let v = pop(&mut stack)?.v;
                    push(&mut stack, Double(v.as_f64()?));
                }
                ArrayOp::Average => {
                    // C `:929-932`: `d = a[firstEl]; for (i=firstEl+1..lastEl) d +=
                    // a[i]; ps->d = d/(1+lastEl-firstEl)`. The seed is read even when
                    // the window is EMPTY (the read is still in bounds, the loop just
                    // does not run), and the divisor is then 0 — so C answers a[0]/0.
                    // That is why the divisor is `span()` and not `window().len()`.
                    unary_op(&mut stack, |v| {
                        unary(
                            v,
                            |c| Double(window_sum(&c) / c.span() as f64),
                            PASS_THROUGH,
                        )
                    })?
                }
                ArrayOp::StdDev => {
                    // C `:934-946` — the same seeded sum, then `sqrt(sum(err^2)/(n-1))`
                    // over the window, falling back to `sqrt(sum(err^2))` when the
                    // window holds one element or none.
                    unary_op(&mut stack, |v| {
                        unary(
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
                        )
                    })?
                }
                // FWHM takes the BUFFER and `lastEl`, not a window slice: C seeds it
                // from `a[firstEl]` even when the window is EMPTY (an in-bounds read),
                // and its no-crossing fallback is `lastEl` itself — so an empty window
                // answers -1, which a slice could not express.
                ArrayOp::Fwhm => unary_op(&mut stack, |v| {
                    unary(v, |c| Double(stats::fwhm(c.buf(), c.last_el())), ZERO)
                })?,
                ArrayOp::ArraySum => {
                    // C `:991-997` seeds `d = 0.0`, unlike AVERAGE — so an empty
                    // window sums to 0 rather than to a[0].
                    unary_op(&mut stack, |v| {
                        unary(v, |c| Double(c.window().iter().sum()), PASS_THROUGH)
                    })?
                }
                ArrayOp::ArrayMax => unary_op(&mut stack, |v| {
                    unary(
                        v,
                        |c| Double(extremum(&c, |x, best| x > best).0),
                        PASS_THROUGH,
                    )
                })?,
                ArrayOp::ArrayMin => unary_op(&mut stack, |v| {
                    unary(
                        v,
                        |c| Double(extremum(&c, |x, best| x < best).0),
                        PASS_THROUGH,
                    )
                })?,
                ArrayOp::IndexMax => unary_op(&mut stack, |v| {
                    unary(
                        v,
                        |c| Double(extremum(&c, |x, best| x > best).1 as f64),
                        ZERO,
                    )
                })?,
                ArrayOp::IndexMin => unary_op(&mut stack, |v| {
                    unary(
                        v,
                        |c| Double(extremum(&c, |x, best| x < best).1 as f64),
                        ZERO,
                    )
                })?,
                ArrayOp::IndexZero => {
                    // C `:1090` — a scalar IS its own element 0, so it "contains a
                    // zero" exactly when it is (near-)zero itself.
                    unary_op(&mut stack, |v| {
                        unary(
                            v,
                            |c| Double(index_zero_crossing(c.window())),
                            |d| if d.abs() < SMALL { 0.0 } else { -1.0 },
                        )
                    })?
                }
                ArrayOp::IndexNonZero => {
                    // C `aCalcPerform.c:893-898` thresholds at SMALL — `fabs(a[i])
                    // > SMALL` — it is not an exact `!= 0.0`. Below 1e-9 an element
                    // counts as zero and is skipped. (aCalc's other zero tests —
                    // logical AND/OR/NOT, the conditional, the DIV/MOD zero divisor
                    // — really are C's exact truthiness, so SMALL stays here.)
                    // Scalar: C `:1091`, the mirror image of IXZ's.
                    unary_op(&mut stack, |v| {
                        unary(
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
                        )
                    })?
                }

                ArrayOp::Smooth => {
                    // C `:968-975` smooths IN PLACE inside the window and touches
                    // nothing outside it — not even to zero it, which is what NSMOOTH
                    // and DERIV do.
                    unary_op(&mut stack, |v| {
                        unary(
                            v,
                            |mut c| {
                                let smoothed = stats::smooth(c.window());
                                c.window_mut().copy_from_slice(&smoothed);
                                ArrayStackValue::Array(c)
                            },
                            PASS_THROUGH,
                        )
                    })?
                }
                ArrayOp::NSmooth => {
                    // The pass count is a narrowing of a stack double like any
                    // other — C is `int j; ... j = ps->d;` (`aCalcPerform.c:304`
                    // declares `j`, `:581` assigns it), an implicit `(int)`, and
                    // the loop `for (k=firstEl; k<j+firstEl; k++)` then runs
                    // `max(j,0)` passes. So it goes through the cast owner
                    // [`c_int`], not an open-coded `as usize`.
                    //
                    // A count no `int` can hold is not a count, and neither
                    // compiled C target gives a usable answer for one (x86-64
                    // yields `INT32_MIN` -> zero passes; aarch64 saturates ->
                    // `INT32_MAX` passes, which does not return). The port does not
                    // pick between those: `NSMOO(AA,2e9)` is an unbounded loop with
                    // a PERFECT cast, so the count is not what bounds this — see
                    // [`stats::nsmooth`], which stops as soon as a pass stops
                    // changing the buffer. That is exactly the value the full count
                    // would have left, because `smooth` is a pure function of its
                    // input: once it reproduces its own input, every later pass is
                    // a no-op.
                    let n = c_int(pop_f64(&mut stack)?).max(0) as usize;
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
                    try_unary_op(&mut stack, |v| {
                        let mut cell = v.as_cell()?.clone();
                        let smoothed = stats::nsmooth(cell.buf(), n);
                        cell.buf_mut().copy_from_slice(&smoothed);
                        Ok(ArrayStackValue::Array(cell))
                    })?
                }
                ArrayOp::Deriv => {
                    // C `:976-989`: `deriv()` IS `nderiv(..., npts=2, ...)`
                    // (`calcUtil.c:71-75`), so DERIV and NDERIV go through one kernel —
                    // window in, window out, outside zeroed, status assigned.
                    //
                    // DERIV is in the unary switch, so a scalar takes the scalar branch
                    // (`case DERIV: ps->d = 0;`) and never touches status.
                    unary_op(&mut stack, |v| {
                        unary(
                            v,
                            |mut c| {
                                derivative_into_window(&mut c, 2, &mut status);
                                ArrayStackValue::Array(c)
                            },
                            ZERO,
                        )
                    })?
                }
                ArrayOp::NDeriv => {
                    // C `:596` — `j = ps->d`, a truncating int cast, and the value is
                    // the number of points on EACH SIDE of the fit.
                    let npts = i64::from(c_int(pop_f64(&mut stack)?));
                    // NDERIV is NOT in C's unary switch (`:594-617`), so it never
                    // reaches the `ps->d = 0` scalar arm. Its own case PROMOTES the
                    // operand with `toArray(ps,1)` (`:599`) — a scalar becomes the
                    // broadcast buffer (NaN filling 0, `to_array` `:135-137`), whose
                    // derivative is a constant's: zero. Compiled C, arraySize 5, A=3:
                    // `NDERIV(A,2)` -> [3.55e-15, 1.78e-15, 0, -1.78e-15, -3.55e-15],
                    // status 0. Refusing it as a type error was the divergence.
                    //
                    // Promotion also resets numEl (`to_array` `:130`), so a promoted
                    // scalar's window is the whole buffer; an array operand keeps the
                    // window it carries, because C's `calcFirstLast` runs after
                    // `DEC(ps)`, on the array itself (`:600`). `into_cell` is that same
                    // `toArray`, so both properties come from one rule.
                    let array_size = inputs.array_size;
                    unary_op(&mut stack, |v| {
                        let mut cell = v.into_cell(array_size);
                        derivative_into_window(&mut cell, npts, &mut status);
                        ArrayStackValue::Array(cell)
                    })?
                }
                ArrayOp::Cum => {
                    // C `:787` — no `calcFirstLast`, so CUM runs over the whole buffer
                    // whatever the window says.
                    unary_op(&mut stack, |v| {
                        unary(
                            v,
                            |mut c| {
                                let buf = c.buf_mut();
                                for i in 1..buf.len() {
                                    buf[i] += buf[i - 1];
                                }
                                ArrayStackValue::Array(c)
                            },
                            PASS_THROUGH,
                        )
                    })?
                }
                ArrayOp::Cat => {
                    let array_size = inputs.array_size;
                    binary(&mut stack, |a, b| match (a, b) {
                        // C `:1383-1391` (array, double): write the scalar at
                        // `lastEl+1` and grow the window by one — but ONLY if there is
                        // room left in the `arraySize` buffer. A left operand with no
                        // window has `lastEl = arraySize-1`, so the test fails and CAT
                        // is a no-op: compiled C, `CAT(AA,9)` on a full AA is AA.
                        (ArrayStackValue::Array(mut cell), Double(d)) => {
                            let at = cell.span();
                            if at >= 0 && at < cell.buf().len() as i64 {
                                cell.buf_mut()[at as usize] = d;
                                cell.set_num_el(at + 1);
                            }
                            ArrayStackValue::Array(cell)
                        }
                        // C `:1359-1365` (array, array), reached after `toArray(ps,1)`
                        // has promoted a scalar LEFT operand: copy the right operand's
                        // WINDOW in after the left one's, stopping at the buffer end,
                        // and set numEl to how far it got. C assigns numEl
                        // unconditionally here, so even a no-copy CAT replaces the
                        // "no window" sentinel with the concrete arraySize.
                        (a, ArrayStackValue::Array(right)) => {
                            let mut cell = a.into_cell(array_size);
                            let mut i = cell.span().max(0) as usize;
                            for &v in right.window() {
                                if i >= cell.buf().len() {
                                    break;
                                }
                                cell.buf_mut()[i] = v;
                                i += 1;
                            }
                            cell.set_num_el(i as i64);
                            ArrayStackValue::Array(cell)
                        }
                        // C `:1411` — `case CAT: break;`. With neither operand an array
                        // the two-arg dispatch takes the SCALAR branch, where CAT does
                        // nothing at all. The left operand's cell IS the result cell, so
                        // the result is the LEFT scalar, still a scalar, and the right
                        // one is discarded with its cell. Compiled C, A=5, B=7:
                        // `CAT(A,B)` is 5 and `AVG(CAT(A,B))` is 5.
                        //
                        // The port built a two-element ARRAY [5,7] here. That is not a
                        // missing feature of C's — CAT concatenates into the LEFT
                        // operand's buffer, and a scalar has none — and it made the
                        // operator's result type depend on its operands in a way nothing
                        // else in the two-arg family does.
                        (Double(x), Double(_)) => Double(x),
                    })?
                }
                ArrayOp::ArrayRandom => {
                    let arr: Vec<f64> = (0..inputs.array_size).map(|_| local_random()).collect();
                    push(&mut stack, ArrayStackValue::array(arr));
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
                    let array_size = inputs.array_size;
                    let (i, j) = pop_subrange_bounds(&mut stack, n)?;
                    let in_place = matches!(aop, ArrayOp::ArraySubrange);
                    unary_op(&mut stack, |v| {
                        let mut cell = v.into_cell(array_size);
                        if in_place {
                            let src = cell.buf().to_vec();
                            let buf = cell.buf_mut();
                            let mut k = 0usize;
                            for s in i..=j {
                                // C's `j` is clamped to arraySize, not arraySize-1, so
                                // its copy loop can read one element PAST the buffer
                                // (`:1536,:1540`). Stopping is the port's deviation
                                // from that out-of-bounds read; the window below still
                                // takes C's count.
                                match (buf.get_mut(k), src.get(s as usize)) {
                                    (Some(dst), Some(&v)) => *dst = v,
                                    _ => break,
                                }
                                k += 1;
                            }
                            buf[k..].fill(0.0);
                            cell.set_num_el(1 + j - i);
                        } else {
                            // C: zero `[0,i)` and `(j,arraySize)`, keep the rest.
                            for (k, v) in cell.buf_mut().iter_mut().enumerate() {
                                let k = k as i64;
                                if k < i || k > j {
                                    *v = 0.0;
                                }
                            }
                            cell.set_num_el(j + 1);
                        }
                        ArrayStackValue::Array(cell)
                    })?
                }
                ArrayOp::ANeg => {
                    // C `:772` (array) / `:1046` (scalar) — ANEG zeroes the
                    // NEGATIVE elements and keeps the rest.
                    unary_op(&mut stack, |v| v.map(|x| if x < 0.0 { 0.0 } else { x }))?
                }
                ArrayOp::APos => {
                    // C `:773` (array) / `:1047` (scalar) — APOS zeroes the
                    // POSITIVE elements.
                    unary_op(&mut stack, |v| v.map(|x| if x > 0.0 { 0.0 } else { x }))?
                }
                ArrayOp::FetchAval => {
                    // C `FETCH_AVAL` (`:534-539`) — push `p_aresult`, the record's
                    // previous array result. The array counterpart of `VAL`.
                    push(
                        &mut stack,
                        ArrayStackValue::Array(ArrayCell::new(
                            inputs.prev_aval.clone(),
                            inputs.array_size,
                        )),
                    );
                }
                ArrayOp::DynFetch => {
                    // C `A_FETCH` (`:1461-1477`) — `@x` is the scalar argument x
                    // INDEXES (`@1` is B). C rounds the index with myNINT and
                    // answers 0 (with a console message) when it is out of range.
                    // It is the second of C's two `sourceDouble` writers (`:1472-1476`):
                    // an in-range `@x` names argument x for FITQ/FITMQ, an out-of-range
                    // one names nothing.
                    let idx = my_nint(pop_f64(&mut stack)?);
                    let found = usize::try_from(idx)
                        .ok()
                        .and_then(|i| inputs.num_arg(i).map(|v| (i, v)));
                    match found {
                        Some((i, v)) => push_src(&mut stack, Double(v), Some(i)),
                        None => push(&mut stack, Double(0.0)),
                    }
                }
                ArrayOp::DynAFetch => {
                    // C `A_AFETCH` (`:1479-1494`) — `@@x` is the ARRAY argument x
                    // indexes (`@@1` is BB). Out of range, and an argument the
                    // record never allocated, are both an all-zero array — and the
                    // result is an array either way (C `toArray(ps,0)`).
                    let idx = my_nint(pop_f64(&mut stack)?);
                    let arr = usize::try_from(idx)
                        .ok()
                        .and_then(|i| inputs.array_arg(i))
                        .cloned()
                        .unwrap_or_default();
                    push(
                        &mut stack,
                        ArrayStackValue::Array(ArrayCell::new(arr, inputs.array_size)),
                    );
                }
                ArrayOp::DynStore => {
                    // C `A_STORE` (`:494-504`) — the mirror of `A_FETCH`:
                    //
                    // ```c
                    // toDouble(ps);  ps1 = ps;  DEC(ps);            /* value */
                    // toDouble(ps);  i = myNINT(ps->d);  DEC(ps);   /* index */
                    // if (i >= num_dArgs || i < 0) printf(...); else p_dArg[i] = ps1->d;
                    // ```
                    //
                    // The index was emitted first, so it is the deeper operand. Both
                    // go; nothing is pushed. Out of range stores nothing and is not
                    // an error. No `amask` bit: this writes a SCALAR arg.
                    let value = pop_f64(&mut stack)?;
                    let idx = my_nint(pop_f64(&mut stack)?);
                    if let Some(slot) = usize::try_from(idx)
                        .ok()
                        .and_then(|i| inputs.num_arg_mut(i))
                    {
                        *slot = value;
                    }
                }
                ArrayOp::DynAStore => {
                    // C `A_ASTORE` (`:506-525`) — `@@n := v`, the ARRAY arg n names.
                    // It is `STORE_AA`'s rule with a computed index: an array value
                    // is copied element-wise, a SCALAR is BROADCAST across the whole
                    // arraySize buffer (`:519`, not `toArray`, so a NaN scalar stores
                    // NaN), and the store sets `*amask |= 1<<i` so the record posts
                    // the field it wrote. C allocates the field if the record never
                    // did; the port's arrays are always present. `i >= num_aArgs` is
                    // refused exactly as `STORE_AA` refuses it — same owner,
                    // [`ArrayInputs::store_array`].
                    let value = pop(&mut stack)?.v;
                    let idx = my_nint(pop_f64(&mut stack)?);
                    if let Ok(i) = usize::try_from(idx) {
                        let buf = match value {
                            ArrayStackValue::Array(cell) => cell.into_buf(),
                            Double(d) => vec![d; inputs.array_size],
                        };
                        inputs.store_array(i, buf);
                    }
                }
                ArrayOp::LenNoop => {
                    // C has no `case LEN` and no `default:` in aCalcPerform's
                    // switch (`aCalcPostfix.c:199`: "Array length not
                    // implemented"), so the opcode falls through and the operand
                    // stays on the stack untouched. Compiled C: `LEN(AA)` is AA.
                }
                // The FIT family (`aCalcPerform.c:1005-1036` FITPOLY/FITMPOLY,
                // :1193-1285 FITQ/FITMQ). All four fit `y = c + b*x + a*x^2` over the
                // operand's WINDOW with `x = the element index`, and all four REPLACE
                // the y operand with the FITTED CURVE — none of them returns the
                // coefficients as an array, which is what the port used to do.
                //
                //   FITPOLY(y)              1 operand
                //   FITMPOLY(y, mask)       2 — and the window comes from the MASK
                //   FITQ(y [,c][,b][,a])    vararg — the extra args NAME scalar
                //   FITMQ(y, mask [,c][,b][,a])   arguments to store the coefficients
                //                                 into (see `Cell::src`)
                ArrayOp::FitPoly => {
                    // FITPOLY and FITMPOLY are in C's UNARY switch, so a SCALAR
                    // operand takes the scalar branch — `case FITPOLY: ps->d = 0;`
                    // (`:1100`). It is not promoted, and it is not an error: compiled
                    // C, `FITPOLY(A)` with A=5 is 0 with status 0.
                    unary_op(&mut stack, |v| {
                        unary(
                            v,
                            |mut c| {
                                fit_into_window(&mut c, None, &mut status);
                                ArrayStackValue::Array(c)
                            },
                            ZERO,
                        )
                    })?
                }
                ArrayOp::FitMPoly => {
                    let array_size = inputs.array_size;
                    // C `:1013-1036`: `calcFirstLast` runs on the MASK (the top
                    // operand) before `DEC(ps)`, so the fit window is the mask's, while
                    // the result lands in the y cell — which keeps its OWN numEl.
                    //
                    // A SCALAR mask is a hard -1 in C, and by a route worth spelling
                    // out: the unary dispatch tests the TOP operand, so a scalar mask
                    // lands in the scalar branch (`case FITMPOLY: ps->d = 0;`, :1101),
                    // which never DECs the y operand below it. The leak trips the
                    // end-of-expression check `if (ps != top) return(-1)` (`:1608`).
                    // Compiled C: `FITMPOLY(AA,1)` is status -1 with no result written,
                    // where `FITMQ(AA,1,...)` — which really does `toArray` its mask —
                    // is status 0.
                    let mask = pop(&mut stack)?.v.as_cell()?.clone();
                    try_unary_op(&mut stack, |v| {
                        let mut cell = v.into_cell(array_size);
                        fit_into_window(&mut cell, Some(&mask), &mut status);
                        Ok(ArrayStackValue::Array(cell))
                    })?
                }
                ArrayOp::FitQ(nargs) => {
                    let array_size = inputs.array_size;
                    // C `:1193-1231`. Arguments past the fourth are discarded from the
                    // TOP (`while (nargs>4) {DEC(ps); nargs--;}`), then the remaining
                    // coefficient args are read off the stack top-down: the 4th names
                    // the QUADRATIC target, the 3rd the LINEAR one, the 2nd the
                    // CONSTANT one. An arg with no provenance names nothing.
                    let targets = pop_fit_targets(&mut stack, *nargs as usize, 4)?;
                    let mut cell = pop(&mut stack)?.v.into_cell(array_size);
                    let coeffs = fit_into_window(&mut cell, None, &mut status);
                    store_fit_coefficients(inputs, targets, coeffs);
                    push(&mut stack, ArrayStackValue::Array(cell));
                }
                ArrayOp::FitMQ(nargs) => {
                    let array_size = inputs.array_size;
                    // C `:1234-1285`, the same with a mask — and with a hard floor:
                    // fewer than two arguments is an immediate `return(-1)` (`:1258`),
                    // not a deferred status.
                    if *nargs < 2 {
                        return Err(CalcError::Underflow);
                    }
                    let targets = pop_fit_targets(&mut stack, *nargs as usize, 5)?;
                    let mask = pop(&mut stack)?.v.into_cell(array_size);
                    let mut cell = pop(&mut stack)?.v.into_cell(array_size);
                    let coeffs = fit_into_window(&mut cell, Some(&mask), &mut status);
                    store_fit_coefficients(inputs, targets, coeffs);
                    push(&mut stack, ArrayStackValue::Array(cell));
                }
            },

            // aCalc has its own copy of sCalc's UNTIL machinery — the pre-scan at
            // `aCalcPerform.c:349-390` and these two cases at `:1551-1590` — and it
            // is the same code with `ps` walking cells instead of doubles. The
            // element table compiles `UNTIL` (`aCalcPostfix.c:200`), so an aCalc
            // program can reach these opcodes; the port had no `Opcode::Control`
            // arm at all and fell into the `_` catch-all, failing every aCalc
            // expression that contained an UNTIL loop.
            Opcode::Control(ctrl) => match ctrl {
                // C `:1551-1567` — UNTIL only records where the stack was, so that
                // UNTIL_END can wind it back before re-running the body. `until_loc`
                // is the key C matches on, and `pc - 1` is that key here.
                ControlOp::Until(_end_pc) => {
                    let until_pc = pc - 1;
                    match until_marks.iter_mut().find(|(k, _)| *k == until_pc) {
                        Some((_, depth)) => *depth = stack.len(),
                        // No ceiling to enforce here — `check_until_ceiling` refused
                        // a program with more than nine UNTILs before the first
                        // opcode ran. This table is a location map, nothing more.
                        None => until_marks.push((until_pc, stack.len())),
                    }
                }
                // C `:1569-1590`, in this order:
                //
                // ```c
                // if (++loopsDone > aCalcLoopMax) break;   /* give up, no error */
                // if (ps->d == 0) { ...wind back and re-run the body... }
                // ```
                //
                // `loopsDone` counts every arrival at an UNTIL_END and is shared by
                // all the UNTILs in one program; when it runs out C simply STOPS
                // LOOPING — the perform continues and returns 0. There is no
                // loop-limit error and the record does not alarm.
                ControlOp::UntilEnd(start_pc) => {
                    loops_done += 1;
                    if loops_done > acalc_loop_max() {
                        continue;
                    }
                    // C PEEKS the condition (`:1573`: `if (ps->d == 0)`) — it pops
                    // nothing, which is why UNTIL_END has a runtime effect of 0.
                    //
                    // Deliberate deviation: C reads the cell's `d` field RAW here,
                    // with no `toDouble`, so an ARRAY-valued condition tests
                    // whatever `d` happened to hold — for a cell that has only ever
                    // been an array, uninitialised memory. The port reads element 0,
                    // C's own `to_double` (`:121`). Every condition an UNTIL is
                    // written to carry (a comparison) is a scalar, where the two
                    // agree.
                    let cond = match stack.last() {
                        Some(c) => c.v.as_f64()?,
                        None => return Err(CalcError::Underflow),
                    };
                    if cond == 0.0 {
                        // Wind the stack back to where the paired UNTIL saw it — C
                        // restores the saved `ps` wholesale, so everything the body
                        // pushed (the condition included) is discarded.
                        let Some((_, depth)) = until_marks.iter().find(|(k, _)| k == start_pc)
                        else {
                            // C's `printf("aCalcPerform: UNTIL not found"); return(-1)`.
                            return Err(CalcError::Internal);
                        };
                        stack.truncate(*depth);
                        pc = *start_pc + 1;
                    }
                    // Condition true: fall out with the condition value still on the
                    // stack. It is the value of the `UNTIL(...)`.
                }
            },

            #[allow(unreachable_patterns)]
            _ => return Err(CalcError::Internal),
        }
    }

    // C `:1602-1605`: the deferred failure is consumed HERE, after the whole
    // expression has run (and after its stores have landed), and it suppresses the
    // result write entirely.
    status.into_result()?;

    // C `:1607-1618`, and it runs AFTER the deferred status above, in this order:
    //
    // ```c
    // if (status) { freeStack(flp, stack); return(status); }
    // /* if everything is peachy, the stack should end at its first position */
    // if (ps != top) { freeStack(flp, stack); return(-1); }
    // ```
    //
    // `ps` starts one BELOW `top` (`:418-419` — `top = ps = &stack[1]; ps--;`, with
    // the comment "Expression handler assumes ps is pointing to a filled element"),
    // and every push is an `INC` first. So `ps == top` means the stack holds
    // EXACTLY ONE value, and C's check is a hard depth-1 assertion: an expression
    // that leaked an operand AND one that consumed everything are both -1, with
    // neither `p_dresult` nor `p_aresult` written.
    //
    // This is an INVARIANT GUARD, not a fix for a live bug. `aCalcPostfix`'s
    // compile-time `runtime_depth` ledger (`aCalcPostfix.c:420-585`) rejects any
    // program that would not end at depth 1, so a program it accepts always
    // balances; no expression has been found where C returns -1 here while the port
    // returns a value. (C's check DOES fire — compiled C, `FITMPOLY(AA,1)` is status
    // -1 with AVAL never written, because a scalar mask takes the unary dispatch's
    // scalar branch and never DECs the y operand below it — but the port already
    // refuses that one earlier, in FITMPOLY's own arm.)
    //
    // It is here so that a future opcode which forgets to pop cannot silently
    // publish the wrong stack cell as VAL/AVAL: `stack.last()` would have handed
    // back the leaked operand, and an empty stack would have invented a 0 that C
    // never produces.
    match <[Cell; 1]>::try_from(stack) {
        Ok([result]) => Ok(result.v),
        Err(_) => Err(CalcError::StackLeak),
    }
}

/// C's single `status` cell (`aCalcPerform.c:422`), read exactly ONCE, at the end of
/// the expression (`:1602-1605`) — a deferred failure flag, not an early return.
///
/// The deferral is observable: aCalc's store opcodes write straight into the record's
/// A..P / AA..LL fields, so a store sequenced AFTER a failing operator still lands.
/// Returning early from the operator's arm would silently skip it.
///
/// Every write to the cell is an ASSIGNMENT — `status = deriv(...)` (`:613`, `:985`),
/// `status = fitpoly(...)` (`:1008`, `:1029`, `:1221`, `:1270`), and `status = 0` at
/// the head of each array SQRT/LOG guard (`:776`, `:792`, `:804`) — so the LAST
/// fallible operator in the expression decides, and a CLEAN one clears an earlier
/// failure. Compiled C, arraySize 4, CC all zero: `SQRT(CC-1)+SQRT(BB)` is status 0
/// (with the failed left operand's zeros still in the sum), while the reverse order
/// `SQRT(BB)+SQRT(CC-1)` is status -1.
///
/// That is why the only mutator is [`Status::set`], which takes the whole outcome:
/// an operator cannot raise the flag without also being able to lower it, which is
/// exactly what made the port's `Option` sticky.
#[derive(Default)]
struct Status(Option<CalcError>);

impl Status {
    /// C's `status = <fallible operator>`. `None` is C's 0.
    fn set(&mut self, outcome: Option<CalcError>) {
        self.0 = outcome;
    }

    fn into_result(self) -> Result<(), CalcError> {
        self.0.map_or(Ok(()), Err)
    }
}

/// C's `fitpoly` call shared by all four FIT operators (`:1020`, `:1027`, `:1221`,
/// `:1271`): fit `y = c + b*x + a*x*x` over the WINDOW with `x[i] = i` (C fills a
/// scratch array with `ps2->a[i] = i`), write the FITTED CURVE back into the
/// window, and zero everything outside it.
///
/// The optional mask is C's FITMPOLY/FITMQ mask: a point takes part in the fit only
/// where `mask[i] > SMALL` (`calcUtil.c:280`, SMALL = 1e-8) — a THRESHOLD, not
/// `!= 0`, so a mask element of 1e-9 masks the point out.
///
/// A failed fit — fewer than 3 points in the window, or a singular normal matrix
/// (`calcUtil.c:271`, `:297`) — is C's `fitpoly` returning -1, and every one of the
/// four call sites ASSIGNS that to `status` (`:1008`, `:1029`, `:1221`, `:1270`). The
/// assignment lives HERE, in the one helper they all go through, so a FIT operator
/// cannot be written that forgets it. Compiled C, arraySize 6:
/// `FITPOLY(BB[0,1])` (a two-point window) is status -1.
///
/// The curve then comes out all-zero. C's is not: `d`/`e`/`f` are aCalcPerform's
/// general-purpose scratch doubles, so a failed fit leaves whatever the previous
/// operator put there. It is unobservable either way — a non-zero status suppresses
/// the result write entirely (`:1602-1605`) — and zeros are the deterministic choice.
fn fit_into_window(
    cell: &mut ArrayCell,
    mask: Option<&ArrayCell>,
    status: &mut Status,
) -> (f64, f64, f64) {
    let window = mask.map_or_else(|| cell.window().len(), |m| m.window().len());
    let window = window.min(cell.buf().len());
    let x: Vec<f64> = (0..window).map(|i| i as f64).collect();
    let y = cell.buf()[..window].to_vec();
    let fit = fitting::fitpoly(&x, &y, mask.map(|m| &m.buf()[..window]));
    status.set(fit.is_none().then_some(CalcError::FitFailed));
    let coeffs = fit.unwrap_or((0.0, 0.0, 0.0));
    let (c, b, a) = coeffs;
    for (i, xi) in x.iter().enumerate() {
        cell.buf_mut()[i] = c + b * xi + a * xi * xi;
    }
    cell.buf_mut()[window..].fill(0.0);
    coeffs
}

/// C's DERIV (`:976-989`) and NDERIV (`:594-617`) are one kernel: `deriv()` is
/// literally `nderiv(x, y, n, d, 2, work)` (`calcUtil.c:71-75`). Both take the
/// derivative over the WINDOW, write it back into the window, zero everything outside
/// it (`:985-987`, `:614-616`), and ASSIGN the return value to `status` — so they
/// share this helper, and the status write is not something a caller can omit.
///
/// A failed fit leaves C copying from a scratch cell it never wrote (a recycled
/// freelist buffer); the port writes a zeroed window instead. Unobservable: the
/// non-zero status suppresses the result write.
fn derivative_into_window(cell: &mut ArrayCell, npts: i64, status: &mut Status) {
    let d = derivative::nderiv(cell.window(), npts);
    status.set(d.is_none().then_some(CalcError::FitFailed));
    let d = d.unwrap_or_else(|| vec![0.0; cell.window().len()]);
    cell.window_mut().copy_from_slice(&d);
    cell.clear_outside_window();
}

/// Pop a FITQ/FITMQ argument list down to its non-coefficient arguments, answering
/// the scalar-argument INDEX each coefficient argument named (C's `sourceDouble`).
///
/// C reads them off the top of the stack (`:1199-1211`, `:1243-1255`): with `max`
/// slots, the topmost argument names the QUADRATIC coefficient's target, the next
/// the LINEAR one's, the next the CONSTANT one's. Anything above `max` is discarded
/// first (`while (nargs>max) {DEC(ps); nargs--;}`), so a caller who passes too many
/// loses the LAST ones, not the first.
///
/// Returns `(constant_target, linear_target, quadratic_target)`.
type FitTargets = (Option<usize>, Option<usize>, Option<usize>);

fn pop_fit_targets(
    stack: &mut Vec<Cell>,
    nargs: usize,
    max: usize,
) -> Result<FitTargets, CalcError> {
    let mut nargs = nargs;
    while nargs > max {
        pop(stack)?;
        nargs -= 1;
    }
    // `max` is 4 for FITQ (y + 3 coefficients) and 5 for FITMQ (y + mask + 3), so
    // the number of coefficient args present is nargs - (max - 3).
    let fixed = max - 3;
    let coeff_args = nargs.saturating_sub(fixed);
    let mut t = [None, None, None]; // constant, linear, quadratic
    for slot in (0..coeff_args.min(3)).rev() {
        t[slot] = pop(stack)?.src;
    }
    Ok((t[0], t[1], t[2]))
}

/// C `:1226-1229` / `:1280-1283`: `p_dArg[arga] = d; p_dArg[argb] = e;
/// p_dArg[argc] = f;` — the CONSTANT term goes to the argument the caller named
/// first, the linear term to the second, the quadratic term to the third. Each
/// write is `if ((arga != -1) && (arga < num_dArgs))`: the caller's count bounds
/// the coefficient write-back too.
fn store_fit_coefficients(inputs: &mut ArrayInputs, targets: FitTargets, coeffs: (f64, f64, f64)) {
    let (tc, tb, ta) = targets;
    let (c, b, a) = coeffs;
    for (target, value) in [(tc, c), (tb, b), (ta, a)] {
        if let Some(i) = target
            && let Some(slot) = inputs.num_arg_mut(i)
        {
            *slot = value;
        }
    }
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
fn domain_guarded(v: ArrayStackValue, f: fn(f64) -> f64, status: &mut Status) -> ArrayStackValue {
    match v {
        // The scalar branch does not touch `status` at all — not even to clear it.
        Double(d) => Double(if d < 0.0 { 0.0 } else { f(d) }),
        ArrayStackValue::Array(mut cell) => {
            // Element-wise, so the whole buffer — C's loop is `for (i=0;
            // i<arraySize; i++)` (`:775-812`), with no `calcFirstLast`.
            //
            // `outcome` starts clean because C's array branch OPENS with `status = 0`
            // (`:776`, `:792`, `:804`): a clean SQRT after a failed one clears the
            // failure. See [`Status`].
            let mut outcome = None;
            for x in cell.buf_mut() {
                if *x < 0.0 {
                    outcome = Some(CalcError::DomainError);
                    *x = 0.0;
                } else {
                    *x = f(*x);
                }
            }
            status.set(outcome);
            ArrayStackValue::Array(cell)
        }
    }
}

/// C's `MAX_VAL` — the `>?` operator. ONE formula, used verbatim in all THREE
/// operand shapes of C's two-arg dispatch: array/array (`:1351`), array/scalar
/// (`:1376`) and scalar/scalar (`:1403`) are each `if (x < y) x = y`.
///
/// It is deliberately not `f64::max`. The comparison is the bare C one, so a NaN
/// is not "the missing value" it is for IEEE maxNum — it simply loses every
/// comparison, and which operand holds it decides the answer. Compiled C:
/// `5 >? NaN` is 5 (the `<` is false, so the left operand stands), while
/// `NaN >? 5` is NaN (likewise false, and the left operand is the NaN).
fn max_val(x: f64, y: f64) -> f64 {
    if x < y { y } else { x }
}

/// C's `MIN_VAL` — `<?`, the mirror image (`:1352`, `:1377`, `:1404`).
fn min_val(x: f64, y: f64) -> f64 {
    if x > y { y } else { x }
}

/// Which of the two extremum operators is running. The point of the type is that
/// C's `case MAX:` and `case MIN:` are ONE arm distinguished by a single `op ==
/// MAX` test (`:1155-1191`), so the two must not drift apart into copied loops.
#[derive(Clone, Copy)]
enum Extremum {
    Max,
    Min,
}

impl Extremum {
    /// C's ARRAY-branch element rule (`:1166-1175`): `if (other > acc) acc = other`
    /// for MAX. That is exactly the `>?` formula, which is why this delegates to it
    /// rather than restating the comparison — one rule, two operators.
    fn fold_element(self, acc: f64, other: f64) -> f64 {
        match self {
            Extremum::Max => max_val(acc, other),
            Extremum::Min => min_val(acc, other),
        }
    }

    /// C's SCALAR-branch keep test (`:1185`, `:1187`) — `ps->d < d` for MAX. Note
    /// this is NOT the array rule: the scalar branch alone carries an `isnan` test,
    /// which is why it cannot reuse [`Self::fold_element`]. See [`vararg_extremum`].
    fn keeps(self, arg: f64, acc: f64) -> bool {
        match self {
            Extremum::Max => arg < acc,
            Extremum::Min => arg > acc,
        }
    }
}

/// C's vararg `MAX()`/`MIN()` (`aCalcPerform.c:1155-1191`) — one opcode with two
/// branches, chosen by whether ANY argument is an array (`:1159`, `j |=
/// isArray(ps-i)`). The port answered a scalar built from each argument's `a[0]` in
/// both, so a record's AVAL got a broadcast scalar instead of the element-wise
/// extremum.
///
/// **Array branch** (`:1161-1178`). C coerces the BOTTOMMOST argument with
/// `toArray(ps1,1)` and folds every other argument into it, so:
///
/// * the result cell is the FIRST argument's — it keeps that argument's window,
///   and a scalar first argument brings `toArray`'s cleared window (and its NaN->0
///   fill) with it. Compiled C, arraySize 6, `AA=[1,5,2,8,3,9]`:
///   `AVG(MAX(AA[1,3],0))` is 5 (the 3-element window survives) while
///   `AVG(MAX(0,AA[1,3]))` is 2.5 (the promoted scalar's window is the whole
///   buffer, so the fold's zero tail counts).
/// * the other arguments are read over the WHOLE `arraySize` buffer, not their
///   windows (`for (i=0; i<arraySize; i++)`).
/// * the fold is a bare comparison with no NaN test, so a NaN argument never wins.
///   Compiled C: `MAX(AA,NaN)` is AA unchanged.
///
/// **Scalar branch** (`:1180-1189`). C starts the accumulator at the TOPMOST (last)
/// argument and folds DOWN through the earlier ones, and this branch — unlike the
/// array one — carries `|| isnan(d)`:
///
/// ```c
/// while (--nargs) { d = ps->d; DEC(ps); if (ps->d < d || isnan(d)) ps->d = d; }
/// ```
///
/// The running value therefore WINS whenever it is NaN, and a NaN argument that
/// arrives later replaces a healthy running value (it is not `<` it). Either way a
/// NaN anywhere in the list reaches the end: compiled C, `MAX(5,NaN)` and
/// `MAX(NaN,5)` are both NaN (status -1). The port's test was inverted — it
/// DISCARDED a NaN accumulator — so both answered 5.
fn vararg_extremum(
    stack: &mut Vec<Cell>,
    nargs: usize,
    array_size: usize,
    op: Extremum,
) -> Result<(), CalcError> {
    let args = popn(stack, nargs)?;
    if args.is_empty() {
        return Err(CalcError::Underflow);
    }

    let result = if args.iter().any(ArrayStackValue::is_array) {
        let mut it = args.into_iter();
        let mut acc = it.next().ok_or(CalcError::Underflow)?.into_cell(array_size);
        for arg in it {
            match arg {
                ArrayStackValue::Array(other) => {
                    for (a, &o) in acc.buf_mut().iter_mut().zip(other.buf()) {
                        *a = op.fold_element(*a, o);
                    }
                }
                Double(d) => {
                    for a in acc.buf_mut() {
                        *a = op.fold_element(*a, d);
                    }
                }
            }
        }
        ArrayStackValue::Array(acc)
    } else {
        let vals: Vec<f64> = args
            .iter()
            .map(ArrayStackValue::as_f64)
            .collect::<Result<_, _>>()?;
        let mut acc = *vals.last().ok_or(CalcError::Underflow)?;
        for &arg in vals.iter().rev().skip(1) {
            if !op.keeps(arg, acc) && !acc.is_nan() {
                acc = arg;
            }
        }
        Double(acc)
    };

    push(stack, result);
    Ok(())
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
/// with an `as_cell()?` that turns a legal expression into CALC_ALARM.
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
        Double(d) => Double(on_scalar(d)),
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
/// keeps `AVG(AA[2,1])` a NaN (`a[0]`/0) instead of a 0/0-free 0.
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
/// element — and narrows each into an `int` with a truncating `(int)`, not
/// `myNINT`.
///
/// That `(int)` is [`c_int`], the engine's cast owner, and this site used to
/// open-code `as i64` instead. An out-of-range narrowing is C UB and the two
/// compiled targets EPICS ships on disagree (x86-64 `cvttsd2si` gives
/// `INT32_MIN`; aarch64 `fcvtzs` saturates), so there is no C answer to be
/// faithful to — [`crate::types::c_cast`] owns the one value the port picks,
/// and every narrowing of a stack double must ask it rather than decide for
/// itself. Under the owner's saturating rule the bound clamps to `arraySize`
/// exactly as the open-coded `as i64` did, so this is a routing change with no
/// observable effect: `AA[2,3e9]` selects the tail, before and after.
///
/// The rest of the rule is [`super::subrange_bounds`], shared with sCalc.
fn pop_subrange_bounds(stack: &mut Vec<Cell>, array_size: i64) -> Result<(i64, i64), CalcError> {
    let j = i64::from(c_int(pop_f64(stack)?));
    let i = i64::from(c_int(pop_f64(stack)?));
    Ok(super::subrange_bounds(i, j, array_size))
}

/// C's `INC(ps)` (`:88`): a fresh cell, with neither a window nor a provenance.
fn push(stack: &mut Vec<Cell>, v: ArrayStackValue) {
    stack.push(Cell { v, src: None });
}

/// C's two `sourceDouble` writers — `FETCH_A..P` (`:434`) and `A_FETCH` (`:1475`).
fn push_src(stack: &mut Vec<Cell>, v: ArrayStackValue, src: Option<usize>) {
    stack.push(Cell { v, src });
}

fn pop(stack: &mut Vec<Cell>) -> Result<Cell, CalcError> {
    stack.pop().ok_or(CalcError::Underflow)
}

/// C's in-place unary: `ps` is both the operand and the result (`toDouble(ps);
/// ps->d = ...`, `for (i...) ps->a[i] = ...`), so the cell's provenance survives.
/// The slice (not `Vec`) is the point — an in-place operator cannot change the
/// stack DEPTH, which is what makes the provenance survive at all.
fn unary_op(
    stack: &mut [Cell],
    f: impl FnOnce(ArrayStackValue) -> ArrayStackValue,
) -> Result<(), CalcError> {
    let cell = stack.last_mut().ok_or(CalcError::Underflow)?;
    let v = std::mem::replace(&mut cell.v, Double(0.0));
    cell.v = f(v);
    Ok(())
}

fn try_unary_op(
    stack: &mut [Cell],
    f: impl FnOnce(ArrayStackValue) -> Result<ArrayStackValue, CalcError>,
) -> Result<(), CalcError> {
    let cell = stack.last_mut().ok_or(CalcError::Underflow)?;
    let v = std::mem::replace(&mut cell.v, Double(0.0));
    cell.v = f(v)?;
    Ok(())
}

/// C's in-place binary: `ps1 = ps; DEC(ps);` then the operator writes into `ps` —
/// the LEFT operand's cell IS the result cell, so the LEFT operand's provenance
/// survives and the right operand's is discarded with its cell.
fn binary(
    stack: &mut Vec<Cell>,
    f: impl FnOnce(ArrayStackValue, ArrayStackValue) -> ArrayStackValue,
) -> Result<(), CalcError> {
    let b = pop(stack)?;
    unary_op(stack, |a| f(a, b.v))
}

/// Pop the `n` arguments of a VARARG operator, keeping their shapes intact.
fn popn(stack: &mut Vec<Cell>, n: usize) -> Result<Vec<ArrayStackValue>, CalcError> {
    if stack.len() < n {
        return Err(CalcError::Underflow);
    }
    Ok(stack
        .split_off(stack.len() - n)
        .into_iter()
        .map(|c| c.v)
        .collect())
}

fn pop_f64(stack: &mut Vec<Cell>) -> Result<f64, CalcError> {
    pop(stack)?.v.as_f64()
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

/// The end-of-expression depth invariant — C's `if (ps != top) return(-1)`
/// (`aCalcPerform.c:1607-1618`).
///
/// These programs are hand-built because `aCalcPostfix` CANNOT emit them: its
/// compile-time `runtime_depth` ledger (`aCalcPostfix.c:420-585`) rejects anything
/// that would not end at depth 1, which is why C's runtime check has no reachable
/// divergence today. Compiled C agrees, and so does the port's compiler — both
/// answer "Incomplete expression, operand missing" for `A:=1`, `AA:=1`, `BB:=AA`
/// and `AA:=AA+1`, and both compile and run `A:=1;2` to 2.
///
/// So the guard is tested where it actually applies: at the engine's own boundary,
/// against a program that violates the invariant.
#[cfg(test)]
mod stack_depth_invariant {
    use super::*;
    use crate::calc::engine::ExprKind;

    fn run(code: Vec<Opcode>) -> Result<ArrayStackValue, CalcError> {
        let expr = CompiledExpr {
            code,
            ..CompiledExpr::empty(ExprKind::Array)
        };
        eval(&expr, &mut ArrayInputs::new(4))
    }

    #[test]
    fn a_leaked_operand_is_an_error_not_the_top_of_stack() {
        // Two pushes, no operator to consume them: C ends with ps one ABOVE top and
        // returns -1, writing neither p_dresult nor p_aresult. The port used to
        // return `stack.last()` — the 2.0 — and publish it as VAL/AVAL.
        let leaked = vec![
            Opcode::Core(CoreOp::PushConst(1.0)),
            Opcode::Core(CoreOp::PushConst(2.0)),
            Opcode::Core(CoreOp::End),
        ];
        assert_eq!(run(leaked), Err(CalcError::StackLeak));
    }

    #[test]
    fn an_empty_stack_is_an_error_not_a_zero() {
        // A store consumes the only value, so the program ends at depth 0: C's `ps`
        // is left one BELOW `top` and the check fails just as hard. The port used to
        // invent a 0.0 that C never produces.
        let consumed = vec![
            Opcode::Core(CoreOp::PushConst(1.0)),
            Opcode::Core(CoreOp::StoreVar(0)),
            Opcode::Core(CoreOp::End),
        ];
        assert_eq!(run(consumed), Err(CalcError::StackLeak));
    }

    #[test]
    fn exactly_one_value_is_the_result() {
        let balanced = vec![
            Opcode::Core(CoreOp::PushConst(1.0)),
            Opcode::Core(CoreOp::PushConst(2.0)),
            Opcode::Core(CoreOp::Add),
            Opcode::Core(CoreOp::End),
        ];
        assert_eq!(run(balanced), Ok(Double(3.0)));
    }
}
