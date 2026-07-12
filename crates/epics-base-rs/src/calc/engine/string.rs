use super::cast::{c_int, c_long, d2ui};
use super::cvt;
use super::error::CalcError;
use super::opcodes::{CoreOp, Opcode, StringOp};
use super::value::{ScalcString, StackValue};
use super::{CompiledExpr, StringInputs};

pub fn eval(expr: &CompiledExpr, inputs: &mut StringInputs) -> Result<StackValue, CalcError> {
    // C `sCalcPerform.c:396` — `if (*post == END_EXPRESSION) return(-1);`,
    // checked before anything else, so an empty CALC (which sCalcPostfix
    // ACCEPTS, CLCV 0) and a failed one behave identically at run time: the
    // record alarms every process and VAL/SVAL keep their previous values.
    if expr.is_empty() {
        return Err(CalcError::EmptyProgram);
    }

    let mut stack: Vec<StackValue> = Vec::with_capacity(20);
    let code = &expr.code;
    let mut pc = 0;
    let mut loop_count: usize = 0;
    // C compiles the USES_STRING marker into the postfix and `sCalcPerform`
    // switches on it ONCE (`sCalcPerform.c:399`) to pick a whole evaluator.
    let string_path = uses_string(code);

    while pc < code.len() {
        let op = &code[pc];
        pc += 1;

        match op {
            Opcode::Core(core) => match core {
                CoreOp::End => break,

                CoreOp::PushConst(v) => stack.push(StackValue::Double(*v)),
                CoreOp::PushVar(idx) => {
                    stack.push(StackValue::Double(inputs.num_vars[*idx as usize]));
                }
                CoreOp::PushDoubleVar(idx) => {
                    // In string evaluator, double vars are string vars
                    stack.push(StackValue::Str(inputs.str_vars[*idx as usize].clone()));
                }

                CoreOp::Pi => stack.push(StackValue::Double(std::f64::consts::PI)),
                CoreOp::D2R => {
                    stack.push(StackValue::Double(std::f64::consts::PI / 180.0));
                }
                CoreOp::R2D => {
                    stack.push(StackValue::Double(180.0 / std::f64::consts::PI));
                }
                // C `CONST_S2R` / `CONST_R2S` (`sCalcPerform.c:952-962`):
                // arcseconds <-> radians, `PI/(180*3600)` and its reciprocal.
                CoreOp::S2R => {
                    stack.push(StackValue::Double(std::f64::consts::PI / (180.0 * 3600.0)));
                }
                CoreOp::R2S => {
                    stack.push(StackValue::Double((180.0 * 3600.0) / std::f64::consts::PI));
                }

                CoreOp::Random => {
                    stack.push(StackValue::Double(simple_random()));
                }
                CoreOp::NormalRandom => {
                    let u1 = simple_random();
                    let u2 = simple_random();
                    let n = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
                    stack.push(StackValue::Double(n));
                }
                CoreOp::FetchVal => {
                    // C FETCH_VAL pushes *presult (the record's previous result).
                    stack.push(StackValue::Double(inputs.prev_val));
                }
                CoreOp::FetchSval => {
                    // C FETCH_SVAL (sCalcPerform.c:927-932) pushes `psresult`,
                    // the record's previous *string* result. (C's `strncpy(...,
                    // SCALC_STRING_SIZE)` there is the fixed char[40] copy of
                    // the same buffer; the port's SVAL is an unbounded PvString,
                    // so the stored value is seeded verbatim.)
                    stack.push(StackValue::Str(inputs.prev_sval.clone()));
                }

                // Type-aware arithmetic
                CoreOp::Add => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(match Pair::of(a, b) {
                        Pair::Numeric(x, y) => StackValue::Double(x + y),
                        // C `strncat(ps->s, ps1->s, SCALC_STRING_SIZE-strlen(ps->s)-1)`
                        // (sCalcPerform.c:975) — the concatenation is written into
                        // the 40-byte stack element, so it is bounded. That bound
                        // is `StackValue::str`.
                        Pair::Strings(x, y) => {
                            StackValue::str([x.as_bytes(), y.as_bytes()].concat())
                        }
                    });
                }
                CoreOp::Sub => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(match Pair::of(a, b) {
                        Pair::Numeric(x, y) => StackValue::Double(x - y),
                        // C SUB: remove the first occurrence of y from x.
                        Pair::Strings(x, y) => {
                            let mut out = x.into_bytes();
                            if let Some(pos) = find_sub(&out, y.as_bytes()) {
                                out.drain(pos..pos + y.len());
                            }
                            StackValue::str(out)
                        }
                    });
                }
                CoreOp::Mul => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    stack.push(StackValue::Double(a * b));
                }
                CoreOp::Div => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    // sCalc, not base: a zero divisor is an ERROR, on both
                    // evaluator paths (`sCalcPerform.c:495-500` no-string,
                    // :1022-1030 string) — `return(-1)`, with `*presult`
                    // never written. Base's DIV (`calcPerform.c:156-159`) is
                    // a bare IEEE divide and the old comment here cited it;
                    // the port must not inherit base's rule in sCalc.
                    if b == 0.0 {
                        return Err(CalcError::DivisionByZero);
                    }
                    stack.push(StackValue::Double(a / b));
                }
                CoreOp::Mod => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    // sCalc, not base: a zero divisor is an ERROR
                    // (`return(-1)`), never NaN, and the operands take a plain
                    // C cast whose WIDTH depends on which evaluator C picked
                    // for this program (see `uses_string`):
                    //   no-string path (sCalcPerform.c:558-563): `(int)`
                    //   string path    (sCalcPerform.c:1102-1110): `(long)`
                    let value = if string_path {
                        let den = c_long(b);
                        if den == 0 {
                            return Err(CalcError::DivisionByZero);
                        }
                        c_long(a).wrapping_rem(den) as f64
                    } else {
                        let den = c_int(b);
                        if den == 0 {
                            return Err(CalcError::DivisionByZero);
                        }
                        c_int(a).wrapping_rem(den) as f64
                    };
                    stack.push(StackValue::Double(value));
                }
                CoreOp::Neg => {
                    let a = pop1_f64(&mut stack)?;
                    stack.push(StackValue::Double(-a));
                }
                CoreOp::Power => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    stack.push(StackValue::Double(a.powf(b)));
                }

                // Type-aware comparison. sCalc does NOT compare doubles exactly:
                // every numeric comparison goes through `SMALL` (= 1e-11,
                // `sCalcPerform.c:46`) on both evaluator paths — :595-620
                // (no-string) and :1161-1255 (string), which are the same six
                // formulas. Two strings are compared with `strcmp` (:1167-1169).
                // The exact IEEE operators these arms used to apply are base's
                // (`calcPerform.c:369-393`) and aCalc's, not sCalc's.
                CoreOp::Eq => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    let result = match Pair::of(a, b) {
                        // C: fabs(a-b) < SMALL
                        Pair::Numeric(x, y) => (x - y).abs() < SMALL,
                        // C compares two strings with strcmp.
                        Pair::Strings(x, y) => x == y,
                    };
                    stack.push(StackValue::Double(if result { 1.0 } else { 0.0 }));
                }
                CoreOp::Ne => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    let result = match Pair::of(a, b) {
                        // C: fabs(a-b) > SMALL
                        Pair::Numeric(x, y) => (x - y).abs() > SMALL,
                        // C compares two strings with strcmp.
                        Pair::Strings(x, y) => x != y,
                    };
                    stack.push(StackValue::Double(if result { 1.0 } else { 0.0 }));
                }
                CoreOp::Lt => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    let result = match Pair::of(a, b) {
                        // C: (b - a) > SMALL
                        Pair::Numeric(x, y) => (y - x) > SMALL,
                        // C compares two strings with strcmp.
                        Pair::Strings(x, y) => x < y,
                    };
                    stack.push(StackValue::Double(if result { 1.0 } else { 0.0 }));
                }
                CoreOp::Le => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    let result = match Pair::of(a, b) {
                        // C: (fabs(a-b) < SMALL) || (a < b)
                        Pair::Numeric(x, y) => (x - y).abs() < SMALL || x < y,
                        // C compares two strings with strcmp.
                        Pair::Strings(x, y) => x <= y,
                    };
                    stack.push(StackValue::Double(if result { 1.0 } else { 0.0 }));
                }
                CoreOp::Gt => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    let result = match Pair::of(a, b) {
                        // C: (a - b) > SMALL
                        Pair::Numeric(x, y) => (x - y) > SMALL,
                        // C compares two strings with strcmp.
                        Pair::Strings(x, y) => x > y,
                    };
                    stack.push(StackValue::Double(if result { 1.0 } else { 0.0 }));
                }
                CoreOp::Ge => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    let result = match Pair::of(a, b) {
                        // C: (fabs(a-b) < SMALL) || (a > b)
                        Pair::Numeric(x, y) => (x - y).abs() < SMALL || x > y,
                        // C compares two strings with strcmp.
                        Pair::Strings(x, y) => x >= y,
                    };
                    stack.push(StackValue::Double(if result { 1.0 } else { 0.0 }));
                }

                // Logical
                CoreOp::And => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    stack.push(StackValue::Double(if a != 0.0 && b != 0.0 {
                        1.0
                    } else {
                        0.0
                    }));
                }
                CoreOp::Or => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    stack.push(StackValue::Double(if a != 0.0 || b != 0.0 {
                        1.0
                    } else {
                        0.0
                    }));
                }
                CoreOp::Not => {
                    let a = pop1_f64(&mut stack)?;
                    stack.push(StackValue::Double(if a == 0.0 { 1.0 } else { 0.0 }));
                }

                // Bitwise. sCalc has no `d2i`: every operand takes a plain
                // `(long)` cast (sCalcPerform.c:575-591, :623-631, :725-727),
                // so these are 64-bit ops, not base's 32-bit ones. The shift
                // count is not masked in C either — x86-64 `shl`/`sar` mask it
                // to 6 bits for a 64-bit operand, which is the observable.
                CoreOp::BitAnd => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    stack.push(StackValue::Double((c_long(a) & c_long(b)) as f64));
                }
                CoreOp::BitOr => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    stack.push(StackValue::Double((c_long(a) | c_long(b)) as f64));
                }
                CoreOp::BitXor => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    stack.push(StackValue::Double((c_long(a) ^ c_long(b)) as f64));
                }
                CoreOp::BitNot => {
                    let a = pop1_f64(&mut stack)?;
                    stack.push(StackValue::Double(!c_long(a) as f64));
                }
                CoreOp::Shl => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    stack.push(StackValue::Double((c_long(a) << (c_long(b) & 63)) as f64));
                }
                CoreOp::Shr => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    stack.push(StackValue::Double((c_long(a) >> (c_long(b) & 63)) as f64));
                }
                // `>>>` (RIGHT_SHIFT_LOGIC) is a BASE opcode: sCalcPostfix has
                // no such element, so a C sCalc expression can never contain
                // one and there is no sCalc semantics to match. Base's is kept
                // for the shared `CoreOp`; the grammar is what must refuse it.
                CoreOp::ShrLogical => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    stack.push(StackValue::Double((d2ui(a) >> (d2ui(b) & 31)) as f64));
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

                // Math functions
                CoreOp::Abs => {
                    let a = pop1_f64(&mut stack)?;
                    stack.push(StackValue::Double(a.abs()));
                }
                CoreOp::Sqrt => {
                    let a = pop1_f64(&mut stack)?;
                    // sCalc, not base: a negative operand is an ERROR, not
                    // NaN (`sCalcPerform.c:521-524` no-string, :1056-1061
                    // string — both `if (< 0) return(-1)` BEFORE the sqrt).
                    if a < 0.0 {
                        return Err(CalcError::DomainError);
                    }
                    stack.push(StackValue::Double(a.sqrt()));
                }
                CoreOp::Exp => {
                    let a = pop1_f64(&mut stack)?;
                    stack.push(StackValue::Double(a.exp()));
                }
                CoreOp::Log10 => {
                    let a = pop1_f64(&mut stack)?;
                    // sCalc, not base (`sCalcPerform.c:531-535`, :1068-1073).
                    // Note the test is `< 0`, so LOG(0) is NOT caught here —
                    // it produces -inf and is caught by the non-finite tail
                    // below, exactly as in C.
                    if a < 0.0 {
                        return Err(CalcError::DomainError);
                    }
                    stack.push(StackValue::Double(a.log10()));
                }
                CoreOp::LogE => {
                    let a = pop1_f64(&mut stack)?;
                    // sCalc, not base (`sCalcPerform.c:537-541`, :1075-1080).
                    if a < 0.0 {
                        return Err(CalcError::DomainError);
                    }
                    stack.push(StackValue::Double(a.ln()));
                }
                CoreOp::Sin => {
                    let a = pop1_f64(&mut stack)?;
                    stack.push(StackValue::Double(a.sin()));
                }
                CoreOp::Cos => {
                    let a = pop1_f64(&mut stack)?;
                    stack.push(StackValue::Double(a.cos()));
                }
                CoreOp::Tan => {
                    let a = pop1_f64(&mut stack)?;
                    stack.push(StackValue::Double(a.tan()));
                }
                CoreOp::Asin => {
                    let a = pop1_f64(&mut stack)?;
                    stack.push(StackValue::Double(a.asin()));
                }
                CoreOp::Acos => {
                    let a = pop1_f64(&mut stack)?;
                    stack.push(StackValue::Double(a.acos()));
                }
                CoreOp::Atan => {
                    let a = pop1_f64(&mut stack)?;
                    stack.push(StackValue::Double(a.atan()));
                }
                CoreOp::Sinh => {
                    let a = pop1_f64(&mut stack)?;
                    stack.push(StackValue::Double(a.sinh()));
                }
                CoreOp::Cosh => {
                    let a = pop1_f64(&mut stack)?;
                    stack.push(StackValue::Double(a.cosh()));
                }
                CoreOp::Tanh => {
                    let a = pop1_f64(&mut stack)?;
                    stack.push(StackValue::Double(a.tanh()));
                }
                CoreOp::Ceil => {
                    let a = pop1_f64(&mut stack)?;
                    stack.push(StackValue::Double(a.ceil()));
                }
                CoreOp::Floor => {
                    let a = pop1_f64(&mut stack)?;
                    stack.push(StackValue::Double(a.floor()));
                }
                CoreOp::Nint => {
                    let a = pop1_f64(&mut stack)?;
                    // C `sCalcPerform.c:716-719`:
                    //   *pd = (double)(long)(d >= 0 ? d+0.5 : d-0.5)
                    let pre = if a >= 0.0 { a + 0.5 } else { a - 0.5 };
                    stack.push(StackValue::Double(c_long(pre) as f64));
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
                    stack.push(StackValue::Double(if result { 1.0 } else { 0.0 }));
                }
                CoreOp::IsInf => {
                    let a = pop1_f64(&mut stack)?;
                    stack.push(StackValue::Double(if a.is_infinite() { 1.0 } else { 0.0 }));
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
                    stack.push(StackValue::Double(if result { 1.0 } else { 0.0 }));
                }
                CoreOp::Atan2 => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    stack.push(StackValue::Double(b.atan2(a)));
                }
                CoreOp::Fmod => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    stack.push(StackValue::Double(a % b));
                }

                // Vararg min/max — type-aware
                CoreOp::Max(nargs) => {
                    let n = *nargs as usize;
                    if stack.len() < n {
                        return Err(CalcError::Underflow);
                    }
                    let first = pop1(&mut stack)?;
                    match first {
                        StackValue::Double(mut result) => {
                            for _ in 1..n {
                                let v = pop1_f64(&mut stack)?;
                                if v > result || result.is_nan() {
                                    result = v;
                                }
                            }
                            stack.push(StackValue::Double(result));
                        }
                        StackValue::Str(mut result) => {
                            for _ in 1..n {
                                let v = pop1(&mut stack)?;
                                let s = v.as_bytes()?;
                                if s > result.as_bytes() {
                                    result = ScalcString::from_c(s);
                                }
                            }
                            stack.push(StackValue::Str(result));
                        }
                    }
                }
                CoreOp::Min(nargs) => {
                    let n = *nargs as usize;
                    if stack.len() < n {
                        return Err(CalcError::Underflow);
                    }
                    let first = pop1(&mut stack)?;
                    match first {
                        StackValue::Double(mut result) => {
                            for _ in 1..n {
                                let v = pop1_f64(&mut stack)?;
                                if v < result || result.is_nan() {
                                    result = v;
                                }
                            }
                            stack.push(StackValue::Double(result));
                        }
                        StackValue::Str(mut result) => {
                            for _ in 1..n {
                                let v = pop1(&mut stack)?;
                                let s = v.as_bytes()?;
                                if s < result.as_bytes() {
                                    result = ScalcString::from_c(s);
                                }
                            }
                            stack.push(StackValue::Str(result));
                        }
                    }
                }

                CoreOp::MaxVal => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    stack.push(StackValue::Double(if a > b { a } else { b }));
                }
                CoreOp::MinVal => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    stack.push(StackValue::Double(if a < b { a } else { b }));
                }

                // Store
                CoreOp::StoreVar(idx) => {
                    let v = pop1_f64(&mut stack)?;
                    inputs.num_vars[*idx as usize] = v;
                }
                CoreOp::StoreDoubleVar(idx) => {
                    // C STORE_AA..STORE_LL (`sCalcPerform.c:888-895`):
                    //
                    //     toString(ps);
                    //     strncpy(psarg[op - STORE_AA], ps->s, SCALC_STRING_SIZE);
                    //
                    // `AA:=` names a STRING field, so the value is coerced, not
                    // dispatched on: a double is converted and stored as text,
                    // and no store of `AA` ever reaches the numeric args.
                    let v = pop1(&mut stack)?;
                    inputs.str_vars[*idx as usize] = v.into_string_value();
                }
            },

            Opcode::String(sop) => match sop {
                StringOp::PushString(s) => {
                    // C LITERAL_STRING (sCalcPerform.c:1493-1502) copies the
                    // literal out of the postfix into the 40-byte element with
                    // `for (i=0; (i<SCALC_STRING_SIZE-1) && *post; )` — so an
                    // over-long literal is truncated at RUN time, not compile
                    // time.
                    stack.push(StackValue::str(s));
                }
                StringOp::PushStringVar(idx) => {
                    stack.push(StackValue::Str(inputs.str_vars[*idx as usize].clone()));
                }
                StringOp::StoreStringVar(idx) => {
                    let v = pop1(&mut stack)?;
                    inputs.str_vars[*idx as usize] = v.into_string_value();
                }
                StringOp::ToString => {
                    // C TO_STRING (sCalcPerform.c:1516-1519) is `toString(ps)`,
                    // no more — the one conversion, not a formatter of its own.
                    let v = pop1(&mut stack)?;
                    stack.push(StackValue::Str(v.into_string_value()));
                }
                StringOp::ToDouble => {
                    // C TO_DOUBLE runs its hunt only `if (isString(ps))`; a
                    // double operand is left exactly as it is.
                    let v = pop1(&mut stack)?;
                    stack.push(StackValue::Double(match &v {
                        StackValue::Double(d) => *d,
                        StackValue::Str(s) => hunt_double(s.as_bytes()),
                    }));
                }
                StringOp::Len => {
                    let v = pop1(&mut stack)?;
                    let len = match &v {
                        StackValue::Str(s) => s.len() as f64,
                        StackValue::Double(_) => 0.0,
                    };
                    stack.push(StackValue::Double(len));
                }
                StringOp::Byte => {
                    let v = pop1(&mut stack)?;
                    let byte_val = match &v {
                        StackValue::Str(s) => {
                            s.as_bytes().first().map(|b| *b as f64).unwrap_or(0.0)
                        }
                        StackValue::Double(_) => 0.0,
                    };
                    stack.push(StackValue::Double(byte_val));
                }
                StringOp::TrEsc => {
                    let v = pop1(&mut stack)?;
                    let s = match v {
                        StackValue::Str(s) => s,
                        StackValue::Double(_) => return Err(CalcError::TypeMismatch),
                    };
                    stack.push(StackValue::str(translate_escapes(s.as_bytes())));
                }
                StringOp::Esc => {
                    let v = pop1(&mut stack)?;
                    let s = match v {
                        StackValue::Str(s) => s,
                        StackValue::Double(_) => return Err(CalcError::TypeMismatch),
                    };
                    stack.push(StackValue::str(escape_string(s.as_bytes())));
                }
                StringOp::Printf => {
                    // Pop format string, then one value
                    let val = pop1(&mut stack)?;
                    let fmt = pop1(&mut stack)?;
                    let result = simple_printf(fmt.as_bytes()?, &val)?;
                    stack.push(StackValue::str(result));
                }
                StringOp::Sscanf => {
                    // Pop format string, then input string
                    let fmt = pop1(&mut stack)?;
                    let input = pop1(&mut stack)?;
                    let result = simple_sscanf(input.as_bytes()?, fmt.as_bytes()?);
                    stack.push(result);
                }
                StringOp::BinRead => {
                    // C `BIN_READ` (sCalcPerform.c:1693): pop the format, then
                    // the subject; both must be strings. The result is a
                    // DOUBLE (`ps->s = NULL`).
                    let fmt = pop1(&mut stack)?;
                    let subject = pop1(&mut stack)?;
                    let value = bin_read(subject.as_bytes()?, fmt.as_bytes()?)?;
                    stack.push(StackValue::Double(value));
                }
                StringOp::BinWrite => {
                    // C `BIN_WRITE` (sCalcPerform.c:1569): pop the value, then
                    // the format; only the format must be a string. The result
                    // is the raw bytes, escaped back into a string.
                    let val = pop1(&mut stack)?;
                    let fmt = pop1(&mut stack)?;
                    let result = bin_write(fmt.as_bytes()?, &val)?;
                    stack.push(StackValue::str(result));
                }
                StringOp::Crc16 => {
                    let v = pop1(&mut stack)?;
                    let crc = super::checksum::crc16(v.as_bytes()?);
                    stack.push(StackValue::Double(crc as f64));
                }
                StringOp::Crc16Append => {
                    // MODBUS: append CRC16 as two bytes (little-endian)
                    let v = pop1(&mut stack)?;
                    let s = v.as_bytes()?;
                    let crc = super::checksum::crc16(s);
                    let mut result = s.to_vec();
                    result.push((crc & 0xFF) as u8);
                    result.push(((crc >> 8) & 0xFF) as u8);
                    stack.push(StackValue::str(result));
                }
                StringOp::Lrc => {
                    let v = pop1(&mut stack)?;
                    match super::checksum::lrc(v.as_bytes()?) {
                        Some(lrc_str) => {
                            stack.push(StackValue::str(lrc_str));
                        }
                        None => return Err(CalcError::InvalidFormat),
                    }
                }
                StringOp::LrcAppend => {
                    // AMODBUS: append LRC hex string
                    let v = pop1(&mut stack)?;
                    let s = v.as_bytes()?;
                    match super::checksum::lrc(s) {
                        Some(lrc_str) => {
                            let mut result = s.to_vec();
                            result.extend_from_slice(lrc_str.as_bytes());
                            stack.push(StackValue::str(result));
                        }
                        None => return Err(CalcError::InvalidFormat),
                    }
                }
                StringOp::Xor8 => {
                    let v = pop1(&mut stack)?;
                    let xor = super::checksum::xor8(v.as_bytes()?);
                    stack.push(StackValue::Double(xor as f64));
                }
                StringOp::Xor8Append => {
                    // ADD_XOR8: append XOR8 as one byte
                    let v = pop1(&mut stack)?;
                    let s = v.as_bytes()?;
                    let xor = super::checksum::xor8(s);
                    let mut result = s.to_vec();
                    result.push(xor);
                    stack.push(StackValue::str(result));
                }
                StringOp::Subrange => {
                    // C `sCalcPerform.c:1869-1901`. Pop: string, i, j — and BOTH
                    // bounds are inclusive:
                    //
                    // ```c
                    // for (s1=s+i, s2=s+j ; *s1 && s1 <= s2; ) *s++ = *s1++;
                    // ```
                    //
                    // so `"hello"[1,4]` is "ello" and `"hello"[2,2]` is "l". The
                    // bound arithmetic itself is `subrange_bounds`, shared with
                    // aCalc's `[`.
                    let end_val = pop1(&mut stack)?;
                    let start_val = pop1(&mut stack)?;
                    let s = pop1(&mut stack)?;
                    let s = s.as_bytes()?;
                    let k = s.len() as i64;
                    let (i, j) = super::subrange_bounds(
                        subrange_index(&start_val)?,
                        subrange_index(&end_val)?,
                        k,
                    );
                    let out = if j < i {
                        &[][..]
                    } else {
                        &s[i as usize..(j + 1).min(k) as usize]
                    };
                    stack.push(StackValue::str(out));
                }
                StringOp::Replace => {
                    // Pop: string, find, replace
                    let replace_val = pop1(&mut stack)?;
                    let find_val = pop1(&mut stack)?;
                    let s = pop1(&mut stack)?;
                    let s = s.as_bytes()?;
                    let find = find_val.as_bytes()?;
                    let replace = replace_val.as_bytes()?;
                    // Replace first occurrence only
                    let mut result = s.to_vec();
                    if let Some(pos) = find_sub(s, find) {
                        result.splice(pos..pos + find.len(), replace.iter().copied());
                    }
                    stack.push(StackValue::str(result));
                }
                StringOp::SubLast => {
                    // Remove last occurrence of substring
                    let pattern = pop1(&mut stack)?;
                    let s = pop1(&mut stack)?;
                    let s = s.as_bytes()?;
                    let pat = pattern.as_bytes()?;
                    let mut result = s.to_vec();
                    if let Some(pos) = rfind_sub(s, pat) {
                        result.drain(pos..pos + pat.len());
                    }
                    stack.push(StackValue::str(result));
                }
            },

            Opcode::Control(ctrl) => match ctrl {
                super::opcodes::ControlOp::Until(_end_pc) => {
                    // UNTIL is just a loop start marker - no-op during execution.
                    // The actual loop jump happens at UntilEnd.
                }
                super::opcodes::ControlOp::UntilEnd(start_pc) => {
                    // Pop condition from stack: if false (0), jump back to loop start
                    let cond = pop1_f64(&mut stack)?;
                    if cond == 0.0 {
                        pc = *start_pc + 1; // jump to instruction after UNTIL marker
                        loop_count += 1;
                        if loop_count > MAX_LOOP_ITERATIONS {
                            return Err(CalcError::LoopLimitExceeded);
                        }
                    }
                    // else: condition true, continue past loop
                }
            },

            #[allow(unreachable_patterns)]
            _ => return Err(CalcError::Internal),
        }
    }

    let result = stack.last().cloned().unwrap_or(StackValue::Double(0.0));
    // Both of C's evaluator paths end with the same line — `sCalcPerform.c:833`
    // (no-string) and `:2056` (string):
    //     return(((isnan(*presult)||isinf(*presult)) ? -1 : 0));
    // `*presult` is the DOUBLE form of the result: a string result is first
    // run through `to_double` (:2046-2050). So an expression whose operators
    // all succeeded still fails the perform when its value is not finite
    // (`LOG(0)` = -inf, `1e300*1e300` = +inf, `ACOS(2)` = NaN) — the record
    // then forces VAL=-1 / SVAL="***ERROR***" / CALC_ALARM.
    if !result.to_double().is_finite() {
        return Err(CalcError::NonFiniteResult);
    }
    Ok(result)
}

