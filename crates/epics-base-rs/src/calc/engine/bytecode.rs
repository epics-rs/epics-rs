//! C's postfix byte stream — the one encoding a compiled program has.
//!
//! Each C compiler writes one byte per opcode into the record's `RPCL` buffer
//! and the matching `*Perform` walks it back: `LITERAL_DOUBLE` is followed by
//! the 8 bytes of a `double`, `LITERAL_INT` by the 4 of an `int`, a vararg
//! operator by its argument count, and sCalc's `LITERAL_STRING` by the string
//! and its NUL (`postfix.c:259-290`, `sCalcPostfix.c:803-812`). sCalc also
//! writes a `NO_STRING`/`USES_STRING` head byte before the first opcode
//! (`sCalcPostfix.c:438`). The three dialects number their opcodes
//! differently; [`BASE`], [`SCALC`] and [`ACALC`] are the three C enums,
//! transcribed in order.
//!
//! [`Emitter`] is the only writer and [`decode`] the only reader: an
//! instruction's length is known in exactly one place.

use super::ExprKind;
use super::error::CalcError;
use super::opcodes::{ArrayOp, ControlOp, CoreOp, Opcode, StringOp};

/// `END_EXPRESSION`, `LITERAL_DOUBLE` and `LITERAL_INT` open all three enums.
pub(crate) const END: u8 = 0;
pub(crate) const LITERAL_DOUBLE: u8 = 1;
pub(crate) const LITERAL_INT: u8 = 2;

/// base `rpn_opcode` (`postfixPvt.h`) by value, for `calcPerform`'s switch,
/// which dispatches on the byte as C does. [`BASE`] is built from the same
/// enum by position; `base_constants_name_the_base_table` pins the two.
pub(crate) mod base {
    pub(crate) use super::{END, LITERAL_DOUBLE, LITERAL_INT};
    pub(crate) const FETCH_VAL: u8 = 3;
    pub(crate) const FETCH_A: u8 = 4;
    pub(crate) const FETCH_U: u8 = 24;
    pub(crate) const STORE_A: u8 = 25;
    pub(crate) const STORE_U: u8 = 45;
    pub(crate) const CONST_PI: u8 = 46;
    pub(crate) const CONST_D2R: u8 = 47;
    pub(crate) const CONST_R2D: u8 = 48;
    pub(crate) const UNARY_NEG: u8 = 49;
    pub(crate) const ADD: u8 = 50;
    pub(crate) const SUB: u8 = 51;
    pub(crate) const MULT: u8 = 52;
    pub(crate) const DIV: u8 = 53;
    pub(crate) const MODULO: u8 = 54;
    pub(crate) const POWER: u8 = 55;
    pub(crate) const ABS_VAL: u8 = 56;
    pub(crate) const EXP: u8 = 57;
    pub(crate) const LOG_10: u8 = 58;
    pub(crate) const LOG_E: u8 = 59;
    pub(crate) const MAX: u8 = 60;
    pub(crate) const MIN: u8 = 61;
    pub(crate) const SQU_RT: u8 = 62;
    pub(crate) const ACOS: u8 = 63;
    pub(crate) const ASIN: u8 = 64;
    pub(crate) const ATAN: u8 = 65;
    pub(crate) const ATAN2: u8 = 66;
    pub(crate) const COS: u8 = 67;
    pub(crate) const COSH: u8 = 68;
    pub(crate) const SIN: u8 = 69;
    pub(crate) const SINH: u8 = 70;
    pub(crate) const TAN: u8 = 71;
    pub(crate) const TANH: u8 = 72;
    pub(crate) const CEIL: u8 = 73;
    pub(crate) const FLOOR: u8 = 74;
    pub(crate) const FMOD: u8 = 75;
    pub(crate) const FINITE: u8 = 76;
    pub(crate) const ISINF: u8 = 77;
    pub(crate) const ISNAN: u8 = 78;
    pub(crate) const NINT: u8 = 79;
    pub(crate) const RANDOM: u8 = 80;
    pub(crate) const REL_OR: u8 = 81;
    pub(crate) const REL_AND: u8 = 82;
    pub(crate) const REL_NOT: u8 = 83;
    pub(crate) const BIT_OR: u8 = 84;
    pub(crate) const BIT_AND: u8 = 85;
    pub(crate) const BIT_EXCL_OR: u8 = 86;
    pub(crate) const BIT_NOT: u8 = 87;
    pub(crate) const RIGHT_SHIFT_ARITH: u8 = 88;
    pub(crate) const LEFT_SHIFT_ARITH: u8 = 89;
    pub(crate) const RIGHT_SHIFT_LOGIC: u8 = 90;
    pub(crate) const NOT_EQ: u8 = 91;
    pub(crate) const LESS_THAN: u8 = 92;
    pub(crate) const LESS_OR_EQ: u8 = 93;
    pub(crate) const EQUAL: u8 = 94;
    pub(crate) const GR_OR_EQ: u8 = 95;
    pub(crate) const GR_THAN: u8 = 96;
    pub(crate) const COND_IF: u8 = 97;
    pub(crate) const COND_ELSE: u8 = 98;
    pub(crate) const COND_END: u8 = 99;
}

