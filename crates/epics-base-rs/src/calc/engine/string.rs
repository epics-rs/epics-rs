use super::cast::{c_int, c_long, d2ui};
use super::cvt;
use super::error::CalcError;
use super::opcodes::{CoreOp, Opcode, StringOp};
use super::value::{SCALC_STRING_SIZE, ScalcString, StackValue};
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
                    stack.push(StackValue::Double(super::c_isinf(a)));
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
                    // C BYTE (sCalcPerform.c:1528-1533):
                    //
                    //     if (isString(ps)) { ps->d = ps->s[0]; ps->s = NULL; }
                    //
                    // `ps->s[0]` is a `char`, which is SIGNED on the reference
                    // platform, so a byte with the high bit set reads NEGATIVE:
                    // compiled C gives BYTE("\xff") = -1 and BYTE("\x80") = -128,
                    // not 255 and 128. (The same signed read is already in
                    // BIN_READ's `%c`.) The empty string reads its NUL: 0. And a
                    // DOUBLE operand falls through the `isString` guard
                    // untouched — it is not an error and not zero.
                    let v = pop1(&mut stack)?;
                    stack.push(StackValue::Double(match &v {
                        StackValue::Double(d) => *d,
                        StackValue::Str(s) => s.as_bytes().first().map_or(0.0, |b| *b as i8 as f64),
                    }));
                }
                StringOp::TrEsc => {
                    // C TR_ESC (sCalcPerform.c:1798-1802):
                    //
                    //     if (isString(ps)) {
                    //         i = dbTranslateEscape(tmpstr, ps->s);
                    //         strNcpy(ps->s, tmpstr, SCALC_STRING_SIZE-1);
                    //     }
                    //
                    // so the table is epicsString.c's, a double operand is left
                    // alone, and a `\0` escape produces a NUL byte that the
                    // strNcpy then reads as the end of the value — which
                    // `StackValue::str` (the same strNcpy) reproduces.
                    let v = pop1(&mut stack)?;
                    stack.push(match v {
                        StackValue::Double(d) => StackValue::Double(d),
                        StackValue::Str(s) => StackValue::str(raw_from_escaped(s.as_bytes())),
                    });
                }
                StringOp::Esc => {
                    // C ESC (sCalcPerform.c:1805-1815) — the same table run
                    // backwards, and a double is again left alone. Its result is
                    // bounded at THIRTY-EIGHT bytes, not 39: C passes
                    // `SCALC_STRING_SIZE-1` as epicsStrSnPrintEscaped's dstlen,
                    // and that function writes at most `dstlen-1` bytes before
                    // its NUL (epicsString.c:133 `if (--rem > 0) *dst++ = chr`).
                    // Compiled C: 20 newlines escape to 40 bytes and come back 38.
                    let v = pop1(&mut stack)?;
                    stack.push(match v {
                        StackValue::Double(d) => StackValue::Double(d),
                        StackValue::Str(s) => {
                            let mut esc = escaped_from_raw(s.as_bytes()).into_bytes();
                            esc.truncate(SCALC_STRING_SIZE - 2);
                            StackValue::str(esc)
                        }
                    });
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
                    // C `sCalcPerform.c:1869-1901`. Pop: string, i, j. BOTH bounds
                    // are inclusive:
                    //
                    // ```c
                    // for (s1=s+i, s2=s+j ; *s1 && s1 <= s2; ) *s++ = *s1++;
                    // ```
                    //
                    // so `"hello"[1,4]` is "ello" and `"hello"[2,2]` is "l". The
                    // subject is `toString(ps)`, not a string-only operand.
                    let end_val = pop1(&mut stack)?;
                    let start_val = pop1(&mut stack)?;
                    let subject = pop1(&mut stack)?.into_string_value();
                    let s = subject.as_bytes();
                    let k = s.len() as i64;
                    let (i, j) = subrange_bounds(s, &start_val, &end_val);
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

/// C PRINTF (`sCalcPerform.c:1535-1567`).
///
/// ```c
/// s = ps->s;
/// while ((s1 = strstr(s, "%%"))) {s = s1+2;}          /* PAST the LAST "%%" */
/// if (((s = strpbrk(s, "%")) == NULL) ||
///     ((s = strpbrk(s+1, "*cdeEfgGiousxX")) == NULL)) {
///     strcpy(tmpstr, ps->s);                          /* the RAW format */
/// } else {
///     switch (*s) {
///     default: case '*':  return(-1);
///     case 'c','d','i','o','u','x','X': toDouble(ps1); l = myNINT(ps1->d);
///                                       snprintf(tmpstr, N, ps->s, l);    break;
///     case 'e','E','f','g','G':         toDouble(ps1);
///                                       snprintf(tmpstr, N, ps->s, ps1->d); break;
///     case 's':                         toString(ps1);
///                                       snprintf(tmpstr, N, ps->s, ps1->s); break;
///     }
/// }
/// ```
///
/// Two things the port got wrong follow from that scan, and compiled C confirms
/// both:
///
///   * When the scan finds no conversion, C copies the format RAW — `%%` and
///     all. `PRINTF("100%%", x)` is `100%%`, not `100%`; and because the scan
///     starts AFTER the last `%%`, a conversion that sits BEFORE one is not a
///     conversion at all: `PRINTF("%.2f%%", 3.14159)` is the literal `%.2f%%`.
///   * When it does find one, C hands the WHOLE format to `snprintf` with that
///     single argument — so every flag, width and precision applies, and the
///     `%%` earlier in the format collapse: `PRINTF("a%%b %5.2f!", 3.14159)` is
///     `a%b  3.14!`.
fn simple_printf(fmt: &[u8], val: &StackValue) -> Result<Vec<u8>, CalcError> {
    match conversion_index(fmt) {
        // `strcpy(tmpstr, ps->s)` — not a re-render of the format, a copy of it.
        None => Ok(fmt.to_vec()),
        // C's `case '*': return(-1)`. (Its `default:` is unreachable: the scan's
        // strpbrk set contains nothing else.)
        Some(i) if fmt[i] == b'*' => Err(CalcError::InvalidFormat),
        Some(_) => Ok(c_snprintf(fmt, val)),
    }
}

/// The conversion the C scan finds in a PRINTF/BIN_WRITE format — the index of
/// the character, since BIN_WRITE also needs the `h`/`l` modifier in front of it
/// (`s[-1]`). C runs the identical block in both (`sCalcPerform.c:1541-1544` and
/// `:1574-1577`), so it is one function here; the two differ only in what they do
/// when there is no conversion (PRINTF copies the format, BIN_WRITE fails).
///
/// This is NOT `findConversionIndicator` (`:105-150`), which SSCANF and BIN_READ
/// use: that one also skips assign-suppressed conversions and knows `%[...]`.
fn conversion_index(fmt: &[u8]) -> Option<usize> {
    // `while ((s1 = strstr(s, "%%"))) {s = s1+2;}`
    let mut s = 0usize;
    while let Some(p) = find_sub(&fmt[s..], b"%%") {
        s += p + 2;
    }
    // `if ((s = strpbrk(s, "%")) == NULL) ...`
    let pct = find_byte(&fmt[s..], b'%')? + s;
    // `if ((s = strpbrk(s+1, "*cdeEfgGiousxX")) == NULL) ...` — note this scans
    // to the END of the format, not to the end of the spec.
    let conv = fmt[pct + 1..]
        .iter()
        .position(|b| b"*cdeEfgGiousxX".contains(b))?;
    Some(pct + 1 + conv)
}

/// One conversion specification: `%[flags][width][.precision][length]conv`.
struct Spec {
    minus: bool,
    plus: bool,
    space: bool,
    zero: bool,
    alt: bool,
    width: usize,
    precision: Option<usize>,
    conv: u8,
    /// One past the conversion character.
    end: usize,
}

/// Parse the spec that starts at `fmt[i] == b'%'`. `None` when what follows is
/// not a conversion C would render (`%z`, a trailing `%`), which `snprintf` then
/// emits literally.
fn parse_spec(fmt: &[u8], i: usize) -> Option<Spec> {
    let mut j = i + 1;
    let (mut minus, mut plus, mut space, mut zero, mut alt) = (false, false, false, false, false);
    while let Some(&c) = fmt.get(j) {
        match c {
            b'-' => minus = true,
            b'+' => plus = true,
            b' ' => space = true,
            b'0' => zero = true,
            b'#' => alt = true,
            _ => break,
        }
        j += 1;
    }
    let mut width = 0usize;
    while let Some(&c) = fmt.get(j) {
        if !c.is_ascii_digit() {
            break;
        }
        width = width * 10 + (c - b'0') as usize;
        j += 1;
    }
    let mut precision = None;
    if fmt.get(j) == Some(&b'.') {
        j += 1;
        let mut p = 0usize;
        while let Some(&c) = fmt.get(j) {
            if !c.is_ascii_digit() {
                break;
            }
            p = p * 10 + (c - b'0') as usize;
            j += 1;
        }
        precision = Some(p);
    }
    // Length modifiers: C passes a `long` or a `double` whatever the format says,
    // so these change nothing here.
    while matches!(
        fmt.get(j),
        Some(b'h' | b'l' | b'L' | b'q' | b'j' | b'z' | b't')
    ) {
        j += 1;
    }
    let conv = *fmt.get(j)?;
    if !b"cdiouxXeEfgGs".contains(&conv) {
        return None;
    }
    Some(Spec {
        minus,
        plus,
        space,
        zero,
        alt,
        width,
        precision,
        conv,
        end: j + 1,
    })
}

/// C `snprintf(tmpstr, TMPSTR_SIZE, format, arg)` with ONE argument: the format
/// is walked from the start, `%%` becomes `%`, and the argument lands in the
/// first conversion.
///
/// A format with a SECOND conversion is undefined behaviour in C (snprintf reads
/// a vararg that was never passed), so there is no behaviour to match: the extra
/// spec is copied out literally rather than fed an invented value.
fn c_snprintf(fmt: &[u8], val: &StackValue) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0usize;
    let mut consumed = false;
    while i < fmt.len() {
        if fmt[i] != b'%' {
            out.push(fmt[i]);
            i += 1;
            continue;
        }
        if fmt.get(i + 1) == Some(&b'%') {
            out.push(b'%');
            i += 2;
            continue;
        }
        match parse_spec(fmt, i) {
            Some(spec) if !consumed => {
                consumed = true;
                out.extend_from_slice(&render_spec(&spec, val));
                i = spec.end;
            }
            Some(spec) => {
                out.extend_from_slice(&fmt[i..spec.end]);
                i = spec.end;
            }
            None => {
                out.push(fmt[i]);
                i += 1;
            }
        }
    }
    out
}