/// C `TO_DOUBLE` — the `DBL` OPERATOR (`sCalcPerform.c:1505-1514), which is not
/// the `toDouble` coercion ([`StackValue::to_double`]) and does not parse:
///
/// ```c
/// s = strpbrk(ps->s, "0123456789");        /* the first DIGIT, anywhere */
/// if ((s > ps->s) && (s[-1] == '.')) s--;  /* take a '.' before it */
/// if ((s > ps->s) && (s[-1] == '-')) s--;  /* and a '-' before that */
/// ps->d = s ? atof(s) : 0.0;
/// ```
///
/// It HUNTS a number out of surrounding text, so it reads `-12.5` out of
/// `"v=-12.5V"` where the coercion would give 0. The asymmetries are C's and
/// compiled sCalc confirms each: a `+` sign is never taken (`"x+3"` is 3), only
/// ONE `-` is (`"--5"` is -5), and a string with no digit at all is 0 even when
/// `atof` would have read it (`DBL("inf")` is 0, `atof("inf")` is inf).
fn hunt_double(s: &[u8]) -> f64 {
    let Some(mut i) = s.iter().position(|c| c.is_ascii_digit()) else {
        return 0.0;
    };
    if i > 0 && s[i - 1] == b'.' {
        i -= 1;
    }
    if i > 0 && s[i - 1] == b'-' {
        i -= 1;
    }
    super::strtod::strtod(&s[i..]).0
}