/// One C enum entry, or a run of them (`FETCH_A`..`FETCH_U`).
#[derive(Clone, Copy)]
enum Def {
    Op(Opcode),
    /// A vararg operator; its argument count follows as one byte.
    Vararg(Opcode),
    LitDouble,
    LitInt,
    LitString,
    /// `count` consecutive codes for args `0..count`.
    Fetch(u8),
    FetchD(u8),
    Store(u8),
    StoreD(u8),
    /// sCalc's `NO_STRING` / `USES_STRING`.
    Head,
    /// A code no compiler emits (`NOT_GENERATED`, `PAREN`, `COMMA`, `LOG_2`).
    Unused,
}

#[derive(Clone, Copy)]
enum Slot {
    Invalid,
    Op(Opcode),
    Vararg(Opcode),
    LitDouble,
    LitInt,
    LitString,
}

/// A dialect's decode table, indexed by opcode byte.
pub(crate) struct Table {
    slots: [Slot; 256],
}

const fn c(op: CoreOp) -> Def {
    Def::Op(Opcode::Core(op))
}
const fn s(op: StringOp) -> Def {
    Def::Op(Opcode::String(op))
}
const fn a(op: ArrayOp) -> Def {
    Def::Op(Opcode::Array(op))
}

const fn build(defs: &[Def]) -> Table {
    let mut slots = [Slot::Invalid; 256];
    let mut code = 0usize;
    let mut i = 0;
    while i < defs.len() {
        match defs[i] {
            Def::Op(op) => {
                slots[code] = Slot::Op(op);
                code += 1;
            }
            Def::Vararg(op) => {
                slots[code] = Slot::Vararg(op);
                code += 1;
            }
            Def::LitDouble => {
                slots[code] = Slot::LitDouble;
                code += 1;
            }
            Def::LitInt => {
                slots[code] = Slot::LitInt;
                code += 1;
            }
            Def::LitString => {
                slots[code] = Slot::LitString;
                code += 1;
            }
            Def::Fetch(n) | Def::FetchD(n) | Def::Store(n) | Def::StoreD(n) => {
                let mut k = 0;
                while k < n {
                    slots[code] = Slot::Op(Opcode::Core(match defs[i] {
                        Def::Fetch(_) => CoreOp::PushVar(k),
                        Def::FetchD(_) => CoreOp::PushDoubleVar(k),
                        Def::Store(_) => CoreOp::StoreVar(k),
                        _ => CoreOp::StoreDoubleVar(k),
                    }));
                    code += 1;
                    k += 1;
                }
            }
            Def::Head | Def::Unused => code += 1,
        }
        i += 1;
    }
    Table { slots }
}

/// base `rpn_opcode` (`postfixPvt.h`).
const BASE_DEFS: &[Def] = &[
    c(CoreOp::End),
    Def::LitDouble,
    Def::LitInt,
    c(CoreOp::FetchVal),
    Def::Fetch(21),
    Def::Store(21),
    c(CoreOp::Pi),
    c(CoreOp::D2R),
    c(CoreOp::R2D),
    c(CoreOp::Neg),
    c(CoreOp::Add),
    c(CoreOp::Sub),
    c(CoreOp::Mul),
    c(CoreOp::Div),
    c(CoreOp::Mod),
    c(CoreOp::Power),
    c(CoreOp::Abs),
    c(CoreOp::Exp),
    c(CoreOp::Log10),
    c(CoreOp::LogE),
    Def::Vararg(Opcode::Core(CoreOp::Max(0))),
    Def::Vararg(Opcode::Core(CoreOp::Min(0))),
    c(CoreOp::Sqrt),
    c(CoreOp::Acos),
    c(CoreOp::Asin),
    c(CoreOp::Atan),
    c(CoreOp::Atan2),
    c(CoreOp::Cos),
    c(CoreOp::Cosh),
    c(CoreOp::Sin),
    c(CoreOp::Sinh),
    c(CoreOp::Tan),
    c(CoreOp::Tanh),
    c(CoreOp::Ceil),
    c(CoreOp::Floor),
    c(CoreOp::Fmod),
    Def::Vararg(Opcode::Core(CoreOp::Finite(0))),
    c(CoreOp::IsInf),
    Def::Vararg(Opcode::Core(CoreOp::IsNan(0))),
    c(CoreOp::Nint),
    c(CoreOp::Random),
    c(CoreOp::Or),
    c(CoreOp::And),
    c(CoreOp::Not),
    c(CoreOp::BitOr),
    c(CoreOp::BitAnd),
    c(CoreOp::BitXor),
    c(CoreOp::BitNot),
    c(CoreOp::Shr),
    c(CoreOp::Shl),
    c(CoreOp::ShrLogical),
    c(CoreOp::Ne),
    c(CoreOp::Lt),
    c(CoreOp::Le),
    c(CoreOp::Eq),
    c(CoreOp::Ge),
    c(CoreOp::Gt),
    c(CoreOp::CondIf),
    c(CoreOp::CondElse),
    c(CoreOp::CondEnd),
    Def::Unused, // NOT_GENERATED
];
pub(crate) static BASE: Table = build(BASE_DEFS);