fn render_spec(spec: &Spec, val: &StackValue) -> Vec<u8> {
    let body = match spec.conv {
        b's' => {
            let text = val.clone().into_string_value();
            let mut b = text.as_bytes().to_vec();
            // `%.Ns` prints at most N bytes.
            if let Some(p) = spec.precision {
                b.truncate(p);
            }
            return pad(b, spec.width, spec.minus, false);
        }
        // C `l = myNINT(ps1->d)` and then a `%d`/`%x`/... conversion, which reads
        // an INT out of the vararg: the value is the low 32 bits of that long.
        b'c' | b'd' | b'i' | b'o' | b'u' | b'x' | b'X' => {
            let l = my_nint(val.to_double()) as i64;
            return pad(render_int(spec, l), spec.width, spec.minus, spec.zero);
        }
        b'e' | b'E' => cvt::fmt_e(
            val.to_double(),
            spec.precision.unwrap_or(6),
            spec.conv == b'E',
        ),
        b'f' => cvt::fmt_f(val.to_double(), spec.precision.unwrap_or(6)),
        b'g' | b'G' => cvt::fmt_g(
            val.to_double(),
            spec.precision.unwrap_or(6),
            spec.conv == b'G',
            spec.alt,
        ),
        _ => unreachable!("parse_spec only returns C's conversion characters"),
    };
    // Float: the sign flags, the `#` decimal point, then the padding. C does not
    // zero-pad a non-finite value.
    let d = val.to_double();
    let mut body = body;
    if spec.alt && !body.contains('.') && d.is_finite() {
        body.push('.');
    }
    if d.is_sign_positive() && !body.starts_with('+') {
        if spec.plus {
            body.insert(0, '+');
        } else if spec.space {
            body.insert(0, ' ');
        }
    }
    pad(
        body.into_bytes(),
        spec.width,
        spec.minus,
        spec.zero && d.is_finite(),
    )
}