/// C `SMALL` (`sCalcPerform.c:46`) — the tolerance sCalc's numeric comparisons
/// are written around. It is sCalc's alone: base and aCalc compare exactly.
const SMALL: f64 = 1e-11;

const MAX_LOOP_ITERATIONS: usize = 1000;

fn translate_escapes(bytes: &[u8]) -> Vec<u8> {
    let mut result = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'n' => {
                    result.push(b'\n');
                    i += 2;
                }
                b't' => {
                    result.push(b'\t');
                    i += 2;
                }
                b'r' => {
                    result.push(b'\r');
                    i += 2;
                }
                b'\\' => {
                    result.push(b'\\');
                    i += 2;
                }
                b'x' if i + 3 < bytes.len() => {
                    if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 2]), hex_val(bytes[i + 3])) {
                        result.push((hi << 4) | lo);
                        i += 4;
                    } else {
                        result.push(b'\\');
                        i += 1;
                    }
                }
                _ => {
                    result.push(b'\\');
                    i += 1;
                }
            }
        } else {
            result.push(bytes[i]);
            i += 1;
        }
    }
    result
}

fn escape_string(bytes: &[u8]) -> Vec<u8> {
    let mut result = Vec::new();
    for &b in bytes {
        match b {
            b'\n' => result.extend_from_slice(b"\\n"),
            b'\t' => result.extend_from_slice(b"\\t"),
            b'\r' => result.extend_from_slice(b"\\r"),
            b'\\' => result.extend_from_slice(b"\\\\"),
            0x00..=0x1f | 0x7f..=0xff => {
                result.extend_from_slice(format!("\\x{b:02x}").as_bytes());
            }
            _ => result.push(b),
        }
    }
    result
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn simple_printf(bytes: &[u8], val: &StackValue) -> Result<Vec<u8>, CalcError> {
    // Find first format specifier
    let mut i = 0;
    let mut result: Vec<u8> = Vec::new();

    while i < bytes.len() {
        if bytes[i] == b'%' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'%' {
                result.push(b'%');
                i += 2;
                continue;
            }
            // Parse format specifier: %[flags][width][.precision]type
            let spec_start = i;
            i += 1; // skip %
            // Skip flags
            while i < bytes.len() && matches!(bytes[i], b'-' | b'+' | b' ' | b'0' | b'#') {
                i += 1;
            }
            // Skip width
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            // Skip precision
            if i < bytes.len() && bytes[i] == b'.' {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if i >= bytes.len() {
                return Err(CalcError::InvalidFormat);
            }
            let spec = bytes[i];
            i += 1;
            let fmt_str =
                std::str::from_utf8(&bytes[spec_start..i]).map_err(|_| CalcError::InvalidFormat)?;
            match spec {
                b'd' | b'i' => {
                    let v = val.to_double() as i64;
                    result.extend_from_slice(c_format_int(fmt_str, v).as_bytes());
                }
                b'f' | b'e' | b'g' | b'E' | b'G' => {
                    let v = val.to_double();
                    result.extend_from_slice(c_format_float(fmt_str, v).as_bytes());
                }
                b'x' | b'X' | b'o' => {
                    let v = val.to_double() as i64;
                    result.extend_from_slice(c_format_int(fmt_str, v).as_bytes());
                }
                b's' => match val {
                    StackValue::Str(s) => result.extend_from_slice(s.as_bytes()),
                    // C PRINTF (sCalcPerform.c:1553) `toString(ps1)` before the
                    // snprintf, so `%s` of a double is cvtDoubleToString, not a
                    // shortest-round-trip rendering.
                    StackValue::Double(d) => {
                        result.extend_from_slice(cvt::to_string(*d).as_bytes());
                    }
                },
                _ => return Err(CalcError::InvalidFormat),
            }
            // Append rest of format string literally
            result.extend_from_slice(&bytes[i..]);
            return Ok(result);
        } else {
            result.push(bytes[i]);
            i += 1;
        }
    }
    // No format specifier found, return format string as-is
    Ok(result)
}

fn c_format_int(fmt: &str, val: i64) -> String {
    // Parse width and type from format string
    let bytes = fmt.as_bytes();
    let spec = bytes[bytes.len() - 1];
    // Extract flags, width
    let inner = &fmt[1..fmt.len() - 1]; // between % and type
    let width: usize = inner
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .unwrap_or(0);
    let left_align = inner.contains('-');
    let zero_pad = inner.starts_with('0') && !left_align;

    let formatted = match spec {
        b'd' | b'i' => format!("{}", val),
        b'x' => format!("{:x}", val as u64),
        b'X' => format!("{:X}", val as u64),
        b'o' => format!("{:o}", val as u64),
        _ => format!("{}", val),
    };

    if width > formatted.len() {
        let pad = width - formatted.len();
        if left_align {
            format!("{}{}", formatted, " ".repeat(pad))
        } else if zero_pad {
            format!("{}{}", "0".repeat(pad), formatted)
        } else {
            format!("{}{}", " ".repeat(pad), formatted)
        }
    } else {
        formatted
    }
}

fn c_format_float(fmt: &str, val: f64) -> String {
    let bytes = fmt.as_bytes();
    let spec = bytes[bytes.len() - 1];
    let inner = &fmt[1..fmt.len() - 1];

    // Parse precision
    let precision = if let Some(dot_pos) = inner.find('.') {
        inner[dot_pos + 1..].parse::<usize>().unwrap_or(6)
    } else {
        6
    };

    match spec {
        b'f' => format!("{:.prec$}", val, prec = precision),
        b'e' => format!("{:.prec$e}", val, prec = precision),
        b'E' => format!("{:.prec$E}", val, prec = precision),
        b'g' | b'G' => {
            // Use shorter of %f and %e
            let f_str = format!("{:.prec$}", val, prec = precision);
            let e_str = format!("{:.prec$e}", val, prec = precision);
            if e_str.len() < f_str.len() {
                e_str
            } else {
                f_str
            }
        }
        _ => format!("{}", val),
    }
}

fn simple_sscanf(input: &[u8], fmt: &[u8]) -> StackValue {
    let bytes = fmt;
    let mut i = 0;
    // Find format specifier
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 1 < bytes.len() && bytes[i + 1] != b'%' {
            i += 1;
            // Skip width
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i >= bytes.len() {
                return StackValue::Double(0.0);
            }
            let spec = bytes[i];
            let trimmed = trim_ascii(input);
            return match spec {
                b'd' | b'i' => {
                    let digits = trimmed
                        .iter()
                        .take_while(|b| b.is_ascii_digit() || **b == b'-')
                        .count();
                    let text = std::str::from_utf8(&trimmed[..digits]).unwrap_or("");
                    StackValue::Double(text.parse::<i64>().unwrap_or(0) as f64)
                }
                // C's `%e`/`%f`/`%g` conversion is strtod on the input, so it
                // takes the longest numeric PREFIX and leaves the rest —
                // `SSCANF("1.5V", "%f")` is 1.5, not a failed conversion.
                b'f' | b'e' | b'g' => StackValue::Double(super::strtod::strtod(trimmed).0),
                b's' => {
                    // Read until whitespace
                    let word: Vec<u8> = trimmed
                        .iter()
                        .copied()
                        .take_while(|b| !b.is_ascii_whitespace())
                        .collect();
                    StackValue::str(word)
                }
                _ => StackValue::Double(0.0),
            };
        }
        i += 1;
    }
    StackValue::Double(0.0)
}

