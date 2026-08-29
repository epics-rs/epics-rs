use super::cast::{c_int, c_long, d2ui, imod, my_nint, nint};
use super::cvt;
use super::error::CalcError;
use super::opcodes::{CoreOp, Opcode, StringOp};
use super::random::local_random;
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

    // C's static UNTIL pre-scan (`:339-360`), which runs before any opcode does and
    // fails the perform outright when the postfix holds more than nine of them —
    // whether or not they are ever reached.
    expr.check_until_ceiling()?;

    let mut stack: Vec<StackValue> = Vec::with_capacity(20);
    let code = &expr.code;
    let mut pc = 0;
    // C's `until_scratch[]` (`sCalcPerform.c:330`) and `loopsDone` (`:331`) —
    // one entry per UNTIL in the program, and ONE iteration budget for all of
    // them together.
    let mut until_marks: Vec<(usize, usize)> = Vec::new();
    let mut loops_done: i32 = 0;
    // C compiles the USES_STRING marker into the postfix and `sCalcPerform`
    // switches on it ONCE (`sCalcPerform.c:399`) to pick a whole evaluator. The
    // marker is the compiler's — `CompiledExpr::uses_string`, latched at element
    // lookup — never re-derived from the finished opcodes here.
    let string_path = expr.uses_string;

    while pc < code.len() {
        let op = &code[pc];
        pc += 1;

        match op {
            Opcode::Core(core) => match core {
                CoreOp::End => break,

                CoreOp::PushConst(v) => stack.push(StackValue::Double(*v)),
                CoreOp::PushVar(idx) => {
                    // C `FETCH_A..P` (`sCalcPerform.c:858-864`) — bounded by the
                    // CALLER's `numArgs`, not by the size of the engine's array. An
                    // arg the caller never supplied is 0 (`:425`), not a phantom slot.
                    let v = inputs.num_arg(*idx as usize).unwrap_or(0.0);
                    stack.push(StackValue::Double(v));
                }
                CoreOp::PushDoubleVar(idx) => {
                    // In the string evaluator, double vars are string vars —
                    // C `FETCH_AA..LL` (`:866-876`), bounded by `numSArgs`. C empties
                    // the cell BEFORE the range test, so an unsupplied string arg is
                    // the empty string. Under transform, which passes `numSArgs == 0`
                    // (`transformRecord.c:593`), that is EVERY string arg.
                    let s = inputs.str_arg(*idx as usize).cloned().unwrap_or_default();
                    stack.push(StackValue::Str(s));
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
                    stack.push(StackValue::Double(local_random()));
                }
                CoreOp::NormalRandom => {
                    let u1 = local_random();
                    let u2 = local_random();
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
                    // (`return(-1)`), never NaN. That rule is C's and is kept.
                    //
                    // sCalc's dialect narrowing is `(long)` (LP64 64-bit), so
                    // `cast::imod` is given `c_long`. C's width actually depends
                    // on which evaluator it picked (no-string `(int)`,
                    // `sCalcPerform.c:558-563`; string `(long)`, `:1102-1110`);
                    // the port models the wider `(long)` path, so 3e9 survives
                    // intact (`3e9 % 7 = 4`, `NINT(3e9) = 3e9`). `cast::imod`
                    // owns the -1 guard. See CBUG-A2.
                    match imod(a, b, c_long) {
                        Some(r) => stack.push(StackValue::Double(r)),
                        None => return Err(CalcError::DivisionByZero),
                    }
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

                // Bitwise. sCalc has no `d2i`: the operands take plain C casts,
                // and the WIDTH of that cast is not uniform — it depends on the
                // operator AND on which evaluator C picked for this program:
                //
                //   op         no-string path        string path
                //   & | ^      (long)  :575-591      (long)  :1119-1147
                //   ~          (long)  :725-727      (int)   :1441-1444
                //   >> <<      (long)  :623-631      (int)   :1270-1276
                //
                // That asymmetry is C's, not a typo to be tidied: the same
                // `A>>B` really does shift 64-bit in one program and 32-bit in
                // another, and the port applied `(long)` to all six everywhere.
                // Neither shift count is masked in C — x86-64 masks it to 6 bits
                // for a 64-bit operand and 5 for a 32-bit one, which is what
                // `wrapping_shl` / `wrapping_shr` do at each width.
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
                    stack.push(StackValue::Double(if string_path {
                        !c_int(a) as f64
                    } else {
                        !c_long(a) as f64
                    }));
                }
                // `>>` and `<<` on the string path are TYPE-BRANCHED, on the LEFT
                // operand (`sCalcPerform.c:1263-1294`):
                //
                // ```c
                // ps1 = ps;  toDouble(ps1);
                // j = myNINT(ps1->d);  j = myMIN(j, SCALC_STRING_SIZE);
                // DEC(ps);
                // if (isDouble(ps)) {              /* bit shift, 32-bit */
                //     ps->d = (int)(ps->d) >> (int)(ps1->d);
                // } else {                         /* CHARACTER shift    */
                //     if (op == RIGHT_SHIFT) {
                //         for (i=SCALC_STRING_SIZE-1; i>=0; i--)
                //             ps->s[i] = (i>=j) ? ps->s[i-j] : ' ';
                //         ps->s[SCALC_STRING_SIZE-1] = '\0';
                //     } else {
                //         if (j == SCALC_STRING_SIZE) ps->s[0] = '\0';
                //         else for (i=0; i < SCALC_STRING_SIZE-j; i++)
                //             ps->s[i] = ps->s[i+j];
                //     }
                // }
                // ```
                //
                // So `"abc">>1` is `" abc"` — a STRING — where the port answered
                // the double 0.0. The shift moves bytes in the 40-byte element:
                // right fills the vacated head with SPACES and forces the last
                // byte to NUL (so a full-width string loses its tail), left slides
                // the bytes down and lets the NUL come with them. The count is
                // ROUNDED (myNINT, so 0.6 shifts by 1) and clamped ABOVE at 40 —
                // the two ops there are `j` (rounded/clamped, for the characters)
                // and `(int)ps1->d` (truncated, for the bits): different operands,
                // deliberately, and the port must not share one.
                CoreOp::Shl | CoreOp::Shr => {
                    let right = core == &CoreOp::Shr;
                    let count = pop1(&mut stack)?.to_double();
                    let subject = pop1(&mut stack)?;
                    stack.push(match subject {
                        StackValue::Str(s) if string_path => {
                            StackValue::str(shift_chars(s.as_bytes(), count, right))
                        }
                        v => {
                            let a = v.to_double();
                            let out = if string_path {
                                let (x, n) = (c_int(a), c_int(count) as u32);
                                if right {
                                    x.wrapping_shr(n) as f64
                                } else {
                                    x.wrapping_shl(n) as f64
                                }
                            } else {
                                let (x, n) = (c_long(a), c_long(count) as u32);
                                if right {
                                    x.wrapping_shr(n) as f64
                                } else {
                                    x.wrapping_shl(n) as f64
                                }
                            };
                            StackValue::Double(out)
                        }
                    });
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
                // C `sCalcPerform.c:513-515` / `:1046-1049` — a conditional
                // negate, NOT `fabs`. See [`super::abs_val`].
                CoreOp::Abs => {
                    let a = pop1_f64(&mut stack)?;
                    stack.push(StackValue::Double(super::abs_val(a)));
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
                    // sCalc `sCalcPerform.c:716-719` narrows with a plain
                    // `(long)` (64-bit); `cast::nint` is given `c_long`, so
                    // `NINT(3e9) = 3e9` (no 32-bit loss). CBUG-A2.
                    stack.push(StackValue::Double(nint(a, c_long)));
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
                    stack.push(StackValue::Double(super::isinf(a)));
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

                // C `MAX` / `MIN` (`sCalcPerform.c:1927-1962`). The argument TYPES
                // are settled for the whole call by a pre-scan over every one of
                // them ([`Operands::of`]), never by the first one popped.
                CoreOp::Max(nargs) => {
                    let args = pop_n(&mut stack, *nargs as usize)?;
                    stack.push(Extremum::Max.fold(Operands::of(args)));
                }
                CoreOp::Min(nargs) => {
                    let args = pop_n(&mut stack, *nargs as usize)?;
                    stack.push(Extremum::Min.fold(Operands::of(args)));
                }

                // C `MAX_VAL` / `MIN_VAL` — the `>?` / `<?` operators
                // (`sCalcPerform.c:1296-1328`). They are NOT numeric-only: they
                // are written in the same three-branch shape as ADD, SUB and the
                // comparisons, which is exactly what [`Pair::of`] models. Both
                // operands strings => strcmp, and the RESULT IS THE STRING.
                //
                // Compiled C: `"abc">?"abd"` = "abd", `"b"<?"a"` = "a". The port
                // used `pop2_f64` and answered the double 0 for both.
                //
                // Distinct from `MAX` / `MIN` (a different opcode, W10-A1): those
                // pre-scan n arguments; these classify a pair, and they carry no
                // `isnan` clause — `NAN >? 5` keeps the NaN because `NAN < 5` is
                // false, so C's `if (ps->d < ps1->d)` simply never fires.
                CoreOp::MaxVal | CoreOp::MinVal => {
                    let which = match core {
                        CoreOp::MaxVal => Extremum::Max,
                        _ => Extremum::Min,
                    };
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(match Pair::of(a, b) {
                        Pair::Numeric(a, b) => StackValue::Double(which.pick(a, b)),
                        Pair::Strings(a, b) => StackValue::Str(which.pick(a, b)),
                    });
                }

                // Store
                CoreOp::StoreVar(idx) => {
                    // C `STORE_A..P` (`:878-886`) pops unconditionally and stores
                    // only `if (numArgs > (op - STORE_A))` — an arg the caller never
                    // supplied swallows the value silently.
                    let v = pop1_f64(&mut stack)?;
                    if let Some(slot) = inputs.num_arg_mut(*idx as usize) {
                        *slot = v;
                    }
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
                    //
                    // The `strncpy` sits under `if (numSArgs > (op - STORE_AA))`, so
                    // under transform (`numSArgs == 0`, and `psarg` itself NULL) the
                    // store lands nowhere — the value is still popped.
                    let v = pop1(&mut stack)?;
                    if let Some(slot) = inputs.str_arg_mut(*idx as usize) {
                        *slot = v.into_string_value();
                    }
                }
            },

            // C has TWO evaluators, and the no-string one is not a subset of the
            // string one: three opcodes reach it with NO case of their own —
            // `SUBLAST`, `TO_DOUBLE` and `BYTE`, the three string operators that
            // are absent from `sCalcPostfix`'s USES_STRING list
            // (`sCalcPostfix.c:449-471`) and so do not switch C to the string
            // evaluator. They fall to the double-only switch's
            //
            // ```c
            // default:
            //     break;      /* sCalcPerform.c:816-818 */
            // ```
            //
            // which does not touch the stack AT ALL — no pop, no push. That is
            // not the same as evaluating them on doubles, and the difference is
            // observable:
            //
            //   - `A|-B` (both numeric) leaves BOTH operands on the stack, so C's
            //     closing depth check (`pd != topd`) fails and the WHOLE
            //     expression errors: stat -1, VAL/SVAL untouched, CALC_ALARM.
            //     The port used to compute `A-B`.
            //   - `DBL(A)` and `BYTE(A)` leave their one operand in place, which
            //     is the identity — the same value either evaluator would answer,
            //     by a different route.
            //
            // So the rule is one rule, applied to all three: on the no-string
            // path these opcodes DO NOTHING. `StackLeak` at the bottom of this
            // function is the same check C makes, and it is what turns SUBLAST's
            // untouched operands into the expression-level error.
            Opcode::String(StringOp::SubLast | StringOp::ToDouble | StringOp::Byte)
                if !string_path => {}

            Opcode::String(sop) => match sop {
                StringOp::PushString(s) => {
                    // C LITERAL_STRING (sCalcPerform.c:1493-1502) copies the
                    // literal out of the postfix into the 40-byte element with
                    // `for (i=0; (i<SCALC_STRING_SIZE-1) && *post; )` — so an
                    // over-long literal is truncated at RUN time, not compile
                    // time.
                    stack.push(StackValue::str(s));
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
                    // C `LEN` (sCalcPerform.c:1520-1526) opens with `toString(ps)`,
                    // so a DOUBLE operand is measured in its string form:
                    // `LEN(4)` is 10, the width of "4.00000000".
                    let v = pop1(&mut stack)?;
                    stack.push(StackValue::Double(v.into_string_value().len() as f64));
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
                        StackValue::Str(s) => StackValue::str_ncpy(raw_from_escaped(s.as_bytes())),
                    });
                }
                StringOp::Esc => {
                    // C ESC (sCalcPerform.c:1805-1815) — the same table run
                    // backwards, and a double is again left alone.
                    let v = pop1(&mut stack)?;
                    stack.push(match v {
                        StackValue::Double(d) => StackValue::Double(d),
                        StackValue::Str(s) => StackValue::str_ncpy(escaped_from_raw(s.as_bytes())),
                    });
                }
                StringOp::Printf => {
                    // Pop format string, then one value
                    let val = pop1(&mut stack)?;
                    let fmt = pop1(&mut stack)?;
                    let result = simple_printf(fmt.as_bytes()?, &val)?;
                    stack.push(StackValue::str_ncpy(result));
                }
                StringOp::Sscanf => {
                    // Pop format string, then input string
                    let fmt = pop1(&mut stack)?;
                    let input = pop1(&mut stack)?;
                    // C `if (i != 1) return(-1)` (sCalcPerform.c:1687): a failed
                    // conversion is an ERROR, not a zero.
                    let result = super::scanf::sscanf(input.as_bytes()?, fmt.as_bytes()?)?;
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
                    stack.push(StackValue::str_ncpy(result));
                }
                StringOp::Crc16 => {
                    let v = pop1(&mut stack)?;
                    stack.push(checksum_op(v, crc16_escaped, Combine::Replace));
                }
                StringOp::Crc16Append => {
                    let v = pop1(&mut stack)?;
                    stack.push(checksum_op(v, crc16_escaped, Combine::Append));
                }
                StringOp::Lrc => {
                    let v = pop1(&mut stack)?;
                    stack.push(checksum_op(v, super::checksum::lrc, Combine::Replace));
                }
                StringOp::LrcAppend => {
                    let v = pop1(&mut stack)?;
                    stack.push(checksum_op(v, super::checksum::lrc, Combine::AsciiFrame));
                }
                StringOp::Xor8 => {
                    let v = pop1(&mut stack)?;
                    stack.push(checksum_op(v, xor8_escaped, Combine::Replace));
                }
                StringOp::Xor8Append => {
                    let v = pop1(&mut stack)?;
                    stack.push(checksum_op(v, xor8_escaped, Combine::Append));
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
                    // C `REPLACE` (sCalcPerform.c:1903-1924) opens with
                    // `toString` on ALL THREE operands, so `4{"4","x"}` is
                    // "x.00000000" — the port raised TypeMismatch instead.
                    // Only the first occurrence is replaced (C `strstr`).
                    let replace = pop1(&mut stack)?.into_string_value();
                    let find = pop1(&mut stack)?.into_string_value();
                    let subject = pop1(&mut stack)?.into_string_value();
                    let s = subject.as_bytes();
                    let mut result = s.to_vec();
                    if let Some(pos) = find_sub(s, find.as_bytes()) {
                        result.splice(pos..pos + find.len(), replace.as_bytes().iter().copied());
                    }
                    stack.push(StackValue::str(result));
                }
                StringOp::SubLast => {
                    // C gives SUBLAST no case of its own: it shares `case SUB`
                    // (`sCalcPerform.c:979-1012`), so it is the SAME operator
                    // with the same mixed-type rule, and only the both-strings
                    // branch splits on `op == SUB` (first occurrence) vs
                    // SUBLAST (last one). A double on either side therefore makes
                    // `|-` plain subtraction: C's `4|-"."` is 4 and `"a.b"|-4` is
                    // -4. The port took `as_bytes()?` on both operands and raised
                    // TypeMismatch for either.
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    stack.push(match Pair::of(a, b) {
                        Pair::Numeric(x, y) => StackValue::Double(x - y),
                        Pair::Strings(x, y) => {
                            let mut out = x.into_bytes();
                            if let Some(pos) = rfind_sub(&out, y.as_bytes()) {
                                out.drain(pos..pos + y.len());
                            }
                            StackValue::str(out)
                        }
                    });
                }
                StringOp::DynFetch => {
                    // C `A_FETCH` (`sCalcPerform.c:1446-1460`):
                    //
                    // ```c
                    // if (isDouble(ps)) d = ps->d; else { d = atof(ps->s); ps->s = NULL; }
                    // i = myNINT(d);
                    // if (i >= numArgs || i < 0) { printf(...); ps->d = 0; }
                    // else                        ps->d = parg[i];
                    // ```
                    //
                    // so the operand is the INDEX: `@0` is A, `@1` is B, and a
                    // string index goes through `atof` first (`@"1"` is B). Out of
                    // range is 0, not an error. `numArgs` is the CALLER's argument
                    // count — [`StringInputs::num_arg`], not the array's length.
                    let idx = my_nint(pop1(&mut stack)?.to_double());
                    let v = c_int_to_index(idx)
                        .and_then(|i| inputs.num_arg(i))
                        .unwrap_or(0.0);
                    stack.push(StackValue::Double(v));
                }
                StringOp::DynSFetch => {
                    // C `A_SFETCH` (`sCalcPerform.c:1462-1476`) — the same index
                    // rule, applied to the STRING arguments: `@@0` is AA. C points
                    // the cell at its local buffer and empties it BEFORE the range
                    // test, so an out-of-range `@@` is the empty STRING (still a
                    // string, still not an error).
                    let idx = my_nint(pop1(&mut stack)?.to_double());
                    let s = c_int_to_index(idx)
                        .and_then(|i| inputs.str_arg(i))
                        .cloned()
                        .unwrap_or_default();
                    stack.push(StackValue::Str(s));
                }
                StringOp::DynStore => {
                    // C `A_STORE` — the string path (`sCalcPerform.c:897-906`) and
                    // the no-string one (`:440-449`) are the same three steps:
                    //
                    // ```c
                    // toDouble(ps);  ps1 = ps;  DEC(ps);     /* the value  */
                    // toDouble(ps);  i = myNINT(ps->d);  DEC(ps);  /* the index */
                    // if (i >= numArgs || i < 0) printf(...); else parg[i] = ps1->d;
                    // ```
                    //
                    // The index was emitted BEFORE the value, so it is the deeper of
                    // the two. Both are popped and nothing is pushed — an assignment
                    // is not an expression in C's calc. Out of range stores nothing
                    // and is not an error, exactly as for `@`.
                    let value = pop1(&mut stack)?.to_double();
                    let idx = my_nint(pop1(&mut stack)?.to_double());
                    if let Some(slot) = c_int_to_index(idx).and_then(|i| inputs.num_arg_mut(i)) {
                        *slot = value;
                    }
                }
                StringOp::DynSStore => {
                    // C `A_SSTORE` (`sCalcPerform.c:909-918`) — the same, with
                    // `toString(ps)` on the value and the STRING args as the target.
                    // (It has no case on the no-string path, but it cannot get
                    // there: `@@` is `A_SFETCH`, which IS in the USES_STRING list,
                    // so a program containing one always runs this evaluator.)
                    let value = pop1(&mut stack)?.into_string_value();
                    let idx = my_nint(pop1(&mut stack)?.to_double());
                    if let Some(slot) = c_int_to_index(idx).and_then(|i| inputs.str_arg_mut(i)) {
                        *slot = value;
                    }
                }
            },

            Opcode::Control(ctrl) => match ctrl {
                // C `UNTIL` (`sCalcPerform.c:1978-1993`) does one thing: it
                // remembers the stack pointer, so that a later `UNTIL_END` can
                // wind the stack back to exactly this point before re-running the
                // body (`ps = until_scratch[i].ps`, `:2004`). `until_loc` is the
                // key C matches on, and `pc - 1` is that key here.
                super::opcodes::ControlOp::Until(_end_pc) => {
                    let until_pc = pc - 1;
                    match until_marks.iter_mut().find(|(k, _)| *k == until_pc) {
                        Some((_, depth)) => *depth = stack.len(),
                        // No ceiling to enforce here: the program cannot hold more
                        // than nine UNTILs, because `check_until_ceiling` refused it
                        // before the first opcode ran. This table is a location map,
                        // nothing more.
                        None => until_marks.push((until_pc, stack.len())),
                    }
                }
                super::opcodes::ControlOp::UntilEnd(start_pc) => {
                    // C `sCalcPerform.c:1995-2018`, in this order:
                    //
                    // ```c
                    // if (++loopsDone > sCalcLoopMax) break;   /* give up, no error */
                    // if (ps->d == 0) { ...wind back and re-run the body... }
                    // ```
                    //
                    // `loopsDone` counts EVERY arrival at an UNTIL_END, is shared
                    // by all the UNTILs in one program, and when it runs out C
                    // simply STOPS LOOPING: the perform continues, returns 0, and
                    // the value is whatever the last condition evaluated to. There
                    // is no loop-limit error in sCalc, and the record does not
                    // alarm. (`sCalcLoopMax` is an ioc-shell variable, so the
                    // ceiling is settable — see `scalc_loop_max`.)
                    loops_done += 1;
                    if loops_done > scalc_loop_max() {
                        continue;
                    }
                    // C PEEKS the condition (`:1999`: `if (ps->d == 0)`); it pops
                    // nothing, which is why UNTIL_END has a runtime effect of 0.
                    //
                    // DELIBERATE DEVIATION — a STRING-valued condition.
                    //
                    // C's line is `if (ps->d==0)` with NO `toDouble(ps)` in front of
                    // it, and `LITERAL_STRING`'s push (`:1493-1499`) sets `ps->s` and
                    // never touches `ps->d`. So when the condition is a string, C
                    // tests an UNINITIALISED double — whatever was last left in that
                    // stack cell — and the string's own content is irrelevant.
                    // Compiled upstream, both of these exit after ONE iteration
                    // (A=1), because the stale `d` happened to be non-zero:
                    //
                    //     A:=0;UNTIL(A:=A+1;"0")   ->  A=1, result "0"
                    //     A:=0;UNTIL(A:=A+1;"1")   ->  A=1, result "1"
                    //
                    // There is no C semantic here to match: the same expression under
                    // a different stack history takes a different branch. The port
                    // reads the condition through `to_double` (`atof`), the same
                    // coercion every other numeric context applies to a string, so
                    // `"0"` is false and loops to `sCalcLoopMax` while `"1"` is true
                    // and exits. That is defined, and it is what an expression writing
                    // `UNTIL(...; "0")` can only have meant.
                    //
                    // Ported UB would be worse than a documented difference, so this
                    // is on the record as a divergence rather than reproduced.
                    // aCalc's UNTIL_END (`array.rs`) carries the same deviation for
                    // an array-valued condition, for the same reason.
                    let cond = match stack.last() {
                        Some(v) => v.to_double(),
                        None => return Err(CalcError::Underflow),
                    };
                    if cond == 0.0 {
                        // Wind the stack back to where the paired UNTIL saw it —
                        // C restores the saved `ps` wholesale, so everything the
                        // body pushed (the condition included) is discarded.
                        let Some((_, depth)) = until_marks.iter().find(|(k, _)| k == start_pc)
                        else {
                            // C's `printf("sCalcPerform: UNTIL not found"); return(-1)`.
                            return Err(CalcError::Internal);
                        };
                        stack.truncate(*depth);
                        pc = *start_pc + 1;
                    }
                    // Condition true: fall out of the loop with the condition value
                    // still on the stack. It is the value of the `UNTIL(...)`.
                }
            },

            #[allow(unreachable_patterns)]
            _ => return Err(CalcError::Internal),
        }
    }

    // C checks the depth on BOTH evaluator paths, in the same place and to the
    // same rule — `sCalcPerform.c:817-823` (no-string path) and `:2023-2032`
    // (string path):
    //
    // ```c
    // /* if everything is peachy, the stack should end at its first position */
    // if (pd != topd) return(-1);        /* no-string path */
    // if (ps != top)  return(-1);        /* string path    */
    // ```
    //
    // The stack pointer starts one BELOW the first slot and every push increments
    // first, so "ends at its first position" means the stack holds EXACTLY ONE
    // value. A leaked operand and a fully-consumed stack are both -1, and C writes
    // neither `*presult` nor `psresult`.
    //
    // The port returned `stack.last()`: a leaked operand was published as VAL/SVAL,
    // and an empty stack invented a 0.0 that C never produces. `aCalcPerform.c:1607`
    // has the identical check, closed for aCalc by b87dcbd7; this is the sCalc half
    // of that family, and it takes the same shape — the one-value invariant PRODUCES
    // the result rather than being checked beside it.
    //
    // Like the aCalc half, it is an invariant guard: `sCalcPostfix`'s compile-time
    // `runtime_depth` ledger rejects any program that would not end at depth 1, so
    // no source expression the port's compiler accepts can reach it. (Compiled C
    // DOES reach it — `4|-2` is -1 there — but for a reason the port does not yet
    // model: C's double-only evaluator has no `case SUBLAST`, silently skips it, and
    // the operand it should have consumed is what trips this check. That gap is
    // reported separately; it is not this guard.)
    let result = match <[StackValue; 1]>::try_from(stack) {
        Ok([result]) => result,
        Err(_) => return Err(CalcError::StackLeak),
    };
    // The non-finite tail (`sCalcPerform.c:834`, `:2056`) is NOT this
    // function's business: C writes `*presult` FIRST and only then returns -1,
    // so the value survives the failing status and the record decides what to
    // do with it. That pairing lives in [`ScalcResult`] / [`epilogue`]; an
    // `Err` here would be C's OTHER -1 — the one an operator raises BEFORE
    // writing anything.
    Ok(result)
}

