#[derive(Debug, Clone, PartialEq)]
pub enum CoreOp {
    // Operands
    PushConst(f64),
    PushVar(u8),       // 0..15 = A..P
    PushDoubleVar(u8), // 0..11 = AA..LL (string vars, fetched as numeric)

    // Arithmetic
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Neg,
    Power,

    // Comparison
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,

    // Logical
    And,
    Or,
    Not,

    // Bitwise
    BitAnd,
    BitOr,
    BitXor,
    BitNot,
    Shl,
    Shr,
    ShrLogical,

    // Conditional
    CondIf,
    CondElse,
    CondEnd,

    // Functions (1 arg)
    Abs,
    Sqrt,
    Exp,
    Log10,
    LogE,
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    Atan,
    Sinh,
    Cosh,
    Tanh,
    Ceil,
    Floor,
    Nint,
    IsNan(u8), // vararg: number of args
    IsInf,
    Finite(u8), // vararg: number of args

    // Functions (2 arg)
    Atan2,
    Fmod,

    // Vararg functions
    Max(u8), // number of args
    Min(u8), // number of args

    // Binary operators (2 arg, infix)
    MaxVal, // >?
    MinVal, // <?

    // Constants
    Pi,
    D2R,
    R2D,
    /// sCalc/aCalc `S2R` — arc-seconds to radians, PI/(180*3600)
    /// (`sCalcPerform.c:470-473`, `aCalcPostfix.c:195`). Base has no such
    /// constant.
    S2R,
    /// sCalc/aCalc `R2S` — radians to arc-seconds, (180*3600)/PI
    /// (`sCalcPerform.c:475-478`).
    R2S,

    // Special
    Random,
    NormalRandom,
    FetchVal,
    /// C `FETCH_SVAL` (sCalcPerform.c:927-932) — push the previous *string*
    /// result (`psresult`). String-calc only; the numeric and array
    /// evaluators reject it, as their C element tables cannot emit it.
    FetchSval,

    // Assignment
    StoreVar(u8),       // 0..15 = A..P
    StoreDoubleVar(u8), // 0..11 = AA..LL

    // End
    End,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StringOp {
    // Phase 2A: Core
    PushString(String),
    PushStringVar(u8),  // AA..LL string push
    StoreStringVar(u8), // AA..LL string store
    ToString,           // STR: number→string
    ToDouble,           // DBL: string→number
    Len,                // string length
    Byte,               // first char ASCII value
    // Phase 2B: Advanced
    TrEsc,
    Esc,
    Printf,
    Sscanf,
    BinRead,
    BinWrite,
    Crc16,
    Crc16Append, // MODBUS
    Lrc,
    LrcAppend, // AMODBUS
    Xor8,
    Xor8Append, // ADD_XOR8
    Subrange,   // [i:j]
    Replace,    // {find,replace}
    SubLast,    // |- last substring removal
}

#[derive(Debug, Clone, PartialEq)]
pub enum ControlOp {
    Until(usize),    // jump target = UntilEnd pc
    UntilEnd(usize), // jump target = Until pc
}

#[derive(Debug, Clone, PartialEq)]
pub enum ArrayOp {
    ConstIndex, // IX: [0,1,...,n-1]
    ToArray,    // ARR: scalar→array
    ToDouble,   // array→scalar (first element, empty=0.0)
    Average,
    StdDev,
    Fwhm,
    ArraySum,
    ArrayMax,
    ArrayMin,
    IndexMax,
    IndexMin,
    IndexZero,
    IndexNonZero,
    // Phase 3B: Advanced
    Smooth,
    NSmooth,
    Deriv,
    NDeriv,
    Cum,
    Cat,
    ArrayRandom,
    ArraySubrange,
    ArraySubrangeInPlace,
    FitPoly,
    FitMPoly,
    FitQ,
    FitMQ,
    /// `ANEG` (`aCalcPerform.c:772,1041`) — zero the NEGATIVE elements, keep the
    /// rest. (The name says which sign it removes, not which it keeps.)
    ANeg,
    /// `APOS` (`aCalcPerform.c:773,1042`) — zero the POSITIVE elements.
    APos,
    /// `AVAL` (`aCalcPerform.c:534-539`) — push the record's previous AVAL, the
    /// array-valued counterpart of the `VAL` token.
    FetchAval,
    /// `@` A_FETCH (`aCalcPerform.c:1461-1477`) — fetch the scalar argument the
    /// operand INDEXES, i.e. `@1` is B. Out of range is 0.
    DynFetch,
    /// `@@` A_AFETCH (`aCalcPerform.c:1479-1494`) — the same for arrays: `@@1`
    /// is BB. Out of range is an all-zero array.
    DynAFetch,
    /// aCalc `LEN` (`aCalcPostfix.c:199`) — in the element table, so it LEXES and
    /// COMPILES, but `aCalcPerform` has no `case LEN` and no `default`, so the
    /// opcode falls straight through the switch and the operand is left on the
    /// stack untouched. C's own table comment says "Array length not
    /// implemented". Compiled aCalc: `LEN(AA)` returns AA unchanged. It is a
    /// no-op, and the port must not invent a length for it.
    LenNoop,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Opcode {
    Core(CoreOp),
    String(StringOp),
    Control(ControlOp),
    Array(ArrayOp),
}