/// C `isspace`-trimmed both ends, the way `sscanf`'s numeric conversions skip
/// leading whitespace.
fn trim_ascii(s: &[u8]) -> &[u8] {
    let start = s
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(s.len());
    let end = s
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |p| p + 1);
    &s[start..end]
}

/// C `myNINT` (sCalcPerform.c:40) — round half away from zero.
fn my_nint(d: f64) -> f64 {
    if d >= 0.0 { d + 0.5 } else { d - 0.5 }.trunc()
}

/// The binary field a printf/scanf conversion character names, with `h`/`l`
/// applied. This is the one place that reads C's `s[-1]` length modifier, so
/// BIN_READ and BIN_WRITE cannot disagree about a width.
///
/// `Int(4)` reads back UNSIGNED, and that is C, not a slip. C reads a 4-byte
/// `%d` with `memcpy(&l, s1, 4)` into `long l = 0L` (sCalcPerform.c:321,1764) —
/// on LP64 that is a 4-byte store into a zero-initialised 8-byte object, so the
/// value is zero-extended. Compiled C agrees: `READ("\xff\xff\xff\xff", "%d")`
/// is 4294967295, the same answer `%x` gives. Only `%hd` sign-extends, because
/// `short h` really is two bytes wide, and `%c` because `char c` really is one.
#[derive(Clone, Copy, PartialEq)]
enum BinField {
    Int(usize),   // 'd','i' — 2 bytes with `h`, else 4
    Uint(usize),  // 'o','u','x','X' — 2 bytes with `h`, else 4
    Float(usize), // 'e','E','f','g','G' — 8 bytes with `l`, else 4
    Char,         // 'c' — 1 byte
}