/// The opcodes sCalc and aCalc share up to `NOT_GENERATED`, in their order.
/// Each dialect inserts its own operand after `FETCH_VAL` and aCalc inserts
/// `ANEG_VAL`/`APOS_VAL` after `EXP`, so the list is spliced around those.
macro_rules! synapps_core_head {
    () => {
        [
            Def::Fetch(16),
            Def::FetchD(12),
            Def::Store(16),
            Def::StoreD(12),
            c(CoreOp::Pi),
            c(CoreOp::D2R),
            c(CoreOp::R2D),
            c(CoreOp::Neg),
            c(CoreOp::Add),
            c(CoreOp::Sub),
            c(CoreOp::Mul),
            c(CoreOp::Div),
            c(CoreOp::Mod),
            c(CoreOp::Power),
            c(CoreOp::Abs),
            c(CoreOp::Exp),
        ]
    };
}
macro_rules! synapps_core_tail {
    () => {
        [
            c(CoreOp::Log10),
            c(CoreOp::LogE),
            Def::Vararg(Opcode::Core(CoreOp::Max(0))),
            Def::Vararg(Opcode::Core(CoreOp::Min(0))),
            c(CoreOp::Sqrt),
            c(CoreOp::Acos),
            c(CoreOp::Asin),
            c(CoreOp::Atan),
            c(CoreOp::Atan2),
            c(CoreOp::Cos),
            c(CoreOp::Cosh),
            c(CoreOp::Sin),
            c(CoreOp::Sinh),
            c(CoreOp::Tan),
            c(CoreOp::Tanh),
            c(CoreOp::Ceil),
            c(CoreOp::Floor),
            Def::Vararg(Opcode::Core(CoreOp::Finite(0))),
            c(CoreOp::IsInf),
            Def::Vararg(Opcode::Core(CoreOp::IsNan(0))),
            c(CoreOp::Nint),
            c(CoreOp::Random),
            c(CoreOp::Or),
            c(CoreOp::And),
            c(CoreOp::Not),
            c(CoreOp::BitOr),
            c(CoreOp::BitAnd),
            c(CoreOp::BitXor),
            c(CoreOp::BitNot),
            c(CoreOp::Shr),
            c(CoreOp::Shl),
            c(CoreOp::Ne),
            c(CoreOp::Lt),
            c(CoreOp::Le),
            c(CoreOp::Eq),
            c(CoreOp::Ge),
            c(CoreOp::Gt),
            c(CoreOp::CondIf),
            c(CoreOp::CondElse),
            c(CoreOp::CondEnd),
            Def::Op(Opcode::Control(ControlOp::NotGenerated)),
        ]
    };
}

const fn concat<const A: usize, const B: usize, const C: usize, const N: usize>(
    x: [Def; A],
    y: [Def; B],
    z: [Def; C],
) -> [Def; N] {
    assert!(A + B + C == N);
    let mut out = [Def::Unused; N];
    let mut i = 0;
    while i < A {
        out[i] = x[i];
        i += 1;
    }
    while i < A + B {
        out[i] = y[i - A];
        i += 1;
    }
    while i < N {
        out[i] = z[i - A - B];
        i += 1;
    }
    out
}

