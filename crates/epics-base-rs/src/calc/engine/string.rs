use super::error::CalcError;
use super::opcodes::{CoreOp, Opcode, StringOp};
use super::value::StackValue;
use super::{CompiledExpr, StringInputs};

pub fn eval(expr: &CompiledExpr, inputs: &mut StringInputs) -> Result<StackValue, CalcError> {
    let mut stack: Vec<StackValue> = Vec::with_capacity(20);
    let code = &expr.code;
    let mut pc = 0;
    let mut loop_count: usize = 0;

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

                CoreOp::Random => {
                    stack.push(StackValue::Double(simple_random()));
                }
                CoreOp::NormalRandom => {
                    let u1 = simple_random();
                    let u2 = simple_random();
                    let n = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
                    stack.push(StackValue::Double(n));
                }

                // Type-aware arithmetic
                CoreOp::Add => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    match (&a, &b) {
                        (StackValue::Double(x), StackValue::Double(y)) => {
                            stack.push(StackValue::Double(x + y));
                        }
                        (StackValue::Str(x), StackValue::Str(y)) => {
                            let mut result = x.clone();
                            result.push_str(y);
                            stack.push(StackValue::Str(result));
                        }
                        _ => return Err(CalcError::TypeMismatch),
                    }
                }
                CoreOp::Sub => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    match (&a, &b) {
                        (StackValue::Double(x), StackValue::Double(y)) => {
                            stack.push(StackValue::Double(x - y));
                        }
                        (StackValue::Str(x), StackValue::Str(y)) => {
                            // Remove first occurrence of y from x
                            let result = if let Some(pos) = x.find(y.as_str()) {
                                let mut s = x.clone();
                                s.replace_range(pos..pos + y.len(), "");
                                s
                            } else {
                                x.clone()
                            };
                            stack.push(StackValue::Str(result));
                        }
                        _ => return Err(CalcError::TypeMismatch),
                    }
                }
                CoreOp::Mul => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    stack.push(StackValue::Double(a * b));
                }
                CoreOp::Div => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    if b == 0.0 {
                        stack.push(StackValue::Double(f64::NAN));
                    } else {
                        stack.push(StackValue::Double(a / b));
                    }
                }
                CoreOp::Mod => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    if b as i64 == 0 {
                        stack.push(StackValue::Double(f64::NAN));
                    } else {
                        stack.push(StackValue::Double(((a as i64) % (b as i64)) as f64));
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

                // Type-aware comparison
                CoreOp::Eq => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    let result = match (&a, &b) {
                        (StackValue::Double(x), StackValue::Double(y)) => (x - y).abs() < 1e-11,
                        (StackValue::Str(x), StackValue::Str(y)) => x == y,
                        _ => return Err(CalcError::TypeMismatch),
                    };
                    stack.push(StackValue::Double(if result { 1.0 } else { 0.0 }));
                }
                CoreOp::Ne => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    let result = match (&a, &b) {
                        (StackValue::Double(x), StackValue::Double(y)) => (x - y).abs() > 1e-11,
                        (StackValue::Str(x), StackValue::Str(y)) => x != y,
                        _ => return Err(CalcError::TypeMismatch),
                    };
                    stack.push(StackValue::Double(if result { 1.0 } else { 0.0 }));
                }
                CoreOp::Lt => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    let result = match (&a, &b) {
                        (StackValue::Double(x), StackValue::Double(y)) => (y - x) > 1e-11,
                        (StackValue::Str(x), StackValue::Str(y)) => x < y,
                        _ => return Err(CalcError::TypeMismatch),
                    };
                    stack.push(StackValue::Double(if result { 1.0 } else { 0.0 }));
                }
                CoreOp::Le => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    let result = match (&a, &b) {
                        (StackValue::Double(x), StackValue::Double(y)) => {
                            (x - y).abs() < 1e-11 || *x < *y
                        }
                        (StackValue::Str(x), StackValue::Str(y)) => x <= y,
                        _ => return Err(CalcError::TypeMismatch),
                    };
                    stack.push(StackValue::Double(if result { 1.0 } else { 0.0 }));
                }
                CoreOp::Gt => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    let result = match (&a, &b) {
                        (StackValue::Double(x), StackValue::Double(y)) => (x - y) > 1e-11,
                        (StackValue::Str(x), StackValue::Str(y)) => x > y,
                        _ => return Err(CalcError::TypeMismatch),
                    };
                    stack.push(StackValue::Double(if result { 1.0 } else { 0.0 }));
                }
                CoreOp::Ge => {
                    let b = pop1(&mut stack)?;
                    let a = pop1(&mut stack)?;
                    let result = match (&a, &b) {
                        (StackValue::Double(x), StackValue::Double(y)) => {
                            (x - y).abs() < 1e-11 || *x > *y
                        }
                        (StackValue::Str(x), StackValue::Str(y)) => x >= y,
                        _ => return Err(CalcError::TypeMismatch),
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

                // Bitwise
                CoreOp::BitAnd => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    stack.push(StackValue::Double(((a as i64) & (b as i64)) as f64));
                }
                CoreOp::BitOr => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    stack.push(StackValue::Double(((a as i64) | (b as i64)) as f64));
                }
                CoreOp::BitXor => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    stack.push(StackValue::Double(((a as i64) ^ (b as i64)) as f64));
                }
                CoreOp::BitNot => {
                    let a = pop1_f64(&mut stack)?;
                    stack.push(StackValue::Double(!(a as i64) as f64));
                }
                CoreOp::Shl => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    stack.push(StackValue::Double(((a as i64) << (b as i64)) as f64));
                }
                CoreOp::Shr => {
                    let (a, b) = pop2_f64(&mut stack)?;
                    stack.push(StackValue::Double(((a as i64) >> (b as i64)) as f64));
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
                    stack.push(StackValue::Double(a.sqrt()));
                }
                CoreOp::Exp => {
                    let a = pop1_f64(&mut stack)?;
                    stack.push(StackValue::Double(a.exp()));
                }
                CoreOp::Log10 => {
                    let a = pop1_f64(&mut stack)?;
                    stack.push(StackValue::Double(a.log10()));
                }
                CoreOp::LogE => {
                    let a = pop1_f64(&mut stack)?;
                    stack.push(StackValue::Double(a.ln()));
                }
                CoreOp::Log2 => {
                    let a = pop1_f64(&mut stack)?;
                    stack.push(StackValue::Double(a.log2()));
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
                    let rounded = if a >= 0.0 {
                        (a + 0.5) as i64
                    } else {
                        (a - 0.5) as i64
                    };
                    stack.push(StackValue::Double(rounded as f64));
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
                        StackValue::Str(result) => {
                            let mut result = result.clone();
                            for _ in 1..n {
                                let v = pop1(&mut stack)?;
                                let s = v.as_str_ref()?;
                                if s > result.as_str() {
                                    result = s.to_string();
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
                        StackValue::Str(result) => {
                            let mut result = result.clone();
                            for _ in 1..n {
                                let v = pop1(&mut stack)?;
                                let s = v.as_str_ref()?;
                                if s < result.as_str() {
                                    result = s.to_string();
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
                    let v = pop1(&mut stack)?;
                    match v {
                        StackValue::Str(s) => {
                            inputs.str_vars[*idx as usize] = s;
                        }
                        StackValue::Double(d) => {
                            inputs.num_vars[*idx as usize] = d;
                        }
                    }
                }
            },

            Opcode::String(sop) => match sop {
                StringOp::PushString(s) => {
                    stack.push(StackValue::Str(s.clone()));
                }
                StringOp::PushStringVar(idx) => {
                    stack.push(StackValue::Str(inputs.str_vars[*idx as usize].clone()));
                }
                StringOp::StoreStringVar(idx) => {
                    let v = pop1(&mut stack)?;
                    inputs.str_vars[*idx as usize] = v.into_string_value();
                }
                StringOp::ToString => {
                    let v = pop1(&mut stack)?;
                    match v {
                        StackValue::Double(d) => {
                            stack.push(StackValue::Str(format_double(d)));
                        }
                        StackValue::Str(s) => {
                            stack.push(StackValue::Str(s));
                        }
                    }
                }
                StringOp::ToDouble => {
                    let v = pop1(&mut stack)?;
                    match v {
                        StackValue::Str(s) => {
                            let d = s.trim().parse::<f64>().unwrap_or(0.0);
                            stack.push(StackValue::Double(d));
                        }
                        StackValue::Double(d) => {
                            stack.push(StackValue::Double(d));
                        }
                    }
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
                        StackValue::Str(s) => s.bytes().next().map(|b| b as f64).unwrap_or(0.0),
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
                    stack.push(StackValue::Str(translate_escapes(&s)));
                }
                StringOp::Esc => {
                    let v = pop1(&mut stack)?;
                    let s = match v {
                        StackValue::Str(s) => s,
                        StackValue::Double(_) => return Err(CalcError::TypeMismatch),
                    };
                    stack.push(StackValue::Str(escape_string(&s)));
                }
                StringOp::Printf => {
                    // Pop format string, then one value
                    let val = pop1(&mut stack)?;
                    let fmt = pop1(&mut stack)?;
                    let fmt_str = fmt.as_str_ref()?;
                    let result = simple_printf(fmt_str, &val)?;
                    stack.push(StackValue::Str(result));
                }
                StringOp::Sscanf => {
                    // Pop format string, then input string
                    let fmt = pop1(&mut stack)?;
                    let input = pop1(&mut stack)?;
                    let input_str = input.as_str_ref()?;
                    let fmt_str = fmt.as_str_ref()?;
                    let result = simple_sscanf(input_str, fmt_str);
                    stack.push(result);
                }
                StringOp::BinRead => {
                    let v = pop1(&mut stack)?;
                    let s = v.as_str_ref()?;
                    let result = bin_read(s);
                    stack.push(StackValue::Str(result));
                }
                StringOp::BinWrite => {
                    let v = pop1(&mut stack)?;
                    let s = v.as_str_ref()?;
                    let result = bin_write(s);
                    stack.push(StackValue::Str(result));
                }
                StringOp::Crc16 => {
                    let v = pop1(&mut stack)?;
                    let s = v.as_str_ref()?;
                    let crc = super::checksum::crc16(s.as_bytes());
                    stack.push(StackValue::Double(crc as f64));
                }
                StringOp::Crc16Append => {
                    // MODBUS: append CRC16 as two bytes (little-endian)
                    let v = pop1(&mut stack)?;
                    let s = v.as_str_ref()?;
                    let crc = super::checksum::crc16(s.as_bytes());
                    let mut result = s.to_string();
                    result.push((crc & 0xFF) as u8 as char);
                    result.push(((crc >> 8) & 0xFF) as u8 as char);
                    stack.push(StackValue::Str(result));
                }
                StringOp::Lrc => {
                    let v = pop1(&mut stack)?;
                    let s = v.as_str_ref()?;
                    match super::checksum::lrc(s) {
                        Some(lrc_str) => {
                            stack.push(StackValue::Str(lrc_str));
                        }
                        None => return Err(CalcError::InvalidFormat),
                    }
                }
                StringOp::LrcAppend => {
                    // AMODBUS: append LRC hex string
                    let v = pop1(&mut stack)?;
                    let s = v.as_str_ref()?;
                    match super::checksum::lrc(s) {
                        Some(lrc_str) => {
                            let mut result = s.to_string();
                            result.push_str(&lrc_str);
                            stack.push(StackValue::Str(result));
                        }
                        None => return Err(CalcError::InvalidFormat),
                    }
                }
                StringOp::Xor8 => {
                    let v = pop1(&mut stack)?;
                    let s = v.as_str_ref()?;
                    let xor = super::checksum::xor8(s.as_bytes());
                    stack.push(StackValue::Double(xor as f64));
                }
                StringOp::Xor8Append => {
                    // ADD_XOR8: append XOR8 as one byte
                    let v = pop1(&mut stack)?;
                    let s = v.as_str_ref()?;
                    let xor = super::checksum::xor8(s.as_bytes());
                    let mut result = s.to_string();
                    result.push(xor as char);
                    stack.push(StackValue::Str(result));
                }
                StringOp::Subrange => {
                    // Pop: string, start, end
                    let end_val = pop1(&mut stack)?;
                    let start_val = pop1(&mut stack)?;
                    let s = pop1(&mut stack)?;
                    let s = s.as_str_ref()?;
                    let start = start_val.as_f64()? as i64;
                    let end = end_val.as_f64()? as i64;
                    let len = s.len() as i64;
                    let start = start.max(0).min(len) as usize;
                    let end = end.max(0).min(len) as usize;
                    let end = end.max(start);
                    stack.push(StackValue::Str(s[start..end].to_string()));
                }
                StringOp::Replace => {
                    // Pop: string, find, replace
                    let replace_val = pop1(&mut stack)?;
                    let find_val = pop1(&mut stack)?;
                    let s = pop1(&mut stack)?;
                    let s = s.as_str_ref()?;
                    let find = find_val.as_str_ref()?;
                    let replace = replace_val.as_str_ref()?;
                    // Replace first occurrence only
                    let result = if let Some(pos) = s.find(find) {
                        let mut r = s.to_string();
                        r.replace_range(pos..pos + find.len(), replace);
                        r
                    } else {
                        s.to_string()
                    };
                    stack.push(StackValue::Str(result));
                }
                StringOp::SubLast => {
                    // Remove last occurrence of substring
                    let pattern = pop1(&mut stack)?;
                    let s = pop1(&mut stack)?;
                    let s = s.as_str_ref()?;
                    let pat = pattern.as_str_ref()?;
                    let result = if let Some(pos) = s.rfind(pat) {
                        let mut r = s.to_string();
                        r.replace_range(pos..pos + pat.len(), "");
                        r
                    } else {
                        s.to_string()
                    };
                    stack.push(StackValue::Str(result));
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

    Ok(stack.last().cloned().unwrap_or(StackValue::Double(0.0)))
}

const MAX_LOOP_ITERATIONS: usize = 1000;

fn translate_escapes(s: &str) -> String {
    let mut result = String::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'n' => {
                    result.push('\n');
                    i += 2;
                }
                b't' => {
                    result.push('\t');
                    i += 2;
                }
                b'r' => {
                    result.push('\r');
                    i += 2;
                }
                b'\\' => {
                    result.push('\\');
                    i += 2;
                }
                b'x' if i + 3 < bytes.len() => {
                    if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 2]), hex_val(bytes[i + 3])) {
                        result.push(((hi << 4) | lo) as char);
                        i += 4;
                    } else {
                        result.push('\\');
                        i += 1;
                    }
                }
                _ => {
                    result.push('\\');
                    i += 1;
                }
            }
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }
    result
}

fn escape_string(s: &str) -> String {
    let mut result = String::new();
    for b in s.bytes() {
        match b {
            b'\n' => result.push_str("\\n"),
            b'\t' => result.push_str("\\t"),
            b'\r' => result.push_str("\\r"),
            b'\\' => result.push_str("\\\\"),
            0x00..=0x1f | 0x7f..=0xff => {
                result.push_str(&format!("\\x{:02x}", b));
            }
            _ => result.push(b as char),
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

fn simple_printf(fmt: &str, val: &StackValue) -> Result<String, CalcError> {
    // Find first format specifier
    let bytes = fmt.as_bytes();
    let mut i = 0;
    let mut result = String::new();

    while i < bytes.len() {
        if bytes[i] == b'%' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'%' {
                result.push('%');
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
            let fmt_str = std::str::from_utf8(&bytes[spec_start..i]).unwrap();
            match spec {
                b'd' | b'i' => {
                    let v = val.as_f64().unwrap_or(0.0) as i64;
                    result.push_str(&c_format_int(fmt_str, v));
                }
                b'f' | b'e' | b'g' | b'E' | b'G' => {
                    let v = val.as_f64().unwrap_or(0.0);
                    result.push_str(&c_format_float(fmt_str, v));
                }
                b'x' | b'X' | b'o' => {
                    let v = val.as_f64().unwrap_or(0.0) as i64;
                    result.push_str(&c_format_int(fmt_str, v));
                }
                b's' => {
                    let s = match val {
                        StackValue::Str(s) => s.clone(),
                        StackValue::Double(d) => format!("{}", d),
                    };
                    result.push_str(&s);
                }
                _ => return Err(CalcError::InvalidFormat),
            }
            // Append rest of format string literally
            while i < bytes.len() {
                result.push(bytes[i] as char);
                i += 1;
            }
            return Ok(result);
        } else {
            result.push(bytes[i] as char);
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

fn simple_sscanf(input: &str, fmt: &str) -> StackValue {
    let bytes = fmt.as_bytes();
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
            return match spec {
                b'd' | b'i' => {
                    let trimmed = input.trim();
                    match trimmed.parse::<i64>() {
                        Ok(v) => StackValue::Double(v as f64),
                        Err(_) => {
                            // Try parsing leading digits
                            let num_str: String = trimmed
                                .chars()
                                .take_while(|c| c.is_ascii_digit() || *c == '-')
                                .collect();
                            StackValue::Double(num_str.parse::<i64>().unwrap_or(0) as f64)
                        }
                    }
                }
                b'f' | b'e' | b'g' => {
                    let trimmed = input.trim();
                    StackValue::Double(trimmed.parse::<f64>().unwrap_or(0.0))
                }
                b's' => {
                    // Read until whitespace
                    let trimmed = input.trim_start();
                    let word: String = trimmed.chars().take_while(|c| !c.is_whitespace()).collect();
                    StackValue::Str(word)
                }
                _ => StackValue::Double(0.0),
            };
        }
        i += 1;
    }
    StackValue::Double(0.0)
}

fn bin_read(s: &str) -> String {
    // Decode escape sequences in binary data
    translate_escapes(s)
}

fn bin_write(s: &str) -> String {
    // Encode binary data with escape sequences
    escape_string(s)
}

fn format_double(d: f64) -> String {
    if d == d.trunc() && d.is_finite() {
        format!("{}", d as i64)
    } else {
        format!("{}", d)
    }
}

fn pop1(stack: &mut Vec<StackValue>) -> Result<StackValue, CalcError> {
    stack.pop().ok_or(CalcError::Underflow)
}

fn pop1_f64(stack: &mut Vec<StackValue>) -> Result<f64, CalcError> {
    let v = stack.pop().ok_or(CalcError::Underflow)?;
    v.as_f64()
}

fn pop2_f64(stack: &mut Vec<StackValue>) -> Result<(f64, f64), CalcError> {
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