impl BinField {
    /// `conv` is the conversion character and `prev` the character before it
    /// (C's `s[-1]`), which is where the `h`/`l` modifier lives.
    fn parse(conv: u8, prev: Option<u8>) -> Option<BinField> {
        Some(match conv {
            b'd' | b'i' => BinField::Int(if prev == Some(b'h') { 2 } else { 4 }),
            b'o' | b'u' | b'x' | b'X' => BinField::Uint(if prev == Some(b'h') { 2 } else { 4 }),
            b'e' | b'E' | b'f' | b'g' | b'G' => {
                BinField::Float(if prev == Some(b'l') { 8 } else { 4 })
            }
            b'c' => BinField::Char,
            // C's `default:` and its explicit `case 's'` both `return(-1)`.
            _ => return None,
        })
    }

    fn width(self) -> usize {
        match self {
            BinField::Int(w) | BinField::Uint(w) | BinField::Float(w) => w,
            BinField::Char => 1,
        }
    }
}

/// C `BIN_WRITE` (sCalcPerform.c:1569-1633): write `val` into `fmt`'s field as
/// raw little-endian bytes, then escape those bytes back into a string.
///
/// C finds the conversion character with its own inline scan — skip every `%%`,
/// take the next `%`, then the first character of `*cdeEfgGiousxX` after it —
/// and bails out (`return -1`) on `*` (suppressed assignment), on `s`, and when
/// there is no conversion character at all.
fn bin_write(f: &[u8], val: &StackValue) -> Result<String, CalcError> {
    // `while ((s1 = strstr(s, "%%"))) {s = s1+2;}` — advance past the LAST `%%`.
    let mut i = 0;
    while let Some(p) = find_sub(&f[i..], b"%%") {
        i += p + 2;
    }
    let pct = find_byte(&f[i..], b'%').ok_or(CalcError::InvalidFormat)? + i;
    let conv = f[pct + 1..]
        .iter()
        .position(|b| b"*cdeEfgGiousxX".contains(b))
        .ok_or(CalcError::InvalidFormat)?
        + pct
        + 1;

    let field = BinField::parse(f[conv], f.get(conv.wrapping_sub(1)).copied())
        .ok_or(CalcError::InvalidFormat)?;

    // C `toDouble(ps1)`: the value operand is coerced, never rejected. C's
    // `myNINT` casts to `int`, so the integer conversions see a 32-bit value
    // and `memcpy` then takes its low `width` bytes.
    let d = val.to_double();
    let n = my_nint(d) as i32;
    let raw: Vec<u8> = match field {
        BinField::Char => vec![n as u8],
        BinField::Int(w) | BinField::Uint(w) => n.to_le_bytes()[..w].to_vec(),
        BinField::Float(4) => (d as f32).to_le_bytes().to_vec(),
        BinField::Float(_) => d.to_le_bytes().to_vec(),
    };
    Ok(escaped_from_raw(&raw))
}