/// What a checksum opcode does with the digest it computed.
#[derive(Clone, Copy, PartialEq)]
enum Combine {
    /// `CRC16` / `LRC` / `XOR8` — the digest REPLACES the operand.
    Replace,
    /// `MODBUS` / `ADD_XOR8` — the digest is APPENDED to it.
    Append,
    /// `AMODBUS` — appended, and a `:` is PREPENDED (`sCalcPerform.c:1846-1850`):
    ///
    /// ```c
    /// strcpy(tmpstr, ":");
    /// strcat(tmpstr, ps->s);
    /// strNcpy(ps->s, tmpstr, SCALC_STRING_SIZE);
    /// strncat(ps->s, tmpstr10, SCALC_STRING_SIZE-strlen(ps->s)-1);
    /// ```
    ///
    /// The `:` is the ASCII-MODBUS start delimiter, and without it the frame is
    /// not a frame. The port dropped it. (C bounds `":" + operand` to 39 before
    /// appending the LRC, which is the same 39 bytes as bounding the whole
    /// concatenation once — a long operand simply crowds the LRC out.)
    AsciiFrame,
}

/// C's six checksum opcodes (`sCalcPerform.c:1819-1866`) are one shape written
/// out three times:
///
/// ```c
/// if (isString(ps)) {                  /* a DOUBLE operand is left ALONE, not rejected */
///     if (chk(tmpstr, ps->s) == 0) {   /* a FAILED checksum leaves it alone too */
///         if (op == CRC16)  strNcpy(ps->s, tmpstr, SCALC_STRING_SIZE-1);
///         else              strncat(ps->s, tmpstr, SCALC_STRING_SIZE-strlen(ps->s)-1);
///     }
/// }
/// ```
///
/// so neither the type guard nor the failure is an error: compiled sCalc answers
/// `CRC16(4)` = 4 and `CRC16(AA)` = "" for an empty AA, both with st=0. The port
/// used `as_bytes()?`, which raised `TypeMismatch` on a double operand, and had no
/// failure path at all.
///
/// `digest` returns the TEXT C's helper wrote into `tmpstr`, or `None` for its
/// `return(-1)`.
fn checksum_op(
    v: StackValue,
    digest: impl FnOnce(&[u8]) -> Option<String>,
    combine: Combine,
) -> StackValue {
    let StackValue::Str(s) = v else {
        return v; // `if (isString(ps))` — a double falls straight through.
    };
    let Some(text) = digest(s.as_bytes()) else {
        return StackValue::Str(s); // the helper returned -1; C writes nothing.
    };
    match combine {
        // C `strNcpy(ps->s, tmpstr, SCALC_STRING_SIZE-1)` (:1823, :1845, :1861):
        // a 38-byte result. The two appending forms use `strncat` instead, whose
        // `SCALC_STRING_SIZE-strlen(ps->s)-1` bounds the TOTAL to 39.
        Combine::Replace => StackValue::str_ncpy(text),
        Combine::Append => StackValue::str([s.as_bytes(), text.as_bytes()].concat()),
        Combine::AsciiFrame => StackValue::str([b":", s.as_bytes(), text.as_bytes()].concat()),
    }
}

