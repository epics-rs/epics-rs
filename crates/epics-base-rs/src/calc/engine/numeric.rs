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
                CoreOp::FetchVal => {
                    // C calcPerform.c:74-76 — FETCH_VAL pushes *presult, the
                    // record's previous calculation result (the VAL field).
                    stack.push(inputs.prev_val);
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
                    // C uses IEEE 754: 1.0/0.0 = Inf, 0.0/0.0 = NaN
                    stack.push(a / b);
                }
                CoreOp::Mod => {
                    let (a, b) = pop2(&mut stack)?;
                    // C: itop = (epicsInt32)den; if(itop) (epicsInt32)num % itop else NaN
                    let den = d2i(b);
                    if den == 0 {
                        stack.push(f64::NAN);
                    } else {
                        stack.push((d2i(a).wrapping_rem(den)) as f64);
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

                // Comparison - exact comparison like C (no epsilon)
                CoreOp::Eq => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(if a == b { 1.0 } else { 0.0 });
                }
                CoreOp::Ne => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(if a != b { 1.0 } else { 0.0 });
                }
                CoreOp::Lt => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(if a < b { 1.0 } else { 0.0 });
                }
                CoreOp::Le => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(if a <= b { 1.0 } else { 0.0 });
                }
                CoreOp::Gt => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(if a > b { 1.0 } else { 0.0 });
                }
                CoreOp::Ge => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(if a >= b { 1.0 } else { 0.0 });
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

                // Bitwise - use C's d2i/d2ui conversion (wrap-on-overflow 32-bit)
                CoreOp::BitAnd => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push((d2i(a) & d2i(b)) as f64);
                }
                CoreOp::BitOr => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push((d2i(a) | d2i(b)) as f64);
                }
                CoreOp::BitXor => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push((d2i(a) ^ d2i(b)) as f64);
                }
                CoreOp::BitNot => {
                    let a = pop1(&mut stack)?;
                    stack.push(!d2i(a) as f64);
                }
                CoreOp::Shl => {
                    let (a, b) = pop2(&mut stack)?;
                    // C: d2i(*ptop) << (d2i(top) & 31)
                    stack.push((d2i(a) << (d2i(b) & 31)) as f64);
                }
                CoreOp::Shr => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push((d2i(a) >> (d2i(b) & 31)) as f64);
                }
                CoreOp::ShrLogical => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push((d2ui(a) >> (d2ui(b) & 31)) as f64);
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
                    // C: *ptop = (epicsInt32)(top>=0 ? top+0.5 : top-0.5)
                    let pre = if a >= 0.0 { a + 0.5 } else { a - 0.5 };
                    stack.push(f64_to_i32_wrap(pre) as f64);
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
                CoreOp::Fmod => {
                    let (a, b) = pop2(&mut stack)?;
                    stack.push(a % b);
                }

                // Vararg min/max
                CoreOp::Max(nargs) => {
                    let n = *nargs as usize;
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
                CoreOp::Min(nargs) => {
                    let n = *nargs as usize;
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

    // C calcPerform.c:418-422 — the stack must hold exactly one value at
    // END_EXPRESSION; otherwise the postfix was malformed.
    // The empty-expression program ([End] only) is a deliberate Rust
    // special case (`compile("")`) and yields 0.0; every other program
    // must leave exactly one residual value.
    let is_empty_program = code
        .iter()
        .all(|op| matches!(op, Opcode::Core(CoreOp::End)));
    if is_empty_program {
        return Ok(stack.first().copied().unwrap_or(0.0));
    }
    if stack.len() != 1 {
        return Err(CalcError::Internal);
    }
    Ok(stack[0])
}

/// C `d2i` macro: positive doubles route through `epicsUInt32` first so that
/// values with bit 31 set become the corresponding signed (negative) pattern;
/// negative doubles cast directly. Wraps modulo 2^32 on overflow.
#[inline]
fn d2i(x: f64) -> i32 {
    if x < 0.0 {
        f64_to_i32_wrap(x)
    } else {
        f64_to_u32_wrap(x) as i32
    }
}

/// C `d2ui` macro: negative doubles cast to `epicsInt32` then reinterpreted
/// as `epicsUInt32`; non-negative doubles cast directly.
#[inline]
fn d2ui(x: f64) -> u32 {
    if x < 0.0 {
        f64_to_i32_wrap(x) as u32
    } else {
        f64_to_u32_wrap(x)
    }
}

/// `(epicsInt32)x` — C cast of a double to a 32-bit signed integer with
/// wrap-on-overflow semantics (matching x86 behavior; Rust `as` saturates).
#[inline]
fn f64_to_i32_wrap(x: f64) -> i32 {
    if x.is_nan() {
        return 0;
    }
    let t = x.trunc();
    // Reduce modulo 2^32 then reinterpret the low 32 bits as signed.
    let m = t.rem_euclid(4294967296.0);
    (m as u64 as u32) as i32
}

/// `(epicsUInt32)x` — C cast of a double to a 32-bit unsigned integer with
/// wrap-on-overflow semantics.
#[inline]
fn f64_to_u32_wrap(x: f64) -> u32 {
    if x.is_nan() {
        return 0;
    }
    let t = x.trunc();
    let m = t.rem_euclid(4294967296.0);
    m as u64 as u32
}

fn pop1(stack: &mut Vec<f64>) -> Result<f64, CalcError> {
    stack.pop().ok_or(CalcError::Underflow)
}

fn pop2(stack: &mut Vec<f64>) -> Result<(f64, f64), CalcError> {
    let b = stack.pop().ok_or(CalcError::Underflow)?;
    let a = stack.pop().ok_or(CalcError::Underflow)?;
    Ok((a, b))
}

/// Forward-scan for a matching conditional opcode, mirroring C `cond_search`
/// (calcPerform.c:520-557): `count` starts at 1, every occurrence of the target
/// opcode decrements it (return when 0), and every `COND_IF` increments it.
/// `COND_ELSE`/`COND_END` are not specially tracked for nesting.
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
    // C calcRandom() returns (double)rand()/RAND_MAX — a closed [0,1] range
    // (both endpoints reachable). Map 53 random bits onto [0,1] inclusive.
    (s >> 11) as f64 / ((1u64 << 53) - 1) as f64
}