/// C `BIN_READ` (sCalcPerform.c:1693-1794): un-escape `subject` into raw bytes
/// and read `fmt`'s field out of them as a double.
///
/// Unlike BIN_WRITE this uses `findConversionIndicator`, which skips
/// assignment-suppressed conversions (`%*...`); the suppressed ones are then
/// re-read as a byte count to skip over before the value is taken.
fn bin_read(subject: &[u8], f: &[u8]) -> Result<f64, CalcError> {
    let conv = find_conversion_indicator(f).ok_or(CalcError::InvalidFormat)?;
    let field = BinField::parse(f[conv], f.get(conv.wrapping_sub(1)).copied())
        .ok_or(CalcError::InvalidFormat)?;

    let raw = raw_from_escaped(subject);
    let skip = match find_byte(f, b'*') {
        // `s2 && s2 < s`: a suppressed conversion ahead of the live one.
        Some(star) if star < conv => suppressed_skip_bytes(&f[star + 1..]),
        _ => 0,
    };

    let w = field.width();
    let bytes = raw.get(skip..skip + w).ok_or(CalcError::InvalidFormat)?;
    Ok(match field {
        // `char c` / `short h`: exactly as wide as the field, so these sign-extend.
        BinField::Char => bytes[0] as i8 as f64,
        BinField::Int(2) => i16::from_le_bytes([bytes[0], bytes[1]]) as f64,
        // `long l = 0L` with a 4-byte memcpy: zero-extended. See BinField.
        BinField::Int(_) | BinField::Uint(4) => {
            u32::from_le_bytes(bytes.try_into().unwrap()) as f64
        }
        BinField::Uint(_) => u16::from_le_bytes([bytes[0], bytes[1]]) as f64,
        BinField::Float(4) => f32::from_le_bytes(bytes.try_into().unwrap()) as f64,
        BinField::Float(_) => f64::from_le_bytes(bytes.try_into().unwrap()),
    })
}

