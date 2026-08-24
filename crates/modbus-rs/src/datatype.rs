//! Modbus PLC data-type conversions.
//!
//! Port of the `modbusDataType_t` enum and the `readPlc*` / `writePlc*`
//! family from `drvModbusAsyn.cpp`. A Modbus register is a 16-bit word; this
//! module converts between a slice of host-order `u16` registers and the
//! scalar/string types EPICS records expect.
//!
//! Multi-register integers and floats come in little-endian (LE) and
//! big-endian (BE) word orders, each with an optional per-register
//! byte-swap (`BS`) variant — covering the wiring quirks of real PLCs.

use crate::error::{ModbusError, ModbusResult};

/// A Modbus data type. The discriminant order matches the C
/// `modbusDataType_t` enum so numeric `drvUser` specifications map identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ModbusDataType {
    /// Signed 16-bit, one register.
    Int16,
    /// Sign-magnitude 16-bit: bit 15 is the sign, bits 0..14 the magnitude.
    Int16Sm,
    /// Unsigned binary-coded decimal, 4 nibbles, one register.
    BcdUnsigned,
    /// Signed BCD: bit 15 is the sign over a 4-nibble magnitude.
    BcdSigned,
    /// Unsigned 16-bit, one register.
    UInt16,
    /// Signed 32-bit, little-endian word order, two registers.
    Int32Le,
    /// Signed 32-bit, little-endian word order, per-register byte swap.
    Int32LeBs,
    /// Signed 32-bit, big-endian word order, two registers.
    Int32Be,
    /// Signed 32-bit, big-endian word order, per-register byte swap.
    Int32BeBs,
    /// Unsigned 32-bit, little-endian word order.
    UInt32Le,
    /// Unsigned 32-bit, little-endian word order, per-register byte swap.
    UInt32LeBs,
    /// Unsigned 32-bit, big-endian word order.
    UInt32Be,
    /// Unsigned 32-bit, big-endian word order, per-register byte swap.
    UInt32BeBs,
    /// Signed 64-bit, little-endian word order, four registers.
    Int64Le,
    /// Signed 64-bit, little-endian word order, per-register byte swap.
    Int64LeBs,
    /// Signed 64-bit, big-endian word order.
    Int64Be,
    /// Signed 64-bit, big-endian word order, per-register byte swap.
    Int64BeBs,
    /// Unsigned 64-bit, little-endian word order.
    UInt64Le,
    /// Unsigned 64-bit, little-endian word order, per-register byte swap.
    UInt64LeBs,
    /// Unsigned 64-bit, big-endian word order.
    UInt64Be,
    /// Unsigned 64-bit, big-endian word order, per-register byte swap.
    UInt64BeBs,
    /// IEEE-754 single precision, little-endian word order, two registers.
    Float32Le,
    /// IEEE-754 single precision, little-endian word order, byte swap.
    Float32LeBs,
    /// IEEE-754 single precision, big-endian word order.
    Float32Be,
    /// IEEE-754 single precision, big-endian word order, byte swap.
    Float32BeBs,
    /// IEEE-754 double precision, little-endian word order, four registers.
    Float64Le,
    /// IEEE-754 double precision, little-endian word order, byte swap.
    Float64LeBs,
    /// IEEE-754 double precision, big-endian word order.
    Float64Be,
    /// IEEE-754 double precision, big-endian word order, byte swap.
    Float64BeBs,
    /// String, one char per register taken from the high byte.
    StringHigh,
    /// String, one char per register taken from the low byte.
    StringLow,
    /// String, two chars per register: high byte then low byte.
    StringHighLow,
    /// String, two chars per register: low byte then high byte.
    StringLowHigh,
    /// NUL-terminated string, one char per register from the high byte.
    ZStringHigh,
    /// NUL-terminated string, one char per register from the low byte.
    ZStringLow,
    /// NUL-terminated string, two chars per register: high then low.
    ZStringHighLow,
    /// NUL-terminated string, two chars per register: low then high.
    ZStringLowHigh,
}

/// All data types, in the C enum order. Equals `MAX_MODBUS_DATA_TYPES`.
pub const ALL_DATA_TYPES: [ModbusDataType; 37] = {
    use ModbusDataType::*;
    [
        Int16,
        Int16Sm,
        BcdUnsigned,
        BcdSigned,
        UInt16,
        Int32Le,
        Int32LeBs,
        Int32Be,
        Int32BeBs,
        UInt32Le,
        UInt32LeBs,
        UInt32Be,
        UInt32BeBs,
        Int64Le,
        Int64LeBs,
        Int64Be,
        Int64BeBs,
        UInt64Le,
        UInt64LeBs,
        UInt64Be,
        UInt64BeBs,
        Float32Le,
        Float32LeBs,
        Float32Be,
        Float32BeBs,
        Float64Le,
        Float64LeBs,
        Float64Be,
        Float64BeBs,
        StringHigh,
        StringLow,
        StringHighLow,
        StringLowHigh,
        ZStringHigh,
        ZStringLow,
        ZStringHighLow,
        ZStringLowHigh,
    ]
};

