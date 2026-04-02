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
    Log2,
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

    // Special
    Random,
    NormalRandom,

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
}

#[derive(Debug, Clone, PartialEq)]
pub enum Opcode {
    Core(CoreOp),
    String(StringOp),
    Control(ControlOp),
    Array(ArrayOp),
}