/// How many bytes a suppressed conversion (`%*2hd`, `%*2c`, `%*2`) covers.
/// `tail` starts just after the `*`. C reads an optional repeat count and then
/// scales it by the width the following conversion names (sCalcPerform.c:1717).
fn suppressed_skip_bytes(tail: &[u8]) -> usize {
    let digits = tail.iter().take_while(|b| b.is_ascii_digit()).count();
    let count: usize = std::str::from_utf8(&tail[..digits])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let rest = &tail[digits..];
    // C switches on the character right after the digits, so `%*2` with no
    // conversion at all skips `count` bare bytes.
    match rest.first() {
        Some(b'h') => count * 2,
        Some(b'l') => {
            if rest.iter().any(|b| b"diouxX".contains(b)) {
                count * 4
            } else {
                count * 8
            }
        }
        Some(b'd' | b'i' | b'o' | b'u' | b'x' | b'X') => count * 4,
        Some(b'e' | b'E' | b'f' | b'g' | b'G') => count * 4,
        _ => count,
    }
}

/// C `findConversionIndicator` (sCalcPerform.c:105): the byte offset of the
/// first conversion character whose assignment is NOT suppressed, skipping
/// `%%` pairs. Returns `None` when there is none.
fn find_conversion_indicator(f: &[u8]) -> Option<usize> {
    const CONV: &[u8] = b"pwn$c[deEfgGiousxX";
    let mut i = 0;
    while i < f.len() {
        if let Some(p) = find_sub(&f[i..], b"%%") {
            if find_byte(&f[i..], b'%') == Some(p) {
                i += p + 2;
                continue;
            }
        }
        let pct = find_byte(&f[i..], b'%')? + i;
        let cc = f[pct..].iter().position(|b| CONV.contains(b))? + pct;
        match find_byte(&f[pct..], b'*') {
            // Suppressed: skip past this conversion and keep looking.
            Some(star) if star + pct < cc => i = cc + 1,
            _ => return Some(cc),
        }
    }
    None
}

fn find_byte(h: &[u8], n: u8) -> Option<usize> {
    h.iter().position(|b| *b == n)
}

/// C `strstr`: the empty needle matches at the start of the haystack.
fn find_sub(h: &[u8], n: &[u8]) -> Option<usize> {
    if n.is_empty() {
        return Some(0);
    }
    if n.len() > h.len() {
        return None;
    }
    h.windows(n.len()).position(|w| w == n)
}

/// The LAST occurrence — C's `SUBLAST` scan (`sCalcPerform.c:996-1003`).
fn rfind_sub(h: &[u8], n: &[u8]) -> Option<usize> {
    if n.is_empty() {
        return Some(h.len());
    }
    if n.len() > h.len() {
        return None;
    }
    h.windows(n.len()).rposition(|w| w == n)
}