impl ModbusDataType {
    /// The `drvUser` string for this type (e.g. `"INT32_LE_BS"`), matching the
    /// `MODBUS_*_STRING` defines in `drvModbusAsyn.h`.
    pub fn as_str(self) -> &'static str {
        use ModbusDataType::*;
        match self {
            Int16 => "INT16",
            Int16Sm => "INT16SM",
            BcdUnsigned => "BCD_UNSIGNED",
            BcdSigned => "BCD_SIGNED",
            UInt16 => "UINT16",
            Int32Le => "INT32_LE",
            Int32LeBs => "INT32_LE_BS",
            Int32Be => "INT32_BE",
            Int32BeBs => "INT32_BE_BS",
            UInt32Le => "UINT32_LE",
            UInt32LeBs => "UINT32_LE_BS",
            UInt32Be => "UINT32_BE",
            UInt32BeBs => "UINT32_BE_BS",
            Int64Le => "INT64_LE",
            Int64LeBs => "INT64_LE_BS",
            Int64Be => "INT64_BE",
            Int64BeBs => "INT64_BE_BS",
            UInt64Le => "UINT64_LE",
            UInt64LeBs => "UINT64_LE_BS",
            UInt64Be => "UINT64_BE",
            UInt64BeBs => "UINT64_BE_BS",
            Float32Le => "FLOAT32_LE",
            Float32LeBs => "FLOAT32_LE_BS",
            Float32Be => "FLOAT32_BE",
            Float32BeBs => "FLOAT32_BE_BS",
            Float64Le => "FLOAT64_LE",
            Float64LeBs => "FLOAT64_LE_BS",
            Float64Be => "FLOAT64_BE",
            Float64BeBs => "FLOAT64_BE_BS",
            StringHigh => "STRING_HIGH",
            StringLow => "STRING_LOW",
            StringHighLow => "STRING_HIGH_LOW",
            StringLowHigh => "STRING_LOW_HIGH",
            ZStringHigh => "ZSTRING_HIGH",
            ZStringLow => "ZSTRING_LOW",
            ZStringHighLow => "ZSTRING_HIGH_LOW",
            ZStringLowHigh => "ZSTRING_LOW_HIGH",
        }
    }

    /// Parse a `drvUser` data-type string (case-insensitive), as
    /// `drvModbusAsynConfigure` does.
    pub fn from_type_string(s: &str) -> Option<Self> {
        ALL_DATA_TYPES
            .into_iter()
            .find(|dt| dt.as_str().eq_ignore_ascii_case(s))
    }

    /// Decode the numeric `modbusDataType_t` form.
    pub fn from_i32(v: i32) -> Option<Self> {
        ALL_DATA_TYPES.get(usize::try_from(v).ok()?).copied()
    }

    /// Number of 16-bit registers a scalar value of this type occupies. For
    /// string types this is 0 — strings span a caller-determined length.
    pub fn register_count(self) -> usize {
        use ModbusDataType::*;
        match self {
            Int16 | Int16Sm | BcdUnsigned | BcdSigned | UInt16 => 1,
            Int32Le | Int32LeBs | Int32Be | Int32BeBs | UInt32Le | UInt32LeBs | UInt32Be
            | UInt32BeBs | Float32Le | Float32LeBs | Float32Be | Float32BeBs => 2,
            Int64Le | Int64LeBs | Int64Be | Int64BeBs | UInt64Le | UInt64LeBs | UInt64Be
            | UInt64BeBs | Float64Le | Float64LeBs | Float64Be | Float64BeBs => 4,
            StringHigh | StringLow | StringHighLow | StringLowHigh | ZStringHigh | ZStringLow
            | ZStringHighLow | ZStringLowHigh => 0,
        }
    }

    /// Whether this type is one of the string encodings.
    pub fn is_string(self) -> bool {
        use ModbusDataType::*;
        matches!(
            self,
            StringHigh
                | StringLow
                | StringHighLow
                | StringLowHigh
                | ZStringHigh
                | ZStringLow
                | ZStringHighLow
                | ZStringLowHigh
        )
    }

    /// Whether this string type is NUL-terminated (`ZSTRING_*`). Mirrors
    /// `drvModbusAsyn::isZeroTerminatedString`.
    pub fn is_zero_terminated_string(self) -> bool {
        use ModbusDataType::*;
        matches!(
            self,
            ZStringHigh | ZStringLow | ZStringHighLow | ZStringLowHigh
        )
    }
}

/// Word order for a multi-register integer or float.
#[derive(Clone, Copy)]
enum WordOrder {
    /// First register holds the least-significant word.
    Little,
    /// First register holds the most-significant word.
    Big,
}