#[cfg(test)]
mod parity_tests {
    //! C-parity regression tests for calc engine fixes (doc/parity-review/01-calc.md).
    use super::{d2i, d2ui, f64_to_i32_wrap};
    use crate::calc::engine::error::calc_error_str;
    use crate::calc::{ArrayInputs, CalcError, NumericInputs, acalc, calc, compile, eval};

    fn run(expr: &str) -> f64 {
        let mut inp = NumericInputs::new();
        calc(expr, &mut inp).unwrap()
    }

    // malformed postfix leaving multiple residual values must error.
    #[test]
    fn c1_runtime_depth_multi_value_errors() {
        // Hand-built postfix: two PushConst, no operator, then End.
        use crate::calc::engine::opcodes::{CoreOp, Opcode};
        use crate::calc::{CompiledExpr, ExprKind};
        let prog = CompiledExpr {
            code: vec![
                Opcode::Core(CoreOp::PushConst(1.0)),
                Opcode::Core(CoreOp::PushConst(2.0)),
                Opcode::Core(CoreOp::End),
            ],
            kind: ExprKind::Numeric,
            loop_pairs: Vec::new(),
        };
        let mut inp = NumericInputs::new();
        assert_eq!(eval(&prog, &mut inp), Err(CalcError::Internal));
    }