const SCALC_DEFS: &[Def] = &concat::<21, 41, 36, 98>(
    concat::<5, 16, 0, 21>(
        [
            c(CoreOp::End),
            Def::LitDouble,
            Def::LitInt,
            c(CoreOp::FetchVal),
            c(CoreOp::FetchSval),
        ],
        synapps_core_head!(),
        [],
    ),
    synapps_core_tail!(),
    [
        c(CoreOp::S2R),
        c(CoreOp::R2S),
        Def::LitString,
        s(StringOp::Sscanf),
        Def::Unused, // LOG_2
        Def::Unused, // PAREN
        c(CoreOp::MaxVal),
        c(CoreOp::MinVal),
        Def::Unused, // COMMA
        s(StringOp::ToDouble),
        s(StringOp::Printf),
        s(StringOp::Subrange),
        s(StringOp::ToString),
        s(StringOp::Replace),
        s(StringOp::DynFetch),
        s(StringOp::DynSFetch),
        s(StringOp::Byte),
        s(StringOp::SubLast),
        s(StringOp::TrEsc),
        s(StringOp::Esc),
        s(StringOp::Crc16),
        s(StringOp::Crc16Append),
        s(StringOp::Lrc),
        s(StringOp::LrcAppend),
        s(StringOp::Xor8),
        s(StringOp::Xor8Append),
        s(StringOp::BinRead),
        s(StringOp::BinWrite),
        s(StringOp::Len),
        c(CoreOp::NormalRandom),
        s(StringOp::DynStore),
        s(StringOp::DynSStore),
        Def::Op(Opcode::Control(ControlOp::Until)),
        Def::Op(Opcode::Control(ControlOp::UntilEnd)),
        Def::Head, // NO_STRING
        Def::Head, // USES_STRING
    ],
);

/// sCalc `sCalc_rpn_opcode` (`sCalcPostfixPvt.h`).
pub(crate) static SCALC: Table = build(SCALC_DEFS);

const ACALC_DEFS: &[Def] = &concat::<23, 41, 41, 105>(
    concat::<5, 16, 2, 23>(
        [
            c(CoreOp::End),
            Def::LitDouble,
            Def::LitInt,
            c(CoreOp::FetchVal),
            a(ArrayOp::FetchAval),
        ],
        synapps_core_head!(),
        [a(ArrayOp::ANeg), a(ArrayOp::APos)],
    ),
    synapps_core_tail!(),
    [
        c(CoreOp::S2R),
        c(CoreOp::R2S),
        a(ArrayOp::ConstIndex),
        Def::Unused, // LOG_2
        Def::Unused, // PAREN
        c(CoreOp::MaxVal),
        c(CoreOp::MinVal),
        Def::Unused, // COMMA
        a(ArrayOp::ToDouble),
        a(ArrayOp::ArraySubrange),
        a(ArrayOp::ArraySubrangeInPlace),
        a(ArrayOp::ToArray),
        a(ArrayOp::DynFetch),
        a(ArrayOp::DynAFetch),
        a(ArrayOp::LenNoop),
        c(CoreOp::NormalRandom),
        a(ArrayOp::DynStore),
        a(ArrayOp::DynAStore),
        Def::Op(Opcode::Control(ControlOp::Until)),
        Def::Op(Opcode::Control(ControlOp::UntilEnd)),
        a(ArrayOp::Average),
        a(ArrayOp::StdDev),
        a(ArrayOp::Fwhm),
        a(ArrayOp::Smooth),
        a(ArrayOp::NSmooth),
        a(ArrayOp::Deriv),
        a(ArrayOp::NDeriv),
        a(ArrayOp::ArraySum),
        a(ArrayOp::ArrayMax),
        a(ArrayOp::ArrayMin),
        a(ArrayOp::FitPoly),
        a(ArrayOp::FitMPoly),
        a(ArrayOp::ArrayRandom),
        a(ArrayOp::Cum),
        a(ArrayOp::IndexMax),
        a(ArrayOp::IndexMin),
        a(ArrayOp::IndexZero),
        a(ArrayOp::IndexNonZero),
        Def::Vararg(Opcode::Array(ArrayOp::FitQ(0))),
        Def::Vararg(Opcode::Array(ArrayOp::FitMQ(0))),
        a(ArrayOp::Cat),
    ],
);

/// aCalc `aCalc_rpn_opcode` (`aCalcPostfixPvt.h`).
pub(crate) static ACALC: Table = build(ACALC_DEFS);

/// sCalc's head byte values: the last two entries of its enum.
pub(crate) const SCALC_NO_STRING: u8 = SCALC_DEFS_CODES - 2;
pub(crate) const SCALC_USES_STRING: u8 = SCALC_DEFS_CODES - 1;
const SCALC_DEFS_CODES: u8 = codes(SCALC_DEFS);

const fn codes(defs: &[Def]) -> u8 {
    let mut n = 0usize;
    let mut i = 0;
    while i < defs.len() {
        n += match defs[i] {
            Def::Fetch(k) | Def::FetchD(k) | Def::Store(k) | Def::StoreD(k) => k as usize,
            _ => 1,
        };
        i += 1;
    }
    assert!(n <= 256);
    n as u8
}