/// Combine `n` host-order registers into a `u64`, applying word order and an
/// optional per-register byte swap.
fn combine(regs: &[u16], order: WordOrder, byte_swap: bool) -> u64 {
    let mut acc: u64 = 0;
    for (i, &r) in regs.iter().enumerate() {
        let word = if byte_swap { r.swap_bytes() } else { r } as u64;
        let shift = match order {
            WordOrder::Little => 16 * i,
            WordOrder::Big => 16 * (regs.len() - 1 - i),
        };
        acc |= word << shift;
    }
    acc
}

/// Split a `u64` into `n` host-order registers, applying word order and an
/// optional per-register byte swap.
fn split(value: u64, n: usize, order: WordOrder, byte_swap: bool) -> Vec<u16> {
    (0..n)
        .map(|i| {
            let shift = match order {
                WordOrder::Little => 16 * i,
                WordOrder::Big => 16 * (n - 1 - i),
            };
            let word = ((value >> shift) & 0xFFFF) as u16;
            if byte_swap { word.swap_bytes() } else { word }
        })
        .collect()
}

/// Decode the word-order / byte-swap parameters of a 32/64-bit type.
fn order_of(dt: ModbusDataType) -> Option<(WordOrder, bool)> {
    use ModbusDataType::*;
    Some(match dt {
        Int32Le | UInt32Le | Int64Le | UInt64Le | Float32Le | Float64Le => {
            (WordOrder::Little, false)
        }
        Int32LeBs | UInt32LeBs | Int64LeBs | UInt64LeBs | Float32LeBs | Float64LeBs => {
            (WordOrder::Little, true)
        }
        Int32Be | UInt32Be | Int64Be | UInt64Be | Float32Be | Float64Be => (WordOrder::Big, false),
        Int32BeBs | UInt32BeBs | Int64BeBs | UInt64BeBs | Float32BeBs | Float64BeBs => {
            (WordOrder::Big, true)
        }
        _ => return None,
    })
}

/// Ensure `regs` holds at least `need` registers.
fn need_regs(regs: &[u16], need: usize) -> ModbusResult<()> {
    if regs.len() < need {
        Err(ModbusError::FrameTooShort {
            got: regs.len() * 2,
            need: need * 2,
        })
    } else {
        Ok(())
    }
}

/// Read a value of `dt` from `regs` as a signed 64-bit integer.
///
/// Returns `(value, registers_consumed)`. Port of `readPlcInt64`; the
/// `readPlcInt32` form is [`read_int32`]. For float types the register is
/// decoded as a float and truncated to `i32` before sign-extension. This
/// matches C `(epicsInt32)fValue` (drvModbusAsyn.cpp:2572) for every in-range
/// value; at the boundary it deliberately differs — Rust's `as` cast
/// saturates (NaN→0, |v|≥2^31→i32::MIN/MAX) where C's cast yields x86's
/// INT_MIN "integer indefinite". The well-defined saturating result is kept
/// rather than copying C's UB-adjacent boundary value.
pub fn read_int64(dt: ModbusDataType, regs: &[u16]) -> ModbusResult<(i64, usize)> {
    use ModbusDataType::*;
    match dt {
        UInt16 => {
            need_regs(regs, 1)?;
            Ok((regs[0] as i64, 1))
        }
        Int16 => {
            need_regs(regs, 1)?;
            Ok((regs[0] as i16 as i64, 1))
        }
        Int16Sm => {
            need_regs(regs, 1)?;
            let v = regs[0];
            let result = if v & 0x8000 != 0 {
                -((v & 0x7FFF) as i16 as i64)
            } else {
                v as i64
            };
            Ok((result, 1))
        }
        BcdUnsigned | BcdSigned => {
            need_regs(regs, 1)?;
            let mut v = regs[0];
            let negative = dt == BcdSigned && (v & 0x8000) != 0;
            if negative {
                v &= 0x7FFF;
            }
            let mut result: i64 = 0;
            let mut mult: i64 = 1;
            for _ in 0..4 {
                result += (v & 0xF) as i64 * mult;
                mult *= 10;
                v >>= 4;
            }
            Ok((if negative { -result } else { result }, 1))
        }
        Int32Le | Int32LeBs | Int32Be | Int32BeBs => {
            need_regs(regs, 2)?;
            let (order, bs) = order_of(dt).unwrap();
            let raw = combine(&regs[..2], order, bs) as u32;
            Ok((raw as i32 as i64, 2))
        }
        UInt32Le | UInt32LeBs | UInt32Be | UInt32BeBs => {
            need_regs(regs, 2)?;
            let (order, bs) = order_of(dt).unwrap();
            let raw = combine(&regs[..2], order, bs) as u32;
            Ok((raw as i64, 2))
        }
        Int64Le | Int64LeBs | Int64Be | Int64BeBs => {
            need_regs(regs, 4)?;
            let (order, bs) = order_of(dt).unwrap();
            Ok((combine(&regs[..4], order, bs) as i64, 4))
        }
        UInt64Le | UInt64LeBs | UInt64Be | UInt64BeBs => {
            need_regs(regs, 4)?;
            let (order, bs) = order_of(dt).unwrap();
            // C reinterprets the same u64 bits; the i64 sign is the caller's.
            Ok((combine(&regs[..4], order, bs) as i64, 4))
        }
        Float32Le | Float32LeBs | Float32Be | Float32BeBs | Float64Le | Float64LeBs | Float64Be
        | Float64BeBs => {
            let (f, words) = read_float(dt, regs)?;
            // C: i64Result = (epicsInt32)fValue (drvModbusAsyn.cpp:2572).
            // `as i32` saturates at the boundary (see read_int64) rather than
            // reproducing C's x86 INT_MIN; in-range results are identical.
            Ok((f as i32 as i64, words))
        }
        StringHigh | StringLow | StringHighLow | StringLowHigh | ZStringHigh | ZStringLow
        | ZStringHighLow | ZStringLowHigh => Err(ModbusError::InvalidFunction(0)),
    }
}

