use super::bytecode::{self, BASE, base::*};
use super::cast::{d2i, d2ui, imod, nint};
use super::error::CalcError;
use super::random::calc_random;
use super::{NumericInputs, NumericVars};

pub fn eval(code: &[u8], inputs: &mut NumericInputs) -> Result<f64, CalcError> {
    eval_vars(code, inputs.view())
}

/// `eval` on a borrowed arg block — the record's own `A..U`, as C's
/// `calcPerform(&prec->a, ...)` is handed a pointer into the record.
pub(crate) fn eval_vars(code: &[u8], mut inputs: NumericVars<'_>) -> Result<f64, CalcError> {
    // C `calcPerform` runs the empty program's loop zero times and leaves the
    // stack empty, so its closing `if (ptop != stack + 1) return -1`
    // (`calcPerform.c:419-420`) fails it — and `*presult` is never written, so
    // the record keeps the previous VAL and goes to CALC_ALARM/INVALID. A
    // failed compile leaves exactly this program behind, which is how C makes a
    // broken CALC alarm on EVERY process rather than once at compile time.
    if bytecode::is_empty(code) {
        return Err(CalcError::EmptyProgram);
    }

    let mut stack = Stack::new();
    let mut pc = 0;

    // C's `switch (*post++)` (`calcPerform.c:58`): one dispatch on the byte.
    // Running off the end of the slice reads `END_EXPRESSION`.
    while let Some(&b) = code.get(pc) {
        pc += 1;

        match b {
            END => break,

            // Push operations
            LITERAL_DOUBLE => {
                let p = code.get(pc..pc + 8).ok_or(CalcError::Internal)?;
                stack.push(f64::from_ne_bytes(p.try_into().unwrap()));
                pc += 8;
            }
            LITERAL_INT => {
                let p = code.get(pc..pc + 4).ok_or(CalcError::Internal)?;
                stack.push(f64::from(i32::from_ne_bytes(p.try_into().unwrap())));
                pc += 4;
            }
            // An arg the caller did not supply fetches 0 — the same rule
            // `sCalcPerform.c:421-427` and `aCalcPerform.c:432` state for
            // theirs. Base's C omits the bound and walks off the end of a
            // short caller's field block (CBUG-G3); see `NumericInputs::num_args`.
            FETCH_A..=FETCH_U => {
                stack.push(inputs.num_arg(usize::from(b - FETCH_A)).unwrap_or(0.0));
            }

            // Constants
            CONST_PI => stack.push(std::f64::consts::PI),
            CONST_D2R => stack.push(std::f64::consts::PI / 180.0),
            CONST_R2D => stack.push(180.0 / std::f64::consts::PI),

            // Random. BASE's generator, not synApps' — `calcRandom`
            // (`calcPerform.c:518-523`) maps the shared Knuth LCG with
            // `seed / 65535.0`, where sCalc/aCalc use `(seed+1) / 65536.0`.
            // Every draw differs, from the first one on.
            RANDOM => {
                stack.push(calc_random());
            }
            FETCH_VAL => {
                // C calcPerform.c:74-76 — FETCH_VAL pushes *presult, the
                // record's previous calculation result (the VAL field).
                stack.push(inputs.prev_val);
            }
            // Arithmetic
            ADD => {
                let (a, b) = pop2(&mut stack)?;
                stack.push(a + b);
            }
            SUB => {
                let (a, b) = pop2(&mut stack)?;
                stack.push(a - b);
            }
            MULT => {
                let (a, b) = pop2(&mut stack)?;
                stack.push(a * b);
            }
            DIV => {
                let (a, b) = pop2(&mut stack)?;
                // C uses IEEE 754: 1.0/0.0 = Inf, 0.0/0.0 = NaN
                stack.push(a / b);
            }
            MODULO => {
                let (a, b) = pop2(&mut stack)?;
                // C `calcPerform.c:176-190` (unmerged PR #925):
                //   itop = d2i(top);
                //   if (itop == 0)       *ptop = epicsNAN;
                //   else if (itop == -1) *ptop = 0;
                //   else                 *ptop = d2i(*ptop) % itop;
                // base's dialect narrowing is `d2i`; `cast::imod` owns the
                // -1 guard; base's zero-divisor rule is NaN. See CBUG-A2.
                stack.push(imod(a, b, |x| i64::from(d2i(x))).unwrap_or(f64::NAN));
            }
            UNARY_NEG => {
                let a = pop1(&mut stack)?;
                stack.push(-a);
            }
            POWER => {
                let (a, b) = pop2(&mut stack)?;
                stack.push(a.powf(b));
            }

            // Comparison - exact comparison like C (no epsilon)
            EQUAL => {
                let (a, b) = pop2(&mut stack)?;
                stack.push(if a == b { 1.0 } else { 0.0 });
            }
            NOT_EQ => {
                let (a, b) = pop2(&mut stack)?;
                stack.push(if a != b { 1.0 } else { 0.0 });
            }
            LESS_THAN => {
                let (a, b) = pop2(&mut stack)?;
                stack.push(if a < b { 1.0 } else { 0.0 });
            }
            LESS_OR_EQ => {
                let (a, b) = pop2(&mut stack)?;
                stack.push(if a <= b { 1.0 } else { 0.0 });
            }
            GR_THAN => {
                let (a, b) = pop2(&mut stack)?;
                stack.push(if a > b { 1.0 } else { 0.0 });
            }
            GR_OR_EQ => {
                let (a, b) = pop2(&mut stack)?;
                stack.push(if a >= b { 1.0 } else { 0.0 });
            }

            // Logical
            REL_AND => {
                let (a, b) = pop2(&mut stack)?;
                stack.push(if a != 0.0 && b != 0.0 { 1.0 } else { 0.0 });
            }
            REL_OR => {
                let (a, b) = pop2(&mut stack)?;
                stack.push(if a != 0.0 || b != 0.0 { 1.0 } else { 0.0 });
            }
            REL_NOT => {
                let a = pop1(&mut stack)?;
                stack.push(if a == 0.0 { 1.0 } else { 0.0 });
            }

            // Bitwise - use C's d2i/d2ui conversion (wrap-on-overflow 32-bit)
            BIT_AND => {
                let (a, b) = pop2(&mut stack)?;
                stack.push((d2i(a) & d2i(b)) as f64);
            }
            BIT_OR => {
                let (a, b) = pop2(&mut stack)?;
                stack.push((d2i(a) | d2i(b)) as f64);
            }
            BIT_EXCL_OR => {
                let (a, b) = pop2(&mut stack)?;
                stack.push((d2i(a) ^ d2i(b)) as f64);
            }
            BIT_NOT => {
                let a = pop1(&mut stack)?;
                stack.push(!d2i(a) as f64);
            }
            LEFT_SHIFT_ARITH => {
                let (a, b) = pop2(&mut stack)?;
                // C: d2i(*ptop) << (d2i(top) & 31)
                stack.push((d2i(a) << (d2i(b) & 31)) as f64);
            }
            RIGHT_SHIFT_ARITH => {
                let (a, b) = pop2(&mut stack)?;
                stack.push((d2i(a) >> (d2i(b) & 31)) as f64);
            }
            RIGHT_SHIFT_LOGIC => {
                let (a, b) = pop2(&mut stack)?;
                stack.push((d2ui(a) >> (d2ui(b) & 31)) as f64);
            }

            // Conditional
            COND_IF => {
                let cond = pop1(&mut stack)?;
                if cond == 0.0 {
                    pc = bytecode::cond_search(&BASE, code, pc, true)?;
                }
            }
            COND_ELSE => {
                pc = bytecode::cond_search(&BASE, code, pc, false)?;
            }
            COND_END => {
                // No-op, just a marker
            }

            // Math functions (1 arg)
            //
            // BASE's ABS is `fabs` (`calcPerform.c:174-176`) and stays that
            // way: `ABS(-0.0)` is `+0.0` here. The synApps engines use a
            // conditional negate and answer `-0.0` — a real dialect
            // difference, not an oversight to unify (see [`super::abs_val`],
            // which they, and only they, call).
            ABS_VAL => {
                let a = pop1(&mut stack)?;
                stack.push(a.abs());
            }
            SQU_RT => {
                let a = pop1(&mut stack)?;
                stack.push(a.sqrt());
            }
            EXP => {
                let a = pop1(&mut stack)?;
                stack.push(a.exp());
            }
            LOG_10 => {
                let a = pop1(&mut stack)?;
                stack.push(a.log10());
            }
            LOG_E => {
                let a = pop1(&mut stack)?;
                stack.push(a.ln());
            }
            SIN => {
                let a = pop1(&mut stack)?;
                stack.push(a.sin());
            }
            COS => {
                let a = pop1(&mut stack)?;
                stack.push(a.cos());
            }
            TAN => {
                let a = pop1(&mut stack)?;
                stack.push(a.tan());
            }
            ASIN => {
                let a = pop1(&mut stack)?;
                stack.push(a.asin());
            }
            ACOS => {
                let a = pop1(&mut stack)?;
                stack.push(a.acos());
            }
            ATAN => {
                let a = pop1(&mut stack)?;
                stack.push(a.atan());
            }
            SINH => {
                let a = pop1(&mut stack)?;
                stack.push(a.sinh());
            }
            COSH => {
                let a = pop1(&mut stack)?;
                stack.push(a.cosh());
            }
            TANH => {
                let a = pop1(&mut stack)?;
                stack.push(a.tanh());
            }
            CEIL => {
                let a = pop1(&mut stack)?;
                stack.push(a.ceil());
            }
            FLOOR => {
                let a = pop1(&mut stack)?;
                stack.push(a.floor());
            }
            NINT => {
                let a = pop1(&mut stack)?;
                // C `calcPerform.c:313-317` (unmerged PR #925):
                //   top = top >= 0 ? top+0.5 : top-0.5;
                //   *ptop = d2i(top);
                // base's dialect narrowing is `d2i`. See CBUG-A2.
                stack.push(nint(a, |x| i64::from(d2i(x))));
            }

            // Test functions
            ISNAN => {
                let n = usize::from(*code.get(pc).ok_or(CalcError::Internal)?);
                pc += 1;
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
            ISINF => {
                let a = pop1(&mut stack)?;
                stack.push(super::isinf(a));
            }
            FINITE => {
                let n = usize::from(*code.get(pc).ok_or(CalcError::Internal)?);
                pc += 1;
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
            ATAN2 => {
                let (a, b) = pop2(&mut stack)?;
                stack.push(b.atan2(a));
            }
            FMOD => {
                let (a, b) = pop2(&mut stack)?;
                stack.push(a % b);
            }

            // Vararg min/max
            MAX => {
                let n = usize::from(*code.get(pc).ok_or(CalcError::Internal)?);
                pc += 1;
                if stack.len() < n {
                    return Err(CalcError::Underflow);
                }
                // C: top = *ptop--; if (*ptop < top || isnan(top)) *ptop = top;
                // The running value (`acc`) flows from the shallowest operand
                // toward the deepest; at each step `acc` is C's `top` and the
                // next-deeper operand is C's accumulator `*ptop`.
                let base = stack.len() - n;
                let mut acc = stack[stack.len() - 1];
                for i in (base..stack.len() - 1).rev() {
                    let deeper = stack[i];
                    // Keep `acc` when `deeper < acc` or `acc` is NaN, else `deeper`.
                    acc = if deeper < acc || acc.is_nan() {
                        acc
                    } else {
                        deeper
                    };
                }
                stack.truncate(base);
                stack.push(acc);
            }
            MIN => {
                let n = usize::from(*code.get(pc).ok_or(CalcError::Internal)?);
                pc += 1;
                if stack.len() < n {
                    return Err(CalcError::Underflow);
                }
                // C: top = *ptop--; if (*ptop > top || isnan(top)) *ptop = top;
                let base = stack.len() - n;
                let mut acc = stack[stack.len() - 1];
                for i in (base..stack.len() - 1).rev() {
                    let deeper = stack[i];
                    acc = if deeper > acc || acc.is_nan() {
                        acc
                    } else {
                        deeper
                    };
                }
                stack.truncate(base);
                stack.push(acc);
            }

            // Store. The stack is popped whether or not the arg exists — C's
            // guarded store (`sCalcPerform.c:432-438`) pops in both arms too;
            // only the assignment is conditional.
            STORE_A..=STORE_U => {
                let v = pop1(&mut stack)?;
                if let Some(slot) = inputs.num_arg_mut(usize::from(b - STORE_A)) {
                    *slot = v;
                }
            }

            // A byte outside base's enum: no compiler writes one.
            _ => return Err(CalcError::Internal),
        }
    }

    // C calcPerform.c:419-420 — the stack must hold exactly one value at
    // END_EXPRESSION; otherwise the postfix was malformed.
    // A push the frame's array could not hold. C has no such test — it writes
    // past `stack[CALCPERFORM_STACK]` and keeps going — so this is the port
    // refusing where C corrupts, and it is checked before the result is read so
    // no overflowed run can answer.
    if stack.overflowed() {
        return Err(CalcError::Overflow);
    }
    if stack.len() != 1 {
        return Err(CalcError::Internal);
    }
    Ok(stack[0])
}

/// `postfix.h:31` — `#define CALCPERFORM_STACK 80`, the ceiling the numeric
/// element table declares (`token.rs` `NUMERIC_TABLE.stack_size`) and the
/// compiler refuses to compile past.
const CALCPERFORM_STACK: usize = 80;

/// C `calcPerform`'s `double stack[CALCPERFORM_STACK+1]` (`calcPerform.c:49`).
type Stack = super::stack::Stack<f64, CALCPERFORM_STACK>;

fn pop1(stack: &mut Stack) -> Result<f64, CalcError> {
    stack.pop().ok_or(CalcError::Underflow)
}

fn pop2(stack: &mut Stack) -> Result<(f64, f64), CalcError> {
    let b = stack.pop().ok_or(CalcError::Underflow)?;
    let a = stack.pop().ok_or(CalcError::Underflow)?;
    Ok((a, b))
}

#[cfg(test)]
mod parity_tests {
    //! C-parity regression tests for calc engine fixes.
    use crate::calc::engine::bytecode::base::*;
    use crate::calc::engine::cast::{d2i, d2ui};
    use crate::calc::engine::error::calc_error_str;
    use crate::calc::{
        ArrayInputs, CalcError, NumericInputs, acalc, acalc_compile, calc, compile, eval,
    };

    fn run(expr: &str) -> f64 {
        let mut inp = NumericInputs::new();
        calc(expr, &mut inp).unwrap()
    }

    // malformed postfix leaving multiple residual values must error.
    #[test]
    fn c1_runtime_depth_multi_value_errors() {
        // Hand-built postfix: two literals, no operator, then END.
        let prog = [LITERAL_INT, 1, 0, 0, 0, LITERAL_INT, 2, 0, 0, 0, END];
        let mut inp = NumericInputs::new();
        assert_eq!(eval(&prog, &mut inp), Err(CalcError::Internal));
    }

    /// This asserted that the empty program yields 0.0. It does not: base
    /// `postfix("")` never even produces one (`CALC_ERR_NULL_ARG`,
    /// postfix.c:235-241), and the empty program a FAILED compile leaves behind
    /// makes `calcPerform` return -1 (its closing `ptop != stack + 1`,
    /// calcPerform.c:419-420) — which is how a broken CALC alarms on every
    /// process. Compiled C confirms both. See tests/calc_empty_program.rs.
    #[test]
    fn c1_the_empty_program_is_an_evaluation_error() {
        use crate::calc::{CompiledExpr, ExprKind};
        assert_eq!(compile("").err(), Some(CalcError::NullArg));

        let mut inp = NumericInputs::new();
        assert_eq!(
            eval(&CompiledExpr::empty(ExprKind::Numeric), &mut inp),
            Err(CalcError::EmptyProgram)
        );
    }

    // compiler rejects net runtime depth != 1.
    #[test]
    fn h1_two_subexpressions_incomplete() {
        // `A;B` leaves two values on the runtime stack -> Incomplete.
        assert!(matches!(compile("A;B"), Err(CalcError::Incomplete)));
    }

    #[test]
    fn h1_too_many_at_semicolon() {
        // At the second `;`, runtime_depth is 2 -> CALC_ERR_TOOMANY.
        assert!(matches!(compile("A;B;C"), Err(CalcError::TooMany)));
    }

    #[test]
    fn h1_store_terminated_expression_is_incomplete() {
        // `:=` is in base's element table too (`postfix.c:162`), with the same
        // runtime_effect -1 as sCalc's — so a store-terminated source ends at
        // depth 0 and fails the `runtime_depth != 1` check. Compiled base
        // postfix: `A:=5` is CALC_ERR_INCOMPLETE, `A:=5;A` is 0.
        //
        // The port used to compile `A:=5` on the theory that base has no `:=`
        // and synApps allows a value-less program. Neither is true.
        assert!(matches!(compile("A:=5"), Err(CalcError::Incomplete)));
    }

    #[test]
    fn h1_well_formed_still_compiles() {
        assert!(compile("A+B").is_ok());
        assert!(compile("A:=5;A+1").is_ok());
        assert!(compile("(A+B)*C").is_ok());
    }

    // Regression: the end-of-expression depth check must accept
    // well-formed array/string extension opcodes. The arity of every
    // non-vararg function (incl. 0-arg ARNDM/IX, 2-arg CAT/NSMOO/NDERIV/
    // FITPOLY/FITQ, 3-arg FITMPOLY/FITMQ) is now modelled exactly.
    //
    // These compile through `acalc_compile` — the aCalcPostfix.c element table
    // is the only one of the three that has these tokens. The numeric
    // `compile()` rejects them (see `numeric_calc_rejects_foreign_engine_tokens`).
    #[test]
    fn h1_array_extension_ops_compile() {
        // 2-arg array concat: CAT consumes 2 operands, pushes 1.
        assert!(acalc_compile("CAT(AA,BB)").is_ok());
        assert!(acalc_compile("CAT(AA,4)").is_ok());
        // 0-arg array generator: ARNDM consumes 0, pushes 1.
        assert!(acalc_compile("ARNDM").is_ok());
        // Other 2-arg array ops.
        assert!(acalc_compile("NSMOO(AA,2)").is_ok());
        assert!(acalc_compile("NDERIV(AA,5)").is_ok());
        assert!(acalc_compile("FITMPOLY(AA,BB)").is_ok());
        // FITPOLY is a UNARY_OPERATOR (`aCalcPostfix.c:142`) — one operand, and a
        // second is a COMPILE error, not an extra argument. Compiled C:
        // `FITPOLY(AA,BB)` is "Incomplete expression, operand missing", the same
        // rejection `SUM(AA,BB)` gets.
        assert!(acalc_compile("FITPOLY(AA)").is_ok());
        assert!(acalc_compile("FITPOLY(AA,BB)").is_err());
        // FITQ/FITMQ are VARARG_OPERATORs (`:140-141`): their trailing arguments
        // name the scalar arguments the coefficients are stored into.
        assert!(acalc_compile("FITQ(AA)").is_ok());
        assert!(acalc_compile("FITQ(AA,C,D,E)").is_ok());
        assert!(acalc_compile("FITMQ(AA,BB,C,D,E)").is_ok());

        // And acalc end-to-end still produces an array result. CAT cannot grow the
        // `arraySize` buffer: with AA carrying no window its `lastEl` is already
        // arraySize-1, so C copies nothing (`aCalcPerform.c:1359-1364`) and the
        // result is AA. See `tests/calc_array_window.rs` for the windowed cases.
        let mut inputs = ArrayInputs::new(3);
        inputs.arrays[0] = vec![1.0, 2.0, 3.0];
        inputs.arrays[1] = vec![4.0, 5.0, 6.0];
        let cat = acalc("CAT(AA,BB)", &mut inputs).unwrap();
        assert_eq!(
            cat,
            crate::calc::ArrayStackValue::array(vec![1.0, 2.0, 3.0])
        );

        let mut inputs2 = ArrayInputs::new(5);
        let arndm = acalc("ARNDM", &mut inputs2).unwrap();
        match arndm {
            crate::calc::ArrayStackValue::Array(cell) => assert_eq!(cell.buf().len(), 5),
            other => panic!("expected Array, got {other:?}"),
        }
    }

    // Regression: an aCalc expression is rejected by the numeric compiler for
    // the reason C rejects it — `CAT` and `AA` are not in postfix.c's ELEMENT
    // table, so C's lexer never reaches the arity question. Verified against
    // the C compiler: `CAT(AA)` is CALC_ERR_SYNTAX (11), not an arity error.
    // (aCalc's own arity check for CAT lives in the array engine's tests.)
    #[test]
    fn h1_array_extension_arity_mismatch_rejected() {
        assert!(matches!(compile("CAT(AA)"), Err(CalcError::Syntax)));
    }

    // max/min NaN propagation matches C.
    #[test]
    fn h2_max_nan_first_arg() {
        // C: max(nan,1) = nan (incoming operand decides).
        assert!(run("MAX(0/0,1)").is_nan());
    }

    #[test]
    fn h2_max_nan_second_arg() {
        assert!(run("MAX(1,0/0)").is_nan());
    }

    #[test]
    fn h2_max_normal() {
        assert_eq!(run("MAX(3,1,2)"), 3.0);
        assert_eq!(run("MIN(3,1,2)"), 1.0);
    }

    #[test]
    fn h2_min_nan_propagates() {
        assert!(run("MIN(0/0,5)").is_nan());
        assert!(run("MIN(5,0/0)").is_nan());
    }

    /// CBUG-A2 — NINT rounds, then narrows through `d2i`, tracking
    /// `calcPerform.c:313-317` at unmerged PR #925. This used to pin the clean
    /// no-narrow deviation (`NINT(3e9) == 3e9`); before that it pinned C's
    /// `(epicsInt32)` cvttsd2si value (`i32::MIN`).
    #[test]
    fn h3_nint_out_of_range_is_the_d2i_narrowed_value() {
        // 3e9 is in `[2^31, 2^32)`: d2i routes it through u32 and bit 31 becomes
        // the sign — `d2i(3000000000.5) = -1294967296`. Bit-identical to fixed C.
        assert_eq!(run("NINT(3000000000)"), -1_294_967_296.0);
        // Negative / `>= 2^32` / NaN / Inf are the window d2i STILL leaves
        // undefined in C (PR #925 is a partial fix). The port mirrors its own
        // d2i — the modular wrap every bitwise operand already uses — rather than
        // reproducing x86 cvttsd2si; these are not C-defined values.
        assert_eq!(run("NINT(-3000000000)"), 1_294_967_296.0);
        assert_eq!(run("NINT(2500000000.4)"), -1_794_967_296.0);
        // NaN/Inf round to NaN/Inf; d2i(NaN)=d2i(Inf)=0 here (UB in C).
        assert_eq!(run("NINT(0/0)"), 0.0);
        assert_eq!(run("NINT(1/0)"), 0.0);
    }

    #[test]
    fn h3_nint_small_values() {
        assert_eq!(run("NINT(2.5)"), 3.0);
        assert_eq!(run("NINT(-2.5)"), -3.0);
        assert_eq!(run("NINT(2.4)"), 2.0);
        // In-range values are bit-identical to C, including the int32 edges.
        assert_eq!(run("NINT(2147483647)"), 2_147_483_647.0);
        assert_eq!(run("NINT(-2147483648)"), -2_147_483_648.0);
    }

    /// CBUG-A2 — MODULO narrows both operands through `d2i`, tracking
    /// `calcPerform.c:176-190` at unmerged PR #925. These used to pin the
    /// clean no-narrow deviation.
    #[test]
    fn h4_mod_out_of_range_operands_are_d2i_narrowed() {
        // Divisor 2^32: d2i(2^32) = 0, so the divisor narrows to zero and base's
        // rule makes it NaN (the clean deviation answered 5).
        assert!(run("5 % 4294967296").is_nan());
        // Dividend 3e9: d2i(3e9) = -1294967296, and -1294967296 % 7 == 0 (7
        // divides it exactly). Fixed C agrees; the clean deviation answered 4.
        assert_eq!(run("3000000000 % 7"), 0.0);
        // NaN dividend: d2i(NaN) = 0 here (UB in C), so 0 % 7 = 0.
        assert_eq!(run("(0/0) % 7"), 0.0);
        // NaN divisor: d2i(NaN) = 0 narrows to a zero divisor -> NaN.
        assert!(run("7 % (0/0)").is_nan());
    }

    #[test]
    fn h4_mod_normal() {
        assert_eq!(run("17 % 5"), 2.0);
        assert!(run("5 % 0").is_nan());
        // The divisor is zero exactly when it TRUNCATES to zero, as in C.
        assert!(run("5 % 0.5").is_nan());
        // C's truncated remainder: the sign follows the dividend.
        assert_eq!(run("-17 % 5"), -2.0);
        assert_eq!(run("17 % -5"), 2.0);
        // Operands truncate toward zero before the remainder, as in C.
        assert_eq!(run("17.9 % 5.9"), 2.0);
    }

    // bitwise ops use d2i/d2ui (wrap), not saturating `as i32`.
    #[test]
    fn h5_bitand_high_bit_value() {
        // 3e9 has bit 31 set; 3e9 & 0xFFFFFFFF == 3e9 as epicsInt32 == -1294967296.
        assert_eq!(run("3000000000 & 4294967295"), -1294967296.0);
    }

    #[test]
    fn h5_d2i_d2ui_helpers() {
        assert_eq!(d2i(3_000_000_000.0), -1_294_967_296);
        assert_eq!(d2ui(3_000_000_000.0), 3_000_000_000);
        assert_eq!(d2i(-1.0), -1);
        assert_eq!(d2ui(-1.0), 0xFFFF_FFFF);
    }

    // nested ternary branch selection matches C cond_search.
    #[test]
    fn m6_nested_ternary_all_branches() {
        // a?(b?c:d):(e?f:g) — exercise each leaf.
        let expr = "A?(B?1:2):(C?3:4)";
        let cases = [
            // (A, B, C, expected)
            (1.0, 1.0, 0.0, 1.0),
            (1.0, 0.0, 0.0, 2.0),
            (0.0, 0.0, 1.0, 3.0),
            (0.0, 0.0, 0.0, 4.0),
        ];
        for (a, b, c, want) in cases {
            let mut inp = NumericInputs::new();
            inp.vars[0] = a;
            inp.vars[1] = b;
            inp.vars[2] = c;
            assert_eq!(calc(expr, &mut inp).unwrap(), want, "A={a} B={b} C={c}");
        }
    }

    // hex literal must fit in 32 bits, and runs as the int32 C's LITERAL_INT is.
    #[test]
    fn m3_hex_literal_32bit() {
        assert_eq!(run("0xFFFFFFFF"), -1.0);
        assert_eq!(run("0x80000000"), -2147483648.0);
        assert_eq!(run("0x7FFFFFFF"), 2147483647.0);
        assert!(matches!(compile("0x1FFFFFFFF"), Err(CalcError::BadLiteral)));
    }

    // numeric error codes and calcErrorStr.
    #[test]
    fn m5_error_codes_and_strings() {
        assert_eq!(CalcError::TooMany.code(), 1);
        assert_eq!(CalcError::Incomplete.code(), 8);
        assert_eq!(CalcError::Syntax.code(), 11);
        assert_eq!(calc_error_str(0), Some("No error"));
        assert_eq!(
            calc_error_str(8),
            Some("Incomplete expression, operand missing")
        );
        assert_eq!(calc_error_str(14), None);
        assert_eq!(calc_error_str(-1), None);
    }

    // L-2: calcArgUsage equivalent.
    #[test]
    fn l2_arg_usage() {
        // A reads bit 0, C reads bit 2.
        let (inputs, stores) = compile("A+C").unwrap().arg_usage();
        assert_eq!(inputs, 0b101);
        assert_eq!(stores, 0);
        // B is stored before any read -> not an input; A is a real input.
        let (inputs, stores) = compile("B:=2;A+B").unwrap().arg_usage();
        assert_eq!(inputs, 0b001);
        assert_eq!(stores, 0b010);
    }

    // L-4: random stays within [0,1].
    #[test]
    fn l4_random_range() {
        for _ in 0..1000 {
            let r = run("RNDM");
            assert!((0.0..=1.0).contains(&r), "RNDM out of range: {r}");
        }
    }

    /// base RNDM replays `calcRandom` (`calcPerform.c:518-523`), NOT sCalc's
    /// `local_random`: the first draw of compiled C is 49156/65535
    /// (0.75007248…), where synApps' is 49157/65536 (0.75007629…).
    /// A fresh thread, because the seed is thread-private.
    #[test]
    fn l4_random_first_draws_are_base_c_not_synapps() {
        let got = std::thread::spawn(|| (0..3).map(|_| run("RNDM")).collect::<Vec<_>>())
            .join()
            .unwrap();
        let mut s: u16 = 0xa3bf;
        let expected: Vec<f64> = (0..3)
            .map(|_| {
                s = s.wrapping_mul(1533).wrapping_add(0x3141);
                f64::from(s) / 65535.0
            })
            .collect();
        assert_eq!(got, expected);
        assert_eq!(got[0], 49156.0 / 65535.0);
    }

    /// NRNDM is not in base's element table, so it cannot compile, and base's
    /// enum has no code for it; a byte past the enum (`NOT_GENERATED`) is
    /// refused rather than run.
    #[test]
    fn l4_normal_random_is_not_a_base_operator() {
        assert_eq!(compile("NRNDM").unwrap_err(), CalcError::Syntax);
        let not_generated = COND_END + 1;
        assert_eq!(
            eval(&[not_generated, END], &mut NumericInputs::new()).unwrap_err(),
            CalcError::Internal
        );
    }

    /// The frame array's ceiling is refused, not written past. C has no test
    /// here — `calcPerform` indexes off the end of `stack[CALCPERFORM_STACK+1]`
    /// — so the program has to be hand-assembled: the compiler stops at
    /// `runtime_depth >= CALCPERFORM_STACK`, which is why no compiled
    /// expression can reach this.
    #[test]
    fn a_push_past_the_frame_array_fails_the_evaluation() {
        for (pushes, expected) in [
            (super::CALCPERFORM_STACK, CalcError::Internal),
            (super::CALCPERFORM_STACK + 1, CalcError::Overflow),
        ] {
            let mut hand_built: Vec<u8> = (0..pushes as i32)
                .flat_map(|i| std::iter::once(LITERAL_INT).chain(i.to_ne_bytes()))
                .collect();
            hand_built.push(END);
            assert_eq!(
                eval(&hand_built, &mut NumericInputs::new()).unwrap_err(),
                expected,
                "{pushes} pushes"
            );
        }
    }

    // L-5: VAL token reads the previous result, not the stack top.
    #[test]
    fn l5_fetchval_reads_prev_val() {
        let compiled = compile("VAL+1").unwrap();
        let mut inp = NumericInputs::new();
        inp.prev_val = 41.0;
        assert_eq!(eval(&compiled, &mut inp).unwrap(), 42.0);
    }
}