pub(crate) fn table(kind: ExprKind) -> &'static Table {
    match kind {
        ExprKind::Numeric => &BASE,
        ExprKind::String => &SCALC,
        ExprKind::Array => &ACALC,
    }
}

/// Where the first instruction sits: past sCalc's head byte, which every
/// non-empty sCalc program has (`sCalcPerform.c:399` consumes it
/// unconditionally).
pub(crate) fn body_start(kind: ExprKind, code: &[u8]) -> usize {
    match kind {
        ExprKind::String if !is_empty(code) => 1,
        _ => 0,
    }
}

/// C `*postfix == END_EXPRESSION` — the program has nothing to run. The empty
/// and the failed compile both leave exactly this (`postfix.c:238,506`,
/// `sCalcPostfix.c:429,880`, `aCalcPostfix.c:434,808`).
pub(crate) fn is_empty(code: &[u8]) -> bool {
    code.first().is_none_or(|&b| b == END)
}

/// sCalc's `*postfix == USES_STRING`, the one test `sCalcPerform` makes to
/// pick its evaluator (`sCalcPerform.c:399`).
pub(crate) fn uses_string(code: &[u8]) -> bool {
    code.first() == Some(&SCALC_USES_STRING)
}

/// Read the instruction at `pc`: the opcode and where the next one starts.
/// Running off the end of the slice reads `END_EXPRESSION`.
#[inline(always)]
pub(crate) fn decode(t: &Table, code: &[u8], pc: usize) -> Result<(Opcode, usize), CalcError> {
    let Some(&b) = code.get(pc) else {
        return Ok((Opcode::Core(CoreOp::End), pc));
    };
    let pc = pc + 1;
    match t.slots[b as usize] {
        Slot::Op(op) => Ok((op, pc)),
        Slot::LitInt => {
            let p = code.get(pc..pc + 4).ok_or(CalcError::Internal)?;
            let v = i32::from_ne_bytes([p[0], p[1], p[2], p[3]]);
            Ok((Opcode::Core(CoreOp::PushConst(f64::from(v))), pc + 4))
        }
        Slot::LitDouble => {
            let p = code.get(pc..pc + 8).ok_or(CalcError::Internal)?;
            let mut b = [0u8; 8];
            b.copy_from_slice(p);
            Ok((
                Opcode::Core(CoreOp::PushConst(f64::from_ne_bytes(b))),
                pc + 8,
            ))
        }
        Slot::LitString => {
            let rest = code.get(pc..).unwrap_or_default();
            let len = rest
                .iter()
                .position(|&c| c == 0)
                .ok_or(CalcError::Internal)?;
            let op = StringOp::PushString {
                at: pc as u32,
                len: len as u32,
            };
            Ok((Opcode::String(op), pc + len + 1))
        }
        Slot::Vararg(op) => {
            let n = *code.get(pc).ok_or(CalcError::Internal)?;
            Ok((with_nargs(op, n), pc + 1))
        }
        Slot::Invalid => Err(CalcError::Internal),
    }
}

fn with_nargs(op: Opcode, n: u8) -> Opcode {
    match op {
        Opcode::Core(CoreOp::Max(_)) => Opcode::Core(CoreOp::Max(n)),
        Opcode::Core(CoreOp::Min(_)) => Opcode::Core(CoreOp::Min(n)),
        Opcode::Core(CoreOp::Finite(_)) => Opcode::Core(CoreOp::Finite(n)),
        Opcode::Core(CoreOp::IsNan(_)) => Opcode::Core(CoreOp::IsNan(n)),
        Opcode::Array(ArrayOp::FitQ(_)) => Opcode::Array(ArrayOp::FitQ(n)),
        Opcode::Array(ArrayOp::FitMQ(_)) => Opcode::Array(ArrayOp::FitMQ(n)),
        other => other,
    }
}

/// Every instruction of `code` with its offset, up to (not including) the
/// `END_EXPRESSION`. Stops at the first undecodable byte.
pub(crate) fn instructions(
    kind: ExprKind,
    code: &[u8],
) -> impl Iterator<Item = (usize, Opcode)> + '_ {
    let t = table(kind);
    let mut pc = body_start(kind, code);
    std::iter::from_fn(move || match decode(t, code, pc) {
        Ok((Opcode::Core(CoreOp::End), _)) | Err(_) => None,
        Ok((op, next)) => {
            let at = pc;
            pc = next;
            Some((at, op))
        }
    })
}