/// Read a value of `dt` from `regs` as a signed 32-bit integer (the 64-bit
/// result truncated). Port of `readPlcInt32`.
pub fn read_int32(dt: ModbusDataType, regs: &[u16]) -> ModbusResult<(i32, usize)> {
    let (v, words) = read_int64(dt, regs)?;
    Ok((v as i32, words))
}

/// Read a value of `dt` from `regs` as an `f64`. Port of `readPlcFloat`.
pub fn read_float(dt: ModbusDataType, regs: &[u16]) -> ModbusResult<(f64, usize)> {
    use ModbusDataType::*;
    match dt {
        // 16-bit and signed-32-bit integer types: via the i32 path.
        UInt16 | Int16Sm | BcdSigned | BcdUnsigned | Int16 | Int32Le | Int32LeBs | Int32Be
        | Int32BeBs => {
            let (v, words) = read_int32(dt, regs)?;
            Ok((v as f64, words))
        }
        UInt32Le | UInt32LeBs | UInt32Be | UInt32BeBs => {
            let (v, words) = read_int64(dt, regs)?;
            Ok((v as u32 as f64, words))
        }
        Int64Le | Int64LeBs | Int64Be | Int64BeBs => {
            let (v, words) = read_int64(dt, regs)?;
            Ok((v as f64, words))
        }
        UInt64Le | UInt64LeBs | UInt64Be | UInt64BeBs => {
            let (v, words) = read_int64(dt, regs)?;
            Ok((v as u64 as f64, words))
        }
        Float32Le | Float32LeBs | Float32Be | Float32BeBs => {
            need_regs(regs, 2)?;
            let (order, bs) = order_of(dt).unwrap();
            let bits = combine(&regs[..2], order, bs) as u32;
            Ok((f32::from_bits(bits) as f64, 2))
        }
        Float64Le | Float64LeBs | Float64Be | Float64BeBs => {
            need_regs(regs, 4)?;
            let (order, bs) = order_of(dt).unwrap();
            let bits = combine(&regs[..4], order, bs);
            Ok((f64::from_bits(bits), 4))
        }
        StringHigh | StringLow | StringHighLow | StringLowHigh | ZStringHigh | ZStringLow
        | ZStringHighLow | ZStringLowHigh => Err(ModbusError::InvalidFunction(0)),
    }
}

/// C's `-(epicsInt16)value`, integer promotion included.
///
/// The cast narrows to 16 bits, then C promotes the result to `int` *before*
/// the unary minus runs, so `-32768` negates to `+32768` instead of
/// overflowing its own type (`drvModbusAsyn.cpp:2622` for `dataTypeInt16SM`,
/// `:2631` for `dataTypeBCDSigned`). Negating in `i16` is what made a
/// `-32768` put panic in an overflow-checked build and run the BCD digit loop
/// on a wrapped negative magnitude in release. Both write arms take the width
/// from here so neither can drift back to the narrow one.
fn negate_promoted_int16(value: i64) -> i32 {
    -(value as i16 as i32)
}