fn render_int(spec: &Spec, l: i64) -> Vec<u8> {
    if spec.conv == b'c' {
        return vec![l as u8];
    }
    let (mut prefix, digits) = match spec.conv {
        b'd' | b'i' => {
            let v = l as i32;
            let sign = if v < 0 {
                "-".to_string()
            } else if spec.plus {
                "+".to_string()
            } else if spec.space {
                " ".to_string()
            } else {
                String::new()
            };
            (sign, v.unsigned_abs().to_string())
        }
        b'u' => (String::new(), (l as u32).to_string()),
        b'o' => {
            let d = format!("{:o}", l as u32);
            let p = if spec.alt && !d.starts_with('0') {
                "0".to_string()
            } else {
                String::new()
            };
            (p, d)
        }
        b'x' | b'X' => {
            let v = l as u32;
            let d = if spec.conv == b'x' {
                format!("{v:x}")
            } else {
                format!("{v:X}")
            };
            let p = match (spec.alt, v, spec.conv) {
                (true, 0, _) => String::new(),
                (true, _, b'x') => "0x".to_string(),
                (true, _, _) => "0X".to_string(),
                _ => String::new(),
            };
            (p, d)
        }
        _ => unreachable!("render_int only sees C's integer conversions"),
    };
    // `%.Nd` is a MINIMUM digit count, zero-filled, and it turns the `0` flag off.
    let digits = match spec.precision {
        Some(p) if digits.len() < p => format!("{}{digits}", "0".repeat(p - digits.len())),
        _ => digits,
    };
    prefix.push_str(&digits);
    prefix.into_bytes()
}