/// C `epicsStrnEscapedFromRaw` (epicsString.c:120), which is what
/// `epicsStrSnPrintEscaped` resolves to. Raw bytes in, printable string out:
/// the C escapes, `\0` for NUL, `\xNN` (lower-case) for anything else
/// unprintable, and the byte itself when `isprint`.
fn escaped_from_raw(src: &[u8]) -> String {
    let mut out = String::new();
    for &c in src {
        match c {
            0x07 => out.push_str("\\a"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x0b => out.push_str("\\v"),
            b'\\' => out.push_str("\\\\"),
            b'\'' => out.push_str("\\'"),
            b'"' => out.push_str("\\\""),
            0 => out.push_str("\\0"),
            // C `isprint` in the C locale: the printable ASCII range.
            0x20..=0x7e => out.push(c as char),
            _ => out.push_str(&format!("\\x{c:02x}")),
        }
    }
    out
}

/// C `dbTranslateEscape` -> `epicsStrnRawFromEscaped` (epicsString.c:49).
/// Escaped string in, raw bytes out. An unknown escape yields the character
/// itself, and a `\x` with no hex digit behind it yields a literal `x`.
fn raw_from_escaped(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        if s[i] != b'\\' {
            out.push(s[i]);
            i += 1;
            continue;
        }
        i += 1;
        let Some(&c) = s.get(i) else { break };
        i += 1;
        match c {
            b'a' => out.push(0x07),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0b),
            b'\\' => out.push(b'\\'),
            b'\'' => out.push(b'\''),
            b'"' => out.push(b'"'),
            b'0' => out.push(0),
            b'x' => {
                let digits = s[i..]
                    .iter()
                    .take_while(|b| b.is_ascii_hexdigit())
                    .count()
                    .min(2);
                if digits == 0 {
                    // C falls back through `goto input`: the `x` is literal.
                    out.push(b'x');
                } else {
                    let hex = std::str::from_utf8(&s[i..i + digits]).unwrap();
                    out.push(u8::from_str_radix(hex, 16).unwrap());
                    i += digits;
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Whether `sCalcPostfix` would have stamped this program `USES_STRING`
/// (`sCalcPostfix.c:447-475`), which is what makes `sCalcPerform` run its
/// string evaluator (`:2057`) instead of the plain double one (`:399`).
///
/// The two evaluators differ arithmetically in exactly one place — MODULO's
/// cast width, `(int)` vs `(long)` — so the marker is not cosmetic.
///
/// C's list is an opcode allowlist, and it is narrower than "mentions a
/// string": `TO_DOUBLE` (`DBL`), `BYTE`, `SUBLAST` (`|-`) and the string-var
/// STORES (`A_SSTORE`, i.e. `AA:=`) are all absent from it, so an expression
/// using only those keeps the no-string evaluator. That asymmetry is C's, and
/// this list mirrors it case for case.
fn uses_string(code: &[Opcode]) -> bool {
    code.iter().any(|op| match op {
        // FETCH_SVAL — the only Core opcode in C's list.
        Opcode::Core(CoreOp::FetchSval) => true,
        Opcode::String(s) => match s {
            StringOp::PushStringVar(_)   // FETCH_AA..FETCH_LL
            | StringOp::ToString         // TO_STRING
            | StringOp::Printf           // PRINTF
            | StringOp::BinWrite         // BIN_WRITE
            | StringOp::Sscanf           // SSCANF
            | StringOp::BinRead          // BIN_READ
            | StringOp::PushString(_)    // LITERAL_STRING
            | StringOp::Subrange         // SUBRANGE
            | StringOp::Replace          // REPLACE
            | StringOp::TrEsc            // TR_ESC
            | StringOp::Esc              // ESC
            | StringOp::Crc16            // CRC16
            | StringOp::Crc16Append      // MODBUS
            | StringOp::Lrc              // LRC
            | StringOp::LrcAppend        // AMODBUS
            | StringOp::Xor8             // XOR8
            | StringOp::Xor8Append       // ADD_XOR8
            | StringOp::Len => true,     // LEN
            // Absent from C's list, deliberately:
            StringOp::ToDouble | StringOp::Byte | StringOp::SubLast
            | StringOp::StoreStringVar(_) => false,
        },
        _ => false,
    })
}

fn pop1(stack: &mut Vec<StackValue>) -> Result<StackValue, CalcError> {
    stack.pop().ok_or(CalcError::Underflow)
}

/// C's mixed-type rule for the binary operators that HAVE a string branch —
/// ADD, SUB/SUBLAST, and the six comparisons (sCalcPerform.c:964-978 and the
/// comparison cases). Each is written the same way:
///
/// ```c
/// if (isDouble(ps))       { toDouble(ps1);  /* numeric */ }
/// else if (isDouble(ps1)) { to_double(ps);  /* numeric */ }
/// else                    { /* the string branch */ }
/// ```
///
/// so the string branch runs ONLY when both operands are strings; if either
/// side is already a double, the other is coerced and the operator is numeric.
/// Classifying the pair once means these operators cannot grow a third,
/// mixed-type outcome — there is no `_` arm left to reject.
enum Pair {
    Numeric(f64, f64),
    Strings(ScalcString, ScalcString),
}

impl Pair {
    fn of(a: StackValue, b: StackValue) -> Pair {
        match (a, b) {
            (StackValue::Str(x), StackValue::Str(y)) => Pair::Strings(x, y),
            (a, b) => Pair::Numeric(a.to_double(), b.to_double()),
        }
    }
}

/// A `SUBRANGE` bound. This is NOT one of the numeric positions C coerces: C
/// branches on the bound's TYPE (sCalcPerform.c:1876-1888) — a double is the
/// index itself, while a STRING is searched for with `strstr` and positions the
/// range at the match. The port implements only the numeric branch, so a string
/// bound is still rejected here; running it through `to_double` instead would
/// silently answer 0 for "abc" rather than looking for it. The missing branch is
/// an open gap, reported, not papered over.
fn subrange_index(v: &StackValue) -> Result<i64, CalcError> {
    match v {
        StackValue::Double(d) => Ok(*d as i64),
        StackValue::Str(_) => Err(CalcError::TypeMismatch),
    }
}

/// Pop a NUMERIC operand. C reaches every one of these through `toDouble`
/// (sCalcPerform.c: MULT, DIV, POWER, MODULO, the trig/log/abs/sqrt functions,
/// COND_IF, REL_AND/OR/NOT, the bit ops, ...), which coerces a string instead
/// of rejecting it — so this cannot fail on type, only on underflow.
fn pop1_f64(stack: &mut Vec<StackValue>) -> Result<f64, CalcError> {
    let v = stack.pop().ok_or(CalcError::Underflow)?;
    Ok(v.to_double())
}

fn pop2_f64(stack: &mut Vec<StackValue>) -> Result<(f64, f64), CalcError> {
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

#[cfg(test)]
mod parity_tests {
    //! C-parity regression tests for the string evaluator
    //! (doc/parity-review/01-calc.md).
    use crate::calc::{StackValue, StringInputs, scalc};

    fn run_num(expr: &str) -> f64 {
        let mut inp = StringInputs::new();
        match scalc(expr, &mut inp).unwrap() {
            StackValue::Double(v) => v,
            StackValue::Str(s) => panic!("expected double, got string {s:?}"),
        }
    }

    // R9-1: these two cases pinned EXACT comparison on the string engine, on
    // the belief (taken from base's `calcPerform.c`) that no calc engine has an
    // epsilon. sCalc has one — `SMALL` = 1e-11 (`sCalcPerform.c:46`), used by
    // all six numeric comparisons (:595-620, :1161-1255) — so the compiled C
    // evaluator answers the opposite of what was pinned here. The values below
    // are its output.
    #[test]
    fn h6_eq_is_within_small() {
        assert_eq!(run_num("1e-12 == 0"), 1.0); // C: d=1 (|1e-12| < SMALL)
        assert_eq!(run_num("1e-12 # 0"), 0.0); // C: d=0. `#` is the NE operator
        assert_eq!(run_num("0 == 0"), 1.0); // C: d=1
        assert_eq!(run_num("0.1+0.2 == 0.3"), 1.0); // C: d=1 — SMALL absorbs the ULP
    }

    #[test]
    fn h6_inequalities_are_within_small() {
        // C: d=0 — the difference (9e-12) does not exceed SMALL, so `<` is false
        // even though 1e-12 is genuinely below 1e-11.
        assert_eq!(run_num("1e-12 < 1e-11"), 0.0);
        assert_eq!(run_num("1 >= 1"), 1.0); // C: d=1
        assert_eq!(run_num("2 > 1"), 1.0); // C: d=1
    }

    // R8-7: this case used to pin base's IEEE divide (`1/0` = +Inf) onto the
    // STRING engine, which was never sCalc's rule — it was read off
    // `calcPerform.c` and never checked against `sCalcPerform.c`. The compiled
    // C evaluator answers `st=-1` for all three (`sCalcPerform.c:495-500`,
    // :1022-1030), i.e. a failed perform, so the expectations below are the
    // C-verified ones.
    #[test]
    fn h7_div_by_zero_fails_the_perform() {
        let mut inp = StringInputs::new();
        assert!(scalc("1/0", &mut inp).is_err()); // C: PERFORM st=-1
        assert!(scalc("-1/0", &mut inp).is_err()); // C: PERFORM st=-1
        assert!(scalc("0/0", &mut inp).is_err()); // C: PERFORM st=-1
    }
}