/// Encode a signed 64-bit value into Modbus registers for `dt`.
///
/// Returns the registers (1, 2, or 4 of them). Port of `writePlcInt64`; the
/// `writePlcInt32` form is [`write_int32`].
pub fn write_int64(dt: ModbusDataType, value: i64) -> ModbusResult<Vec<u16>> {
    use ModbusDataType::*;
    match dt {
        UInt16 => Ok(vec![value as u16]),
        Int16 => Ok(vec![value as i16 as u16]),
        Int16Sm => {
            let mut v = value as u16;
            if v & 0x8000 != 0 {
                // C truncates the promoted `int` back into `epicsUInt16`
                // (:2622-2623), which is what keeps 0x8000 at 0x8000.
                v = negate_promoted_int16(v as i64) as u16;
                v |= 0x8000;
            }
            Ok(vec![v])
        }
        BcdUnsigned | BcdSigned => {
            let mut magnitude = value;
            let negative = dt == BcdSigned && (value as i16) < 0;
            if negative {
                magnitude = negate_promoted_int16(value) as i64;
            }
            let mut out: u16 = 0;
            let mut div: i64 = 1000;
            for _ in 0..4 {
                out <<= 4;
                let digit = (magnitude / div) as u16;
                out |= digit & 0xF;
                magnitude -= (digit as i64) * div;
                div /= 10;
            }
            if negative {
                out |= 0x8000;
            }
            Ok(vec![out])
        }
        Int32Le | Int32LeBs | Int32Be | Int32BeBs => {
            let (order, bs) = order_of(dt).unwrap();
            Ok(split(value as i32 as u32 as u64, 2, order, bs))
        }
        UInt32Le | UInt32LeBs | UInt32Be | UInt32BeBs => {
            let (order, bs) = order_of(dt).unwrap();
            Ok(split(value as u32 as u64, 2, order, bs))
        }
        Int64Le | Int64LeBs | Int64Be | Int64BeBs | UInt64Le | UInt64LeBs | UInt64Be
        | UInt64BeBs => {
            let (order, bs) = order_of(dt).unwrap();
            Ok(split(value as u64, 4, order, bs))
        }
        Float32Le | Float32LeBs | Float32Be | Float32BeBs | Float64Le | Float64LeBs | Float64Be
        | Float64BeBs => write_float(dt, value as f64),
        StringHigh | StringLow | StringHighLow | StringLowHigh | ZStringHigh | ZStringLow
        | ZStringHighLow | ZStringLowHigh => Err(ModbusError::InvalidFunction(0)),
    }
}

/// Encode a signed 32-bit value into Modbus registers for `dt`. Port of
/// `writePlcInt32` (which widens to 64-bit and delegates).
pub fn write_int32(dt: ModbusDataType, value: i32) -> ModbusResult<Vec<u16>> {
    write_int64(dt, value as i64)
}

/// Encode an `f64` value into Modbus registers for `dt`. Port of
/// `writePlcFloat`.
pub fn write_float(dt: ModbusDataType, value: f64) -> ModbusResult<Vec<u16>> {
    use ModbusDataType::*;
    match dt {
        // Integer types: round-trip through the integer writer. C writePlcFloat
        // casts `(epicsInt64)value`; the Rust `as i64`/`as u64` matches in
        // range and saturates at the boundary instead of C's UB cast (see
        // read_int64).
        UInt16 | Int16Sm | BcdSigned | BcdUnsigned | Int16 | Int32Le | Int32LeBs | Int32Be
        | Int32BeBs | Int64Le | Int64LeBs | Int64Be | Int64BeBs => write_int64(dt, value as i64),
        UInt32Le | UInt32LeBs | UInt32Be | UInt32BeBs | UInt64Le | UInt64LeBs | UInt64Be
        | UInt64BeBs => write_int64(dt, value as u64 as i64),
        Float32Le | Float32LeBs | Float32Be | Float32BeBs => {
            let (order, bs) = order_of(dt).unwrap();
            Ok(split((value as f32).to_bits() as u64, 2, order, bs))
        }
        Float64Le | Float64LeBs | Float64Be | Float64BeBs => {
            let (order, bs) = order_of(dt).unwrap();
            Ok(split(value.to_bits(), 4, order, bs))
        }
        StringHigh | StringLow | StringHighLow | StringLowHigh | ZStringHigh | ZStringLow
        | ZStringHighLow | ZStringLowHigh => Err(ModbusError::InvalidFunction(0)),
    }
}

/// Decode a string of up to `max_chars` characters from `regs`.
///
/// Returns `(bytes, registers_consumed)`. The bytes stop at the first NUL
/// (`strlen` semantics, as in `readPlcString`). The number of registers
/// consumed equals the number of loop iterations the C code performs.
pub fn read_string(
    dt: ModbusDataType,
    regs: &[u16],
    max_chars: usize,
) -> ModbusResult<(Vec<u8>, usize)> {
    use ModbusDataType::*;
    if !dt.is_string() {
        return Err(ModbusError::InvalidFunction(0));
    }
    let mut chars: Vec<u8> = Vec::with_capacity(max_chars);
    let mut reg_idx = 0usize;
    while chars.len() < max_chars && reg_idx < regs.len() {
        let reg = regs[reg_idx];
        let hi = (reg >> 8) as u8;
        let lo = (reg & 0xFF) as u8;
        match dt {
            StringHigh | ZStringHigh => chars.push(hi),
            StringLow | ZStringLow => chars.push(lo),
            StringHighLow | ZStringHighLow => {
                chars.push(hi);
                if chars.len() < max_chars {
                    chars.push(lo);
                }
            }
            StringLowHigh | ZStringLowHigh => {
                chars.push(lo);
                if chars.len() < max_chars {
                    chars.push(hi);
                }
            }
            _ => unreachable!("guarded by is_string"),
        }
        reg_idx += 1;
    }
    // Truncate at the first NUL (C uses strlen on the result).
    if let Some(nul) = chars.iter().position(|&c| c == 0) {
        chars.truncate(nul);
    }
    Ok((chars, reg_idx))
}