    #[test]
    fn c1_empty_program_still_yields_zero() {
        let compiled = compile("").unwrap();
        let mut inp = NumericInputs::new();
        assert_eq!(eval(&compiled, &mut inp).unwrap(), 0.0);
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
    fn h1_store_terminated_expression_compiles() {
        // This engine is a synApps superset: sCalc `:=` store-assignment is
        // a valid construct that epics-base `postfix.c` does not have.
        // `A:=5` is store-terminated — it ends at runtime depth 0 with a
        // store as its final opcode — and must compile successfully.
        use crate::calc::engine::opcodes::{CoreOp, Opcode};
        let compiled = compile("A:=5").expect("A:=5 is a valid sCalc store");
        assert_eq!(
            compiled.code,
            vec![
                Opcode::Core(CoreOp::PushConst(5.0)),
                Opcode::Core(CoreOp::StoreVar(0)),
                Opcode::Core(CoreOp::End),
            ]
        );
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
    #[test]
    fn h1_array_extension_ops_compile() {
        // 2-arg array concat: CAT consumes 2 operands, pushes 1.
        assert!(compile("CAT(AA,BB)").is_ok());
        assert!(compile("CAT(AA,4)").is_ok());
        // 0-arg array generator: ARNDM consumes 0, pushes 1.
        assert!(compile("ARNDM").is_ok());
        // Other 2-arg array ops.
        assert!(compile("NSMOO(AA,2)").is_ok());
        assert!(compile("NDERIV(AA,5)").is_ok());
        assert!(compile("FITPOLY(AA,BB)").is_ok());
        assert!(compile("FITQ(AA,BB)").is_ok());

        // And acalc end-to-end still produces an array result.
        let mut inputs = ArrayInputs::new(3);
        inputs.arrays[0] = vec![1.0, 2.0, 3.0];
        inputs.arrays[1] = vec![4.0, 5.0, 6.0];
        let cat = acalc("CAT(AA,BB)", &mut inputs).unwrap();
        assert_eq!(
            cat,
            crate::calc::ArrayStackValue::Array(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0])
        );

        let mut inputs2 = ArrayInputs::new(5);
        let arndm = acalc("ARNDM", &mut inputs2).unwrap();
        match arndm {
            crate::calc::ArrayStackValue::Array(arr) => assert_eq!(arr.len(), 5),
            other => panic!("expected Array, got {other:?}"),
        }
    }

    // Regression: a genuinely malformed array expression is still
    // rejected — too few operands for a 2-arg function leaves the
    // runtime stack short of exactly 1 value.
    #[test]
    fn h1_array_extension_arity_mismatch_rejected() {
        // CAT needs 2 operands; one operand -> net depth 0 -> Incomplete.
        assert!(matches!(compile("CAT(AA)"), Err(CalcError::Incomplete)));
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

    // nint uses 32-bit wrap, not i64 saturation.
    #[test]
    fn h3_nint_wraps_at_32bit() {
        // 3e9 rounds to 3000000000, wrapping into a negative epicsInt32.
        assert_eq!(run("NINT(3000000000)"), -1294967296.0);
    }

    #[test]
    fn h3_nint_small_values() {
        assert_eq!(run("NINT(2.5)"), 3.0);
        assert_eq!(run("NINT(-2.5)"), -3.0);
        assert_eq!(run("NINT(2.4)"), 2.0);
    }

    // MODULO uses epicsInt32 truncation and zero detection.
    #[test]
    fn h4_mod_large_denominator_is_int32_zero() {
        // 4294967296 == 2^32 truncates to epicsInt32 0 -> NaN.
        assert!(run("5 % 4294967296").is_nan());
    }

    #[test]
    fn h4_mod_normal() {
        assert_eq!(run("17 % 5"), 2.0);
        assert!(run("5 % 0").is_nan());
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
        assert_eq!(f64_to_i32_wrap(f64::NAN), 0);
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

    // hex literal must fit in 32 bits.
    #[test]
    fn m3_hex_literal_32bit() {
        assert_eq!(run("0xFFFFFFFF"), 4294967295.0);
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

    // L-5: VAL token reads the previous result, not the stack top.
    #[test]
    fn l5_fetchval_reads_prev_val() {
        let compiled = compile("VAL+1").unwrap();
        let mut inp = NumericInputs::new();
        inp.prev_val = 41.0;
        assert_eq!(eval(&compiled, &mut inp).unwrap(), 42.0);
    }
}