/// C `crc16` (`sCalcPerform.c:192-229`) — the digest CRC16 and MODBUS share.
///
/// Two things the port had wrong, and compiled sCalc confirms both:
///
///   * The operand is ESCAPED text, and the CRC is taken over what
///     `dbTranslateEscape` makes of it (`:199`), not over the operand's own
///     bytes. `MODBUS("\x01\x03")` checksums TWO bytes, not the eight characters
///     that spell them.
///   * The digest is handed back as ESCAPED text — a literal
///     `sprintf(output, "\\x%02x\\x%02x", crc&0xff, (crc&0xff00)>>8)` (`:227`),
///     low byte first. That is NOT the escape table: a printable digest byte is
///     still written `\x41`, never `A` (compiled sCalc: `XOR8("A")` = `\x41`).
///     The frame therefore stays escaped all the way to the octet layer, which is
///     what translates it. The port emitted raw bytes, which the driver then
///     escaped a second time.
///
/// `dbTranslateEscape` returning 0 is C's `return(-1)`, and the caller then leaves
/// the operand untouched.
///
/// The framing above is C's, byte for byte. The digest VALUE deviates for any
/// payload byte ≥ 0x80 — see [`checksum::crc16`](super::checksum::crc16) and
/// CBUG-F8.
fn crc16_escaped(operand: &[u8]) -> Option<String> {
    let raw = raw_from_escaped(operand);
    if raw.is_empty() {
        return None;
    }
    let crc = super::checksum::crc16(&raw);
    Some(format!("\\x{:02x}\\x{:02x}", crc & 0xff, (crc >> 8) & 0xff))
}