/// The width field. `zero` pads with `0` after any sign or `0x` prefix, and is
/// ignored when the value is left-justified (C: "the 0 flag is ignored with -").
fn pad(body: Vec<u8>, width: usize, minus: bool, zero: bool) -> Vec<u8> {
    if body.len() >= width {
        return body;
    }
    let fill = width - body.len();
    if minus {
        let mut out = body;
        out.extend(std::iter::repeat_n(b' ', fill));
        return out;
    }
    if !zero {
        let mut out = vec![b' '; fill];
        out.extend(body);
        return out;
    }
    // Keep the sign / `0x` in front of the zeros.
    let skip = match body.first() {
        Some(b'-' | b'+' | b' ') => 1,
        _ if body.starts_with(b"0x") || body.starts_with(b"0X") => 2,
        _ => 0,
    };
    let mut out = body[..skip].to_vec();
    out.extend(std::iter::repeat_n(b'0', fill));
    out.extend_from_slice(&body[skip..]);
    out
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
/// C finds the conversion character with the same scan PRINTF uses
/// ([`conversion_index`]) and bails out (`return -1`) on `*` (suppressed
/// assignment), on `s`, and — unlike PRINTF — when there is no conversion
/// character at all.
fn bin_write(f: &[u8], val: &StackValue) -> Result<String, CalcError> {
    let conv = conversion_index(f).ok_or(CalcError::InvalidFormat)?;

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

/// C `dbTranslateEscape` -> `epicsStrnRawFromEscaped` (epicsString.c:49-118):
/// escaped text in, raw bytes out. The owner of the escape table for TR_ESC and
/// BIN_READ, since C gives both the same function.
///
/// The `\x` cases follow C's `goto input`, which re-enters the loop with the
/// offending character rather than backing up over it — so the `x` is SWALLOWED
/// and the character is processed as if the `\x` had not been there. Compiled C:
/// `\xZ` is `Z` (not `xZ`), `\xA!` is `0x0a` then `!`, and a trailing `\x` is
/// nothing at all. Re-entering also means the character can itself start an
/// escape.
///
/// (C's `!c` breaks are its string terminator; a [`ScalcString`] cannot contain a
/// NUL, so `s.len()` is C's `strlen` and those breaks have nothing to do here.)
fn raw_from_escaped(s: &[u8]) -> Vec<u8> {
    fn nibble(c: u8) -> u8 {
        (c as char).to_digit(16).expect("checked is_ascii_hexdigit") as u8
    }

    let mut out = Vec::new();
    let mut i = 0;
    'next: while i < s.len() {
        let mut c = s[i];
        i += 1;
        // C's `input:` label — `goto input` comes back here with a new `c`.
        loop {
            if c != b'\\' {
                out.push(c);
                continue 'next;
            }
            let Some(&e) = s.get(i) else { break 'next };
            i += 1;
            match e {
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
                    // `if (!srclen--) goto done;` — a trailing `\x` ends it.
                    let Some(&c1) = s.get(i) else { return out };
                    i += 1;
                    if !c1.is_ascii_hexdigit() {
                        c = c1; // goto input
                        continue;
                    }
                    let u = nibble(c1);
                    let Some(&c2) = s.get(i) else {
                        out.push(u);
                        return out;
                    };
                    i += 1;
                    if !c2.is_ascii_hexdigit() {
                        out.push(u);
                        c = c2; // goto input
                        continue;
                    }
                    out.push((u << 4) | nibble(c2));
                }
                other => out.push(other),
            }
            continue 'next;
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

/// `SUBRANGE`'s bounds (`sCalcPerform.c:1875-1895`). A bound is NOT one of the
/// numeric positions C coerces — C branches on its TYPE:
///
/// ```c
/// if (isDouble(ps1)) { i = (int)ps1->d;  if (i < 0) i += k; }
/// else { s = strstr(ps->s, ps1->s);  i = s ? (s - ps->s) + strlen(ps1->s) : 0; }
///
/// if (isDouble(ps2)) { j = (int)ps2->d;  if (j < 0) j += k; }
/// else if (*(ps2->s)) { s = strstr(ps->s, ps2->s);  j = s ? (s - ps->s) - 1 : k; }
/// else { j = k; }
///
/// i = myMAX(myMIN(i,k),0);   /* i is clamped BOTH ways */
/// j = myMIN(j,k);            /* j only from above — a negative j selects nothing */
/// ```
///
/// So a string START bound puts the range just AFTER its match and a string END
/// bound just BEFORE its match, each falling back to the whole string when the
/// search fails (`i = 0`, `j = k`), and an empty END bound means "to the end"
/// — without that special case `strstr` would match at 0 and give `j = -1`.
///
/// Only the DOUBLE branch wraps a negative bound around the end; a search never
/// produces one except the `j = -1` of a match at position 0, which C leaves
/// negative on purpose (`"hello world"["h","h"]` is empty). That is why the wrap
/// lives in the branch and not in the clamp.
fn subrange_bounds(subject: &[u8], start: &StackValue, end: &StackValue) -> (i64, i64) {
    let k = subject.len() as i64;
    let i = match start {
        StackValue::Double(d) => {
            let i = *d as i64;
            if i < 0 { i + k } else { i }
        }
        StackValue::Str(needle) => {
            find_sub(subject, needle.as_bytes()).map_or(0, |p| (p + needle.len()) as i64)
        }
    };
    let j = match end {
        StackValue::Double(d) => {
            let j = *d as i64;
            if j < 0 { j + k } else { j }
        }
        StackValue::Str(needle) if needle.is_empty() => k,
        StackValue::Str(needle) => find_sub(subject, needle.as_bytes()).map_or(k, |p| p as i64 - 1),
    };
    (i.clamp(0, k), j.min(k))
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