/// C `cond_search` (`calcPerform.c:527-557`, `sCalcPerform.c:2103-2150`,
/// `aCalcPerform.c:1691-1735`): scan forward from `pc` for the `COND_ELSE`
/// (`find_else`) or `COND_END` matching the one being skipped, counting nested
/// `COND_IF`s, and return the offset after it.
pub(crate) fn cond_search(
    t: &Table,
    code: &[u8],
    mut pc: usize,
    find_else: bool,
) -> Result<usize, CalcError> {
    let mut count: i32 = 1;
    loop {
        let (op, next) = decode(t, code, pc)?;
        let is_match = match op {
            Opcode::Core(CoreOp::End) => return Err(CalcError::Conditional),
            Opcode::Core(CoreOp::CondElse) => find_else,
            Opcode::Core(CoreOp::CondEnd) => !find_else,
            _ => false,
        };
        if is_match {
            count -= 1;
            if count == 0 {
                return Ok(next);
            }
        }
        if matches!(op, Opcode::Core(CoreOp::CondIf)) {
            count += 1;
        }
        pc = next;
    }
}

/// C's `until_scratch[10]` guarded by `if (i>9) return(-1)` (`sCalcPerform.c:356-360`)
/// and aCalc's `MAX_UNTIL_OP 10` with `if (i > (MAX_UNTIL_OP-1))`
/// (`aCalcPerform.c:369-373`): the TENTH UNTIL present fails the perform.
pub(crate) const MAX_UNTILS: usize = 9;

/// C's `until_scratch[]` (`sCalcPerform.c:330-375`): every UNTIL's offset, the
/// offset of the UNTIL_END that closes it, and the stack depth the UNTIL last
/// saw. Filled by a STATIC pre-scan before a single opcode runs, so the
/// ceiling counts UNTILs present, not UNTILs reached.
pub(crate) struct UntilScratch {
    slots: [UntilSlot; MAX_UNTILS],
    n: usize,
}

#[derive(Clone, Copy)]
struct UntilSlot {
    until: usize,
    end: Option<usize>,
    depth: usize,
}

impl UntilScratch {
    /// `sCalcPerform.c:339-378` / `aCalcPerform.c:356-394`, stepping with
    /// [`decode`]. C steps by byte value and skips only literals, so it reads
    /// the count byte after `MAX`/`MIN`/`FINITE`/`ISNAN` and the bytes of a
    /// literal as opcodes, misses or invents an UNTIL/UNTIL_END and fails a
    /// valid program with -1 (CBUG-O1), or reads past the postfix (CBUG-O2).
    /// Neither is copied.
    pub(crate) fn scan(t: &Table, kind: ExprKind, code: &[u8]) -> Result<Self, CalcError> {
        let mut me = UntilScratch {
            slots: [UntilSlot {
                until: 0,
                end: None,
                depth: 0,
            }; MAX_UNTILS],
            n: 0,
        };
        let mut pc = body_start(kind, code);
        loop {
            let (op, next) = decode(t, code, pc)?;
            match op {
                Opcode::Core(CoreOp::End) => return Ok(me),
                Opcode::Control(ControlOp::Until) => me.open(pc)?,
                Opcode::Control(ControlOp::UntilEnd) => me.close(pc)?,
                _ => {}
            }
            pc = next;
        }
    }

    fn open(&mut self, at: usize) -> Result<(), CalcError> {
        if self.n == MAX_UNTILS {
            return Err(CalcError::Overflow);
        }
        self.slots[self.n] = UntilSlot {
            until: at,
            end: None,
            depth: 0,
        };
        self.n += 1;
        Ok(())
    }

    /// The latest UNTIL not yet claimed; C's "unmatched UNTIL_END" is -1.
    fn close(&mut self, at: usize) -> Result<(), CalcError> {
        let slot = self.slots[..self.n]
            .iter_mut()
            .rev()
            .find(|s| s.end.is_none())
            .ok_or(CalcError::Internal)?;
        slot.end = Some(at);
        Ok(())
    }

    /// C `UNTIL`: remember the stack depth for the UNTIL at `at`; one the
    /// scan never saw is C's "UNTIL not found", -1 (`sCalcPerform.c:782-785,1989-1992`,
    /// `aCalcPerform.c:1562-1566`).
    pub(crate) fn mark(&mut self, at: usize, depth: usize) -> Result<(), CalcError> {
        let s = self.slots[..self.n]
            .iter_mut()
            .find(|s| s.until == at)
            .ok_or(CalcError::Internal)?;
        s.depth = depth;
        Ok(())
    }