/// C `xor8` (`sCalcPerform.c:258-282`) — the digest XOR8 and ADD_XOR8 share. The
/// same shape as [`crc16_escaped`], one byte wide:
/// `sprintf(output, "\\x%02x", xor8&0xff)`.
fn xor8_escaped(operand: &[u8]) -> Option<String> {
    let raw = raw_from_escaped(operand);
    if raw.is_empty() {
        return None;
    }
    Some(format!("\\x{:02x}", super::checksum::xor8(&raw)))
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
    super::strtod::strtod(&s[i..]).value
}

/// C `SMALL` (`sCalcPerform.c:46`) — the tolerance sCalc's numeric comparisons
/// are written around. It is sCalc's alone: base and aCalc compare exactly.
const SMALL: f64 = 1e-11;

/// C `volatile int sCalcLoopMax = 1000` (`sCalcPerform.c:52`), exported to the
/// ioc shell with `epicsExportAddress(int, sCalcLoopMax)` — a settable global,
/// not a constant. It bounds the TOTAL number of `UNTIL_END` arrivals in one
/// perform, across every UNTIL in the program, and running out is not an error:
/// C stops looping and carries on (`:1997`).
static SCALC_LOOP_MAX: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1000);

/// Read `sCalcLoopMax`.
pub fn scalc_loop_max() -> i32 {
    SCALC_LOOP_MAX.load(std::sync::atomic::Ordering::Relaxed)
}

