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
    /// C `LITERAL_STRING`. The bytes the compiler copied out of the source
    /// verbatim (`sCalcPostfix.c:808`) — raw, un-decoded, backslashes intact.
    /// `$T` / `TR_ESC` is the only translator.
    PushString(Vec<u8>),
    ToString, // STR: number→string
    ToDouble, // DBL: string→number
    Len,      // string length
    Byte,     // first char ASCII value
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
    /// sCalc `@` — C `A_FETCH` (`sCalcPerform.c:1446-1460`). The scalar argument
    /// the operand INDEXES, so `@1` is B; the index is rounded with `myNINT` and
    /// out of range answers 0. A sibling of aCalc's [`ArrayOp::DynFetch`], but it
    /// lives here because the string evaluator is the only one whose C table can
    /// emit it.
    ///
    /// It is NOT in C's `USES_STRING` list (`sCalcPostfix.c:450-471`) — it answers
    /// a double — which is why `uses_string` classifies it `false`.
    DynFetch,
    /// sCalc `@@` — C `A_SFETCH` (`sCalcPerform.c:1462-1476`). The STRING argument
    /// the operand indexes, so `@@1` is BB; out of range is the EMPTY string, not
    /// an error. This is where sCalc and aCalc genuinely part: aCalc's `@@` is
    /// `A_AFETCH`, the ARRAY argument ([`ArrayOp::DynAFetch`]).
    ///
    /// It IS in C's `USES_STRING` list (`sCalcPostfix.c:461`).
    DynSFetch,
    /// sCalc `@n:=v` — C `A_STORE` (`sCalcPerform.c:440-450` no-string,
    /// `:897-906` string). The mirror of [`DynFetch`](Self::DynFetch): it pops
    /// the VALUE and then the INDEX (the index expression was emitted first),
    /// rounds the index with `myNINT`, and writes the scalar argument it names.
    /// Out of range prints and stores nothing — it is not an error, exactly as
    /// for the fetch. It pushes NOTHING back.
    DynStore,
    /// sCalc `@@n:=v` — C `A_SSTORE` (`sCalcPerform.c:909-918`). The same, for
    /// the STRING argument (`toString` on the value first). aCalc's `@@:=` is a
    /// different opcode, [`ArrayOp::DynAStore`], as its `@@` is a different
    /// fetch.
    DynSStore,
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
    /// `FITPOLY(y)` (`aCalcPerform.c:999-1015`) — fit `y = c + b*i + a*i*i` over
    /// the operand's window and REPLACE it with the fitted curve.
    FitPoly,
    /// `FITMPOLY(y, mask)` (`:1017-1036`) — the same, with the fit restricted to
    /// the points the mask admits and the window taken from the MASK operand.
    FitMPoly,
    /// `FITQ(y [,c][,b][,a])` (`:1193-1235`) — VARARG (`aCalcPostfix.c:140`), and
    /// the payload is the argument count, because the trailing arguments NAME the
    /// scalar arguments the coefficients are stored into.
    FitQ(u8),
    /// `FITMQ(y, mask [,c][,b][,a])` (`aCalcPerform.c:1237-1284`) — likewise
    /// (`aCalcPostfix.c:141`).
    FitMQ(u8),
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
    /// `@n:=v` A_STORE (`aCalcPerform.c:494-504`) — pop the value, pop and round
    /// the index, write that scalar argument. Out of range prints and stores
    /// nothing. Pushes nothing.
    DynStore,
    /// `@@n:=v` A_ASTORE (`aCalcPerform.c:506-525`) — the same for the ARRAY
    /// argument: an array value is copied element-wise, a SCALAR value is
    /// broadcast to every element, and the store sets `amask` bit `n` so the
    /// record posts the field it wrote.
    DynAStore,
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