    /// C `UNTIL_END` looping back: the UNTIL paired with the UNTIL_END at
    /// `at`, and the depth it saw.
    pub(crate) fn loop_back(&self, at: usize) -> Option<(usize, usize)> {
        self.slots[..self.n]
            .iter()
            .find(|s| s.end == Some(at))
            .map(|s| (s.until, s.depth))
    }
}

/// The one writer of a program. The compiler hands it opcodes in emission
/// order; it lays them down in the dialect's encoding.
pub(crate) struct Emitter {
    kind: ExprKind,
    code: Vec<u8>,
    /// Offset of the last instruction written, for `:=`'s retraction.
    last: Option<usize>,
    /// An opcode this dialect has no code for — a compiler defect, reported
    /// as `CALC_ERR_INTERNAL` by [`finish`](Self::finish).
    bad: bool,
}

impl Emitter {
    pub(crate) fn new(kind: ExprKind) -> Self {
        let mut code = Vec::new();
        if kind == ExprKind::String {
            code.push(SCALC_NO_STRING);
        }
        Emitter {
            kind,
            code,
            last: None,
            bad: false,
        }
    }

    pub(crate) fn op(&mut self, op: Opcode) {
        let at = self.code.len();
        if encode(table(self.kind), op, &mut self.code) {
            self.last = Some(at);
        } else {
            self.bad = true;
        }
    }

    /// sCalc `LITERAL_STRING`: the bytes and their NUL (`sCalcPostfix.c:805-809`).
    /// The source is a C string, so `s` cannot hold a NUL.
    pub(crate) fn string(&mut self, s: &[u8]) {
        let at = self.code.len();
        match lit_string_code(table(self.kind)) {
            Some(b) => {
                self.code.push(b);
                self.code.extend_from_slice(s);
                self.code.push(END);
                self.last = Some(at);
            }
            None => self.bad = true,
        }
    }

    /// The instruction most recently written.
    pub(crate) fn last(&self) -> Option<Opcode> {
        let at = self.last?;
        decode(table(self.kind), &self.code, at)
            .ok()
            .map(|(op, _)| op)
    }

    /// Drop the instruction most recently written.
    pub(crate) fn retract_last(&mut self) {
        if let Some(at) = self.last.take() {
            self.code.truncate(at);
        }
    }

    /// sCalc's `*ppostfix = USES_STRING` (`sCalcPostfix.c:450-471`).
    pub(crate) fn set_uses_string(&mut self) {
        if self.kind == ExprKind::String {
            self.code[0] = SCALC_USES_STRING;
        }
    }

    /// `*pout = END_EXPRESSION`.
    pub(crate) fn finish(mut self) -> Result<Vec<u8>, CalcError> {
        if self.bad {
            return Err(CalcError::Internal);
        }
        self.code.push(END);
        Ok(self.code)
    }
}

fn lit_string_code(t: &Table) -> Option<u8> {
    t.slots
        .iter()
        .position(|s| matches!(s, Slot::LitString))
        .map(|i| i as u8)
}

/// Lay down `op` in `t`'s encoding. False when `t` has no code for it.
fn encode(t: &Table, op: Opcode, out: &mut Vec<u8>) -> bool {
    if let Opcode::Core(CoreOp::PushConst(v)) = op {
        // C's `lit_i = lit_d; if (lit_d != (double) lit_i)` (`postfix.c:268`,
        // `sCalcPostfix.c:497`, `aCalcPostfix.c:467`). Rust's saturating cast
        // answers the same question as x86's out-of-range INT_MIN: the value is
        // an int exactly when it is integral and in i32's range.
        let i = v as i32;
        if v == f64::from(i) {
            out.push(LITERAL_INT);
            out.extend_from_slice(&i.to_ne_bytes());
        } else {
            out.push(LITERAL_DOUBLE);
            out.extend_from_slice(&v.to_ne_bytes());
        }
        return true;
    }
    let nargs = match op {
        Opcode::Core(CoreOp::Max(n) | CoreOp::Min(n) | CoreOp::Finite(n) | CoreOp::IsNan(n))
        | Opcode::Array(ArrayOp::FitQ(n) | ArrayOp::FitMQ(n)) => Some(n),
        _ => None,
    };
    let found = t.slots.iter().position(|s| match (s, nargs) {
        (Slot::Op(o), None) => *o == op,
        (Slot::Vararg(o), Some(n)) => with_nargs(*o, n) == op,
        _ => false,
    });
    let Some(b) = found else { return false };
    out.push(b as u8);
    if let Some(n) = nargs {
        out.push(n);
    }
    true
}