/// Write `sCalcLoopMax` — the ioc-shell `var sCalcLoopMax, <n>`.
pub fn set_scalc_loop_max(n: i32) {
    SCALC_LOOP_MAX.store(n, std::sync::atomic::Ordering::Relaxed);
}

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
        // C `long l = myNINT(ps1->d)` and then a `%d`/`%x`/... conversion, which
        // reads an INT out of the vararg. `myNINT` has ALREADY cast to `int`, so
        // `l` is that 32-bit value sign-extended — never a 64-bit conversion of
        // the double. (`PRINTF("%d",3e9)` is C's `-2147483648`, the indefinite
        // value `cvttsd2si` yields, not `3e9`'s low 32 bits.)
        b'c' | b'd' | b'i' | b'o' | b'u' | b'x' | b'X' => {
            let l = i64::from(my_nint(val.to_double()));
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

/// C's `i = myNINT(d); if (i >= numArgs || i < 0)` — the already-rounded index
/// as a subscript, or `None` when it is negative (the `>= numArgs` half is the
/// caller's `get`). NaN and the non-representable magnitudes arrive here as
/// `INT32_MIN` from [`my_nint`]'s cast, so they take the `None` branch, which is
/// where C's out-of-range test lands them too.
fn c_int_to_index(i: i32) -> Option<usize> {
    usize::try_from(i).ok()
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
    let n = my_nint(d);
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
    let conv = super::scanf::find_conversion_indicator(f).ok_or(CalcError::InvalidFormat)?;
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

/// C's CHARACTER shift — `RIGHT_SHIFT` / `LEFT_SHIFT` with a string left operand
/// (`sCalcPerform.c:1277-1293`). It runs over the stack element's whole 40-byte
/// buffer, not over the visible string, which is what makes it a shift and not a
/// slice: bytes move, the head is space-filled, and the NUL travels with them.
///
/// ```c
/// j = myNINT(ps1->d);  j = myMIN(j, SCALC_STRING_SIZE);
/// if (right) {
///     for (i = SCALC_STRING_SIZE-1; i >= 0; i--)
///         ps->s[i] = (i >= j) ? ps->s[i-j] : ' ';
///     ps->s[SCALC_STRING_SIZE-1] = '\0';
/// } else if (j == SCALC_STRING_SIZE) {
///     ps->s[0] = '\0';
/// } else {
///     for (i = 0; i < SCALC_STRING_SIZE-j; i++) ps->s[i] = ps->s[i+j];
/// }
/// ```
///
/// The buffer is modelled zero-padded, which is what `strncpy`/`strNcpy` leave
/// behind on every path that fills a stack element from an argument or a result.
///
/// ONE documented deviation: C clamps `j` only from ABOVE. A NEGATIVE count makes
/// both loops index outside the element (`ps->s[i-j]` with `i` at 39, `ps->s[i+j]`
/// with `i` at 0) — an out-of-bounds read of whatever the neighbouring stack
/// element holds, so compiled C has no defined answer to reproduce. The port
/// clamps below at 0, which makes a negative count the identity — the same thing
/// C does at exactly `j == 0`, and the only continuous choice.
fn shift_chars(bytes: &[u8], count: f64, right: bool) -> Vec<u8> {
    const N: usize = SCALC_STRING_SIZE;
    // C's `j` is an `int`: `myNINT` casts, and `myMIN(j, 40)` clamps only above.
    let j = my_nint(count).clamp(0, N as i32) as usize;

    let mut buf = [0u8; N];
    for (slot, b) in buf.iter_mut().zip(bytes.iter().take(N)) {
        *slot = *b;
    }

    let mut out = buf;
    if right {
        for i in (0..N).rev() {
            out[i] = if i >= j { buf[i - j] } else { b' ' };
        }
        out[N - 1] = 0;
    } else if j == N {
        out[0] = 0;
    } else {
        // The tail `[N-j, N)` is NOT written by C's loop: those bytes keep the
        // values they already had, which `out`'s copy of `buf` preserves.
        out[..N - j].copy_from_slice(&buf[j..]);
    }

    let end = find_byte(&out, 0).unwrap_or(N);
    out[..end].to_vec()
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

/// C `sCalcPerform`'s output: the `(*presult, *psresult)` pair a record keeps
/// as `(VAL, SVAL)` or `(OVAL, OSV)`, TOGETHER with the status C returns
/// beside them.
///
/// C has two different `-1`s and they do not mean the same thing:
///
/// * an operator refuses (`1/0`, `SQRT(-1)`, a failed `SSCANF`):
///   `sCalcPerform` returns -1 from inside the loop, BEFORE the epilogue, so
///   `*presult` is never written and the record's cell keeps its old value.
///   That is the `Err` from [`eval`] — no result exists.
/// * every operator succeeds but the result is not finite (`LOG(0)` = -inf,
///   `1e300*1e300` = +inf, `ACOS(2)` = NaN): the epilogue writes `*presult`
///   and the LAST line then returns -1
///   (`return(((isnan(*presult)||isinf(*presult)) ? -1 : 0))`,
///   `sCalcPerform.c:834`, `:2056`). The cells ARE written. That is this
///   struct with [`ScalcResult::non_finite`] set.
///
/// The two records that consume it read the pair differently, which is why the
/// value cannot be dropped on the failing status: scalcout replaces it with its
/// `VAL = -1` / `SVAL = "***ERROR***"` sentinel (sCalcoutRecord.c:361-363),
/// while transform KEEPS the non-finite value in the channel and merely alarms
/// (transformRecord.c:593-596) — an `inf` that C then fans out through `OUTA`.
#[derive(Debug, Clone, PartialEq)]
pub struct ScalcResult {
    pub val: f64,
    pub sval: ScalcString,
    /// C's `-1` from the non-finite tail: the cells above are written, and the
    /// perform still failed.
    pub non_finite: bool,
}

/// C `sCalcPerform`'s epilogue — and there are TWO of them, one per evaluator,
/// which is why this cannot be a function of the final stack value alone.
///
/// ```c
/// /* NO_STRING (:826-832) */
/// *presult = *pd;
/// if (psresult && (lenSresult > 15)) {
///     if (isnan(*pd)) strcpy(psresult, "NaN");
///     else            cvtDoubleToString(*pd, psresult, precision);
/// }
///
/// /* USES_STRING (:2034-2054) */
/// if (isDouble(ps)) { *presult = ps->d;  to_string(ps); ...copy ps->s... }
/// else              { ...copy ps->s...;  to_double(ps); *presult = ps->d; }
/// ```
///
/// The numeric evaluator renders SVAL at the record's **PREC** — the `precision`
/// argument sCalcoutRecord passes as `pcalc->prec` (`sCalcoutRecord.c:359`,
/// `:770`). The string evaluator never sees `precision`: its `to_string` is
/// `cvtDoubleToString(d, s, 8)` (`sCalcPerform.c:90-96`), hardcoded. Compiled sCalc, `PI`:
///
/// ```text
/// program    PREC=0   PREC=2   PREC=8        PREC=12
/// numeric    "3"      "3.14"   "3.14159265"  " 3.141592653590e+00"
/// string     "3.14159265" whatever the PREC
/// ```
///
/// so at the shipped default PREC=0 a numeric scalcout's SVAL is "3", and the
/// port's uniform precision-8 rendering was wrong for every record that had not
/// set PREC to 8.
///
/// (`lenSresult > 15` always holds: scalcout's buffer is `STRING_SIZE` = 40.)
pub fn epilogue(expr: &CompiledExpr, top: &StackValue, precision: i16) -> ScalcResult {
    let val = top.to_double();
    // C's last line, evaluated on the `*presult` this epilogue just wrote —
    // for a STRING result that is `to_double(ps)` (`sCalcPerform.c:2046-2050`),
    // which is exactly `val` here.
    let non_finite = !val.is_finite();
    if expr.uses_string {
        ScalcResult {
            val,
            sval: top.clone().into_string_value(),
            non_finite,
        }
    } else {
        let sval = if val.is_nan() {
            ScalcString::from_c("NaN")
        } else {
            // `cvtDoubleToString`'s parameter is `epicsUInt16` (`cvtFast.c:114`)
            // while the record's PREC is a `short`, so C reinterprets a negative
            // PREC as a huge unsigned one — which the function then clamps to 17
            // and renders in %e. Not clamped to 0; this cast is that conversion.
            ScalcString::from_c(super::cvt::cvt_double_to_string(val, precision as u16))
        };
        ScalcResult {
            val,
            sval,
            non_finite,
        }
    }
}

fn pop1(stack: &mut Vec<StackValue>) -> Result<StackValue, CalcError> {
    stack.pop().ok_or(CalcError::Underflow)
}

/// Pop `n` operands in C's scan order: `[0]` is the top of the stack, which is
/// where C's `ps` starts and the direction its `DEC(ps)` walks.
fn pop_n(stack: &mut Vec<StackValue>, n: usize) -> Result<Vec<StackValue>, CalcError> {
    if n == 0 || stack.len() < n {
        return Err(CalcError::Underflow);
    }
    Ok(stack.split_off(stack.len() - n).into_iter().rev().collect())
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

/// The n-ary form of [`Pair`], for C's `MAX` and `MIN` varargs
/// (`sCalcPerform.c:1927-1962`).
///
/// C settles the type of the WHOLE operation before it compares anything, by
/// pre-scanning every argument:
///
/// ```c
/// for (i=0, j=0; i<nargs; j |= isDouble(ps-i), i++);
/// if (j) { /* an arg is double: coerce all to double, compare numerically */ }
/// else   { /* all args are string: compare with strcmp, answer a STRING */ }
/// ```
///
/// One double anywhere makes every argument a double. Only an all-string call
/// takes the `strcmp` path, and only that path can answer a string. The port
/// branched on the type of the FIRST argument it popped, so `MAX(4,"a")` raised
/// TypeMismatch where C answers 4 (`atof("a")` is 0), and `MAX("10",9)` would
/// have compared strings where C compares numbers.
enum Operands {
    Numeric(Vec<f64>),
    Strings(Vec<ScalcString>),
}

impl Operands {
    /// `args` in C's scan order: `args[0]` is the top of the stack.
    fn of(args: Vec<StackValue>) -> Operands {
        if args.iter().any(StackValue::is_double) {
            Operands::Numeric(args.iter().map(StackValue::to_double).collect())
        } else {
            Operands::Strings(
                args.into_iter()
                    .map(|v| match v {
                        StackValue::Str(s) => s,
                        StackValue::Double(_) => unreachable!("no arg is a double here"),
                    })
                    .collect(),
            )
        }
    }
}

/// Which end C's extremum operators keep.
#[derive(Clone, Copy)]
enum Extremum {
    Max,
    Min,
}

impl Extremum {
    /// C's binary keep-rule, `if (ps->d < ps1->d) ps->d = ps1->d;` for `MAX_VAL`
    /// (`sCalcPerform.c:1300`) — the LEFT operand survives unless it strictly
    /// loses, so a tie keeps the left. `ScalcString` orders by its bytes, which is
    /// `strcmp`: both compare unsigned bytes and a prefix sorts first.
    ///
    /// `MAX`/`MIN`'s n-ary [`Self::fold`] cannot use this: it carries an extra
    /// `isnan(d)` clause that `MAX_VAL`/`MIN_VAL` do not have.
    fn pick<T: PartialOrd>(self, a: T, b: T) -> T {
        let right_wins = match self {
            Extremum::Max => a < b,
            Extremum::Min => a > b,
        };
        if right_wins { b } else { a }
    }

    /// C's `MAX` / `MIN` fold (`sCalcPerform.c:1930-1962`), walking DOWN the stack
    /// from the top exactly as C does.
    ///
    /// ```c
    /// toDouble(ps);
    /// while (--nargs) {
    ///     d = ps->d;  DEC(ps);  toDouble(ps);
    ///     if (ps->d < d || isnan(d)) ps->d = d;   /* MAX; MIN uses > */
    /// }
    /// ```
    ///
    /// `isnan(d)` tests the RUNNING value, so once a NaN enters the fold it stays:
    /// compiled C answers a PERFORM ERROR for `MAX(NAN,5)` (the record's non-finite
    /// result check rejects the NaN), where the port used to drop the NaN and
    /// answer 5.
    fn fold(self, args: Operands) -> StackValue {
        match args {
            Operands::Numeric(vals) => {
                let mut running = vals[0];
                for &cur in &vals[1..] {
                    let keep_running = match self {
                        Extremum::Max => cur < running,
                        Extremum::Min => cur > running,
                    };
                    if !(keep_running || running.is_nan()) {
                        running = cur;
                    }
                }
                StackValue::Double(running)
            }
            // `if (strcmp(ps->s, ps1->s) < 0) strcpy(ps->s, ps1->s);` — the running
            // value survives only when the incoming argument loses. Byte order is
            // `strcmp`'s: both compare unsigned bytes, and a prefix sorts first.
            Operands::Strings(vals) => {
                let mut running = vals[0].clone();
                for cur in &vals[1..] {
                    let keep_running = match self {
                        Extremum::Max => cur.as_bytes() < running.as_bytes(),
                        Extremum::Min => cur.as_bytes() > running.as_bytes(),
                    };
                    if !keep_running {
                        running = cur.clone();
                    }
                }
                StackValue::Str(running)
            }
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
        // C `i = (int)ps1->d` (`sCalcPerform.c:1876`) — a narrowing of a stack
        // double, so it belongs to the engine's cast owner [`c_int`] and not to
        // an open-coded `as`, exactly as in aCalc's `[` (`pop_subrange_bounds`).
        StackValue::Double(d) => {
            let i = i64::from(c_int(*d));
            if i < 0 { i + k } else { i }
        }
        StackValue::Str(needle) => {
            find_sub(subject, needle.as_bytes()).map_or(0, |p| (p + needle.len()) as i64)
        }
    };
    let j = match end {
        // C `j = (int)ps2->d` (`sCalcPerform.c:1883`).
        StackValue::Double(d) => {
            let j = i64::from(c_int(*d));
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

#[cfg(test)]
mod parity_tests {
    //! C-parity regression tests for the string evaluator.
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

/// The end-of-expression depth invariant — C's `if (pd != topd) return(-1)` on the
/// no-string path (`sCalcPerform.c:817-823`) and `if (ps != top) return(-1)` on the
/// string path (`:2023-2032`).
///
/// These programs are hand-built because `sCalcPostfix` CANNOT emit them: its
/// compile-time `runtime_depth` ledger rejects anything that would not end at depth
/// 1, which is why no source expression reaches the guard. Compiled C agrees with
/// the port's compiler on every probe — `A:=1`, `AA:=4` and `1;2` are all
/// "Incomplete expression, operand missing" in both, and `A:=1;A+1` compiles and
/// runs to 2 in both.
///
/// So the guard is tested where it applies: at the engine's own boundary, against a
/// program that violates the invariant. Same reasoning, and same shape, as the aCalc
/// half in `array.rs` (b87dcbd7).
#[cfg(test)]
mod stack_depth_invariant {
    use super::*;
    use crate::calc::engine::ExprKind;

    fn run(code: Vec<Opcode>) -> Result<StackValue, CalcError> {
        let expr = CompiledExpr {
            code,
            ..CompiledExpr::empty(ExprKind::String)
        };
        eval(&expr, &mut StringInputs::new())
    }

    /// The same, on the evaluator C picks for a USES_STRING program — the marker
    /// is the compiler's, so a hand-built program must state it.
    fn run_string(code: Vec<Opcode>) -> Result<StackValue, CalcError> {
        let expr = CompiledExpr {
            code,
            uses_string: true,
            ..CompiledExpr::empty(ExprKind::String)
        };
        eval(&expr, &mut StringInputs::new())
    }

    #[test]
    fn a_leaked_operand_is_an_error_not_the_top_of_stack() {
        // Two pushes and no operator to consume them: C ends with the stack pointer
        // one ABOVE its first position and returns -1, writing neither *presult nor
        // psresult. The port used to return `stack.last()` — the 2.0 — and publish
        // it as VAL/SVAL.
        let leaked = vec![
            Opcode::Core(CoreOp::PushConst(1.0)),
            Opcode::Core(CoreOp::PushConst(2.0)),
            Opcode::Core(CoreOp::End),
        ];
        assert_eq!(run(leaked), Err(CalcError::StackLeak));
    }

    #[test]
    fn an_empty_stack_is_an_error_not_a_zero() {
        // A store consumes the only value, so the program ends at depth 0 — C's
        // pointer is left one BELOW the first position and the check fails just as
        // hard. The port used to invent a 0.0 that C never produces.
        let consumed = vec![
            Opcode::Core(CoreOp::PushConst(1.0)),
            Opcode::Core(CoreOp::StoreVar(0)),
            Opcode::Core(CoreOp::End),
        ];
        assert_eq!(run(consumed), Err(CalcError::StackLeak));
    }

    #[test]
    fn a_leaked_string_operand_is_an_error_too() {
        // The string path has its own copy of the check (`:2023-2032`), and it is the
        // same rule: depth 1, whatever the type of the value.
        let leaked = vec![
            Opcode::String(StringOp::PushString(b"a".to_vec())),
            Opcode::String(StringOp::PushString(b"b".to_vec())),
            Opcode::Core(CoreOp::End),
        ];
        assert_eq!(run_string(leaked), Err(CalcError::StackLeak));
    }

    #[test]
    fn exactly_one_value_is_the_result() {
        let balanced = vec![
            Opcode::Core(CoreOp::PushConst(1.0)),
            Opcode::Core(CoreOp::PushConst(2.0)),
            Opcode::Core(CoreOp::Add),
            Opcode::Core(CoreOp::End),
        ];
        assert_eq!(run(balanced), Ok(StackValue::Double(3.0)));
    }
}