/// Encode a string into Modbus registers for `dt`.
///
/// Writes up to `max_regs` registers; characters beyond that are dropped, as
/// the C `writePlcString` loop stops at `modbusLength_`. Returns
/// `(registers, chars_consumed)`.
pub fn write_string(
    dt: ModbusDataType,
    data: &[u8],
    max_regs: usize,
) -> ModbusResult<(Vec<u16>, usize)> {
    use ModbusDataType::*;
    if !dt.is_string() {
        return Err(ModbusError::InvalidFunction(0));
    }
    let mut regs: Vec<u16> = Vec::with_capacity(max_regs);
    let mut char_idx = 0usize;
    while regs.len() < max_regs && char_idx < data.len() {
        let c0 = data[char_idx] as u16;
        let reg = match dt {
            StringHigh | ZStringHigh => (c0 << 8) & 0xFF00,
            StringLow | ZStringLow => c0 & 0x00FF,
            StringHighLow | ZStringHighLow => {
                let mut r = (c0 << 8) & 0xFF00;
                if char_idx + 1 < data.len() {
                    char_idx += 1;
                    r |= data[char_idx] as u16 & 0x00FF;
                }
                r
            }
            StringLowHigh | ZStringLowHigh => {
                let mut r = c0 & 0x00FF;
                if char_idx + 1 < data.len() {
                    char_idx += 1;
                    r |= (data[char_idx] as u16) << 8 & 0xFF00;
                }
                r
            }
            _ => unreachable!("guarded by is_string"),
        };
        regs.push(reg);
        char_idx += 1;
    }
    Ok((regs, char_idx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_table_round_trips() {
        for (i, dt) in ALL_DATA_TYPES.into_iter().enumerate() {
            assert_eq!(ModbusDataType::from_i32(i as i32), Some(dt));
            assert_eq!(ModbusDataType::from_type_string(dt.as_str()), Some(dt));
        }
        assert_eq!(ALL_DATA_TYPES.len(), 37);
        assert_eq!(
            ModbusDataType::from_type_string("int32_le_bs"),
            Some(ModbusDataType::Int32LeBs)
        );
        assert_eq!(ModbusDataType::from_i32(37), None);
    }

    #[test]
    fn int16_signed_and_unsigned() {
        assert_eq!(
            read_int64(ModbusDataType::Int16, &[0xFFFF]).unwrap(),
            (-1, 1)
        );
        assert_eq!(
            read_int64(ModbusDataType::UInt16, &[0xFFFF]).unwrap(),
            (65535, 1)
        );
        assert_eq!(
            read_int64(ModbusDataType::Int16, &[0x7FFF]).unwrap(),
            (32767, 1)
        );
    }

    #[test]
    fn int16_sign_magnitude() {
        // 0x8005 = sign + magnitude 5 → -5.
        assert_eq!(
            read_int64(ModbusDataType::Int16Sm, &[0x8005]).unwrap().0,
            -5
        );
        assert_eq!(read_int64(ModbusDataType::Int16Sm, &[0x0005]).unwrap().0, 5);
        // write -5 → 0x8005, write 5 → 0x0005.
        assert_eq!(
            write_int64(ModbusDataType::Int16Sm, -5).unwrap(),
            vec![0x8005]
        );
        assert_eq!(
            write_int64(ModbusDataType::Int16Sm, 5).unwrap(),
            vec![0x0005]
        );
    }

    #[test]
    fn bcd_unsigned_round_trip() {
        // 0x1234 BCD = decimal 1234.
        assert_eq!(
            read_int64(ModbusDataType::BcdUnsigned, &[0x1234])
                .unwrap()
                .0,
            1234
        );
        assert_eq!(
            write_int64(ModbusDataType::BcdUnsigned, 1234).unwrap(),
            vec![0x1234]
        );
        assert_eq!(
            write_int64(ModbusDataType::BcdUnsigned, 9999).unwrap(),
            vec![0x9999]
        );
    }

    #[test]
    fn bcd_signed_negative() {
        // -42 → magnitude 0042 BCD with sign bit = 0x8042.
        assert_eq!(
            write_int64(ModbusDataType::BcdSigned, -42).unwrap(),
            vec![0x8042]
        );
        assert_eq!(
            read_int64(ModbusDataType::BcdSigned, &[0x8042]).unwrap().0,
            -42
        );
        assert_eq!(
            read_int64(ModbusDataType::BcdSigned, &[0x0042]).unwrap().0,
            42
        );
    }

    /// MB-1 boundary: `-32768` is the one input whose magnitude does not fit
    /// the type C casts to. C negates *after* integer promotion
    /// (`drvModbusAsyn.cpp:2631`), so `+32768` survives into `epicsInt64` and
    /// the digit loop runs on a positive magnitude. Negating in `i16`
    /// panicked the IOC in an overflow-checked build and, in release, wrapped
    /// back to `-32768` and put `0x89A8` on the wire.
    #[test]
    fn bcd_signed_negates_int16_min_the_way_c_promotes() {
        assert_eq!(
            write_int64(ModbusDataType::BcdSigned, -32768).unwrap(),
            vec![0x8768]
        );
    }

    /// The same boundary on the sign-magnitude arm (`drvModbusAsyn.cpp:2622`).
    /// C truncates the promoted `int` back into `epicsUInt16`, so the register
    /// is `0x8000`; only the panic diverged.
    #[test]
    fn int16_sm_negates_int16_min_the_way_c_promotes() {
        assert_eq!(
            write_int64(ModbusDataType::Int16Sm, -32768).unwrap(),
            vec![0x8000]
        );
    }

    /// The rest of the negative range, pinned so widening the negation cannot
    /// pay for the boundary with the values C and the port already agreed on.
    #[test]
    fn int16_negation_neighbours_are_unchanged() {
        for (value, bcd, sm) in [
            (-1i64, 0x8001u16, 0x8001u16),
            (-1234, 0x9234, 0x84D2),
            (-9999, 0x9999, 0xA70F),
            (-32767, 0x8767, 0xFFFF),
        ] {
            assert_eq!(
                write_int64(ModbusDataType::BcdSigned, value).unwrap(),
                vec![bcd],
                "BCD_SIGNED {value}"
            );
            assert_eq!(
                write_int64(ModbusDataType::Int16Sm, value).unwrap(),
                vec![sm],
                "INT16SM {value}"
            );
        }
    }

    #[test]
    fn int32_le_be_word_order() {
        // value 0x12345678: LE → [0x5678, 0x1234], BE → [0x1234, 0x5678].
        assert_eq!(
            write_int32(ModbusDataType::Int32Le, 0x12345678).unwrap(),
            vec![0x5678, 0x1234]
        );
        assert_eq!(
            write_int32(ModbusDataType::Int32Be, 0x12345678).unwrap(),
            vec![0x1234, 0x5678]
        );
        assert_eq!(
            read_int32(ModbusDataType::Int32Le, &[0x5678, 0x1234]).unwrap(),
            (0x12345678, 2)
        );
        assert_eq!(
            read_int32(ModbusDataType::Int32Be, &[0x1234, 0x5678]).unwrap(),
            (0x12345678, 2)
        );
    }

    #[test]
    fn int32_byte_swap_variant() {
        // LE_BS: each register's bytes swapped relative to LE.
        let le = write_int32(ModbusDataType::Int32Le, 0x12345678).unwrap();
        let le_bs = write_int32(ModbusDataType::Int32LeBs, 0x12345678).unwrap();
        assert_eq!(le_bs, vec![le[0].swap_bytes(), le[1].swap_bytes()]);
        assert_eq!(
            read_int32(ModbusDataType::Int32LeBs, &le_bs).unwrap().0,
            0x12345678
        );
    }

    #[test]
    fn uint32_full_range() {
        assert_eq!(
            read_int64(ModbusDataType::UInt32Le, &[0xFFFF, 0xFFFF])
                .unwrap()
                .0,
            0xFFFF_FFFF_i64
        );
        // Same bits read as signed Int32 is -1.
        assert_eq!(
            read_int64(ModbusDataType::Int32Le, &[0xFFFF, 0xFFFF])
                .unwrap()
                .0,
            -1
        );
    }

    #[test]
    fn int64_round_trip_all_orders() {
        let v: i64 = 0x0123_4567_89AB_CDEF;
        for dt in [
            ModbusDataType::Int64Le,
            ModbusDataType::Int64LeBs,
            ModbusDataType::Int64Be,
            ModbusDataType::Int64BeBs,
        ] {
            let regs = write_int64(dt, v).unwrap();
            assert_eq!(regs.len(), 4);
            assert_eq!(read_int64(dt, &regs).unwrap(), (v, 4));
        }
    }

    #[test]
    fn float32_round_trip() {
        for dt in [
            ModbusDataType::Float32Le,
            ModbusDataType::Float32LeBs,
            ModbusDataType::Float32Be,
            ModbusDataType::Float32BeBs,
        ] {
            let regs = write_float(dt, 3.5).unwrap();
            assert_eq!(regs.len(), 2);
            assert_eq!(read_float(dt, &regs).unwrap(), (3.5, 2));
        }
    }

    #[test]
    fn float64_round_trip() {
        for dt in [
            ModbusDataType::Float64Le,
            ModbusDataType::Float64LeBs,
            ModbusDataType::Float64Be,
            ModbusDataType::Float64BeBs,
        ] {
            let regs = write_float(dt, -1234.5678).unwrap();
            assert_eq!(regs.len(), 4);
            assert_eq!(read_float(dt, &regs).unwrap(), (-1234.5678, 4));
        }
    }

    #[test]
    fn float_word_order_distinct() {
        let le = write_float(ModbusDataType::Float32Le, 1.0).unwrap();
        let be = write_float(ModbusDataType::Float32Be, 1.0).unwrap();
        assert_eq!(le, vec![be[1], be[0]]);
    }

    #[test]
    fn read_int_from_float_register_truncates() {
        // 3.9 stored as Float32LE, read as an integer → truncated to 3.
        let regs = write_float(ModbusDataType::Float32Le, 3.9).unwrap();
        assert_eq!(read_int64(ModbusDataType::Float32Le, &regs).unwrap().0, 3);
    }

    #[test]
    fn read_float_from_int_register() {
        let regs = write_int64(ModbusDataType::Int16, -7).unwrap();
        assert_eq!(read_float(ModbusDataType::Int16, &regs).unwrap().0, -7.0);
    }

    #[test]
    fn string_high_byte_round_trip() {
        // "Hi" packed one char per register, high byte.
        let regs = write_string(ModbusDataType::StringHigh, b"Hi", 8)
            .unwrap()
            .0;
        assert_eq!(regs, vec![(b'H' as u16) << 8, (b'i' as u16) << 8]);
        let (s, words) = read_string(ModbusDataType::StringHigh, &regs, 8).unwrap();
        assert_eq!(s, b"Hi");
        assert_eq!(words, 2);
    }

    #[test]
    fn string_high_low_packs_two_chars_per_register() {
        let (regs, consumed) = write_string(ModbusDataType::StringHighLow, b"ABCD", 8).unwrap();
        assert_eq!(consumed, 4);
        assert_eq!(
            regs,
            vec![
                ((b'A' as u16) << 8) | b'B' as u16,
                ((b'C' as u16) << 8) | b'D' as u16,
            ]
        );
        let (s, _) = read_string(ModbusDataType::StringHighLow, &regs, 8).unwrap();
        assert_eq!(s, b"ABCD");
    }

    #[test]
    fn string_low_high_packs_two_chars_per_register() {
        let (regs, _) = write_string(ModbusDataType::StringLowHigh, b"AB", 8).unwrap();
        assert_eq!(regs, vec![(b'A' as u16) | ((b'B' as u16) << 8)]);
        let (s, _) = read_string(ModbusDataType::StringLowHigh, &regs, 8).unwrap();
        assert_eq!(s, b"AB");
    }

    #[test]
    fn read_string_stops_at_nul() {
        let regs = [
            (b'O' as u16) << 8,
            (b'K' as u16) << 8,
            0x0000,
            (b'X' as u16) << 8,
        ];
        let (s, _) = read_string(ModbusDataType::ZStringHigh, &regs, 16).unwrap();
        assert_eq!(s, b"OK");
    }

    #[test]
    fn read_string_honours_max_chars() {
        let regs = [(b'A' as u16) << 8, (b'B' as u16) << 8, (b'C' as u16) << 8];
        let (s, words) = read_string(ModbusDataType::StringHigh, &regs, 2).unwrap();
        assert_eq!(s, b"AB");
        assert_eq!(words, 2);
    }

    #[test]
    fn write_string_honours_max_regs() {
        // Only 1 register of room for a 2-char-per-register type.
        let (regs, consumed) = write_string(ModbusDataType::StringHighLow, b"ABCD", 1).unwrap();
        assert_eq!(regs.len(), 1);
        assert_eq!(consumed, 2);
    }

    #[test]
    fn zero_terminated_classification() {
        assert!(ModbusDataType::ZStringHigh.is_zero_terminated_string());
        assert!(!ModbusDataType::StringHigh.is_zero_terminated_string());
        assert!(ModbusDataType::StringHigh.is_string());
        assert!(!ModbusDataType::Int16.is_string());
    }

    #[test]
    fn register_count_matches_type_width() {
        assert_eq!(ModbusDataType::Int16.register_count(), 1);
        assert_eq!(ModbusDataType::Int32Be.register_count(), 2);
        assert_eq!(ModbusDataType::Float64Le.register_count(), 4);
        assert_eq!(ModbusDataType::StringHigh.register_count(), 0);
    }

    #[test]
    fn short_register_slice_is_rejected() {
        assert!(matches!(
            read_int64(ModbusDataType::Int32Le, &[0x1234]),
            Err(ModbusError::FrameTooShort { .. })
        ));
        assert!(read_int64(ModbusDataType::Int16, &[]).is_err());
    }
}