/// `calcExprDump`-style listing, one instruction per line with its offset.
pub(crate) fn disassemble(kind: ExprKind, code: &[u8]) -> String {
    let mut out = String::new();
    for (at, op) in instructions(kind, code) {
        out.push_str(&format!("{at:4}: {op:?}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every table entry decodes back to the opcode it encodes — one table is
    /// both directions, so this pins the two against each other.
    #[test]
    fn every_code_round_trips() {
        for kind in [ExprKind::Numeric, ExprKind::String, ExprKind::Array] {
            let t = table(kind);
            for (b, slot) in t.slots.iter().enumerate() {
                let op = match *slot {
                    Slot::Op(op) => op,
                    Slot::Vararg(op) => with_nargs(op, 3),
                    _ => continue,
                };
                let mut code = Vec::new();
                assert!(encode(t, op, &mut code), "{kind:?} {op:?}");
                assert_eq!(code[0] as usize, b, "{kind:?} {op:?}");
                assert_eq!(decode(t, &code, 0).unwrap(), (op, code.len()));
            }
        }
    }

    #[test]
    fn base_constants_name_the_base_table() {
        use CoreOp as C;
        use base::*;
        let op = |b: u8| decode(&BASE, &[b, 1, 0, 0, 0, 0, 0, 0, 0], 0).unwrap().0;
        for (b, want) in [
            (END, C::End),
            (LITERAL_INT, C::PushConst(1.0)),
            (FETCH_VAL, C::FetchVal),
            (FETCH_A, C::PushVar(0)),
            (FETCH_U, C::PushVar(20)),
            (STORE_A, C::StoreVar(0)),
            (STORE_U, C::StoreVar(20)),
            (CONST_PI, C::Pi),
            (CONST_D2R, C::D2R),
            (CONST_R2D, C::R2D),
            (UNARY_NEG, C::Neg),
            (ADD, C::Add),
            (SUB, C::Sub),
            (MULT, C::Mul),
            (DIV, C::Div),
            (MODULO, C::Mod),
            (POWER, C::Power),
            (ABS_VAL, C::Abs),
            (EXP, C::Exp),
            (LOG_10, C::Log10),
            (LOG_E, C::LogE),
            (MAX, C::Max(1)),
            (MIN, C::Min(1)),
            (SQU_RT, C::Sqrt),
            (ACOS, C::Acos),
            (ASIN, C::Asin),
            (ATAN, C::Atan),
            (ATAN2, C::Atan2),
            (COS, C::Cos),
            (COSH, C::Cosh),
            (SIN, C::Sin),
            (SINH, C::Sinh),
            (TAN, C::Tan),
            (TANH, C::Tanh),
            (CEIL, C::Ceil),
            (FLOOR, C::Floor),
            (FMOD, C::Fmod),
            (FINITE, C::Finite(1)),
            (ISINF, C::IsInf),
            (ISNAN, C::IsNan(1)),
            (NINT, C::Nint),
            (RANDOM, C::Random),
            (REL_OR, C::Or),
            (REL_AND, C::And),
            (REL_NOT, C::Not),
            (BIT_OR, C::BitOr),
            (BIT_AND, C::BitAnd),
            (BIT_EXCL_OR, C::BitXor),
            (BIT_NOT, C::BitNot),
            (RIGHT_SHIFT_ARITH, C::Shr),
            (LEFT_SHIFT_ARITH, C::Shl),
            (RIGHT_SHIFT_LOGIC, C::ShrLogical),
            (NOT_EQ, C::Ne),
            (LESS_THAN, C::Lt),
            (LESS_OR_EQ, C::Le),
            (EQUAL, C::Eq),
            (GR_OR_EQ, C::Ge),
            (GR_THAN, C::Gt),
            (COND_IF, C::CondIf),
            (COND_ELSE, C::CondElse),
            (COND_END, C::CondEnd),
        ] {
            assert_eq!(op(b), Opcode::Core(want), "byte {b}");
        }
        let lit_d = [LITERAL_DOUBLE, 0, 0, 0, 0, 0, 0, 0xf8, 0x3f];
        assert_eq!(
            decode(&BASE, &lit_d, 0).unwrap().0,
            Opcode::Core(C::PushConst(1.5))
        );
        // NOT_GENERATED closes the enum and decodes to nothing.
        assert!(decode(&BASE, &[COND_END + 1], 0).is_err());
    }

    /// The enum extents, from compiled C: base `NOT_GENERATED` is 100, sCalc
    /// `USES_STRING` 149, aCalc `CAT` 156.
    #[test]
    fn the_three_enums_have_cs_extent() {
        assert_eq!(codes(BASE_DEFS), 101);
        assert_eq!(codes(SCALC_DEFS), 150);
        assert_eq!(codes(ACALC_DEFS), 157);
        assert_eq!(SCALC_USES_STRING, 149);
    }
}
