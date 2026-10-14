use super::ExprKind;
use super::error::CalcError;
use super::strtod;

#[derive(Debug, Clone, PartialEq)]
pub enum FuncName {
    Abs,
    Sqrt,
    Sqr,
    Exp,
    Log10,
    LogE,
    Ln,
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    Atan,
    Atan2,
    Fmod,
    Sinh,
    Cosh,
    Tanh,
    Ceil,
    Floor,
    Nint,
    Int,
    IsNan,
    IsInf,
    Finite,
    Max,
    Min,
    Not, // bitwise NOT as function
    // String functions (Phase 2A)
    Dbl,
    Str,
    Len,
    Byte,
    // String functions (Phase 2B)
    TrEsc,
    Esc,
    /// aCalc `ANEG` / `APOS` (`aCalcPostfix.c:153-154`).
    ANeg,
    APos,
    /// aCalc `@` / `@@` (`aCalcPostfix.c:93-94`) — fetch the scalar/array
    /// argument that the operand indexes.
    DynFetch,
    DynAFetch,
    /// sCalc `@` / `@@` (`sCalcPostfix.c:99-100`) — `A_FETCH` and `A_SFETCH`.
    /// `@` is the same idea as aCalc's, but it lands on the string engine's
    /// stack, and `@@` is a different opcode entirely: aCalc's fetches the ARRAY
    /// argument (`A_AFETCH`), sCalc's fetches the STRING argument (`A_SFETCH`).
    /// Separate names because the opcode a token compiles to is a function of the
    /// token alone — the same rule that gives aCalc's no-op `LEN` its own
    /// [`FuncName::ALenNoop`].
    SDynFetch,
    SDynSFetch,
    /// aCalc `LEN` (`aCalcPostfix.c:199`) — a table entry with no implementation
    /// in `aCalcPerform`, so it compiles and does nothing. Distinct from the
    /// sCalc `LEN` string length ([`FuncName::Len`]), which IS implemented.
    ALenNoop,
    Printf,
    Sscanf,
    BinRead,
    BinWrite,
    Crc16,
    ModBus,
    Lrc,
    AModBus,
    Xor8,
    AddXor8,
    // Array functions (Phase 3A)
    Avg,
    Std,
    FwhmFunc,
    Sum,
    AMax,
    AMin,
    IxMax,
    IxMin,
    IxZ,
    IxNz,
    Arr,
    AToD,
    // Array functions (Phase 3B)
    Smoo,
    NSmoo,
    Deriv,
    NDeriv,
    FitPoly,
    FitMPoly,
    FitQ,
    FitMQ,
    Cum,
    Cat,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConstName {
    Pi,
    D2R,
    R2D,
    /// `S2R` / `R2S` — arc-seconds <-> radians. In the sCalc AND aCalc element
    /// tables (`sCalcPostfix.c:136,173`, `aCalcPostfix.c:186,195`); base has
    /// neither, so they stay out of BASE_TABLE.
    S2R,
    R2S,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Number(f64),
    Var(u8),       // A=0..
    DoubleVar(u8), // AA=0..
    Rndm,
    Nrndm,
    FetchVal,
    /// `SVAL` — the previous *string* result (C `FETCH_SVAL`,
    /// sCalcPostfix.c:188). String-calc only: the numeric `postfix()` and
    /// `aCalcPostfix()` element tables have no such symbol.
    FetchSval,

    /// C `LITERAL_STRING` (`sCalcPostfix.c:803-812`). RAW BYTES: the compiler
    /// copies the source verbatim between the quotes and interprets nothing.
    /// Hence `Vec<u8>` rather than a `String` — there is no decoding step in
    /// which a translation could hide.
    StringLiteral(Vec<u8>),

    Plus,
    Minus,
    /// aCalc `AVAL` — the array-valued `VAL` (`aCalcPostfix.c:118`, OPERAND).
    FetchAval,
    /// aCalc `IX` / `ARNDM` — OPERANDs in C (`aCalcPostfix.c:157,114`), so they
    /// emit where they are read, not as nullary functions after their operator.
    ConstIx,
    ARndm,
    Star,
    Slash,
    Percent,
    Caret,
    DoubleStar,

    Eq, // == or =
    Ne, // != or #
    Lt,
    Le,
    Gt,
    Ge,

    AndAnd,     // &&
    OrOr,       // ||
    BitAnd,     // &
    BitOr,      // |
    BitXor,     // XOR
    Tilde,      // ~
    Shl,        // <<
    Shr,        // >>
    ShrLogical, // >>>

    Bang, // !
    Question,
    Colon,
    // NOTE: there are no `AND` / `OR` keyword tokens. All three C tables give the
    // words the SAME `code` as the symbols `&` and `|` — `BIT_AND` and `BIT_OR`
    // (postfix.c:174-175, sCalcPostfix.c:237-238, aCalcPostfix.c:234-235) — just
    // as `XOR` shares `BIT_EXCL_OR` with `^`. They are alternate spellings of the
    // bitwise operators, never of `&&` / `||`, so they lex straight to
    // [`Token::BitAnd`] / [`Token::BitOr`] and no opcode mapping can diverge.
    LParen,
    RParen,
    Comma,
    Semicolon,

    LBracket,
    RBracket,
    LBrace,
    RBrace,
    PipeMinus, // |-

    Func(FuncName),
    Const(ConstName),
    Assign, // :=

    MaxOp, // >?
    MinOp, // <?

    UntilKeyword,
}

/// One C compiler's `ELEMENT` table.
///
/// C has three separate compilers — `postfix.c` (calc/calcout/swait),
/// `sCalcPostfix.c` (scalcout) and `aCalcPostfix.c` (acalcout) — and each owns
/// its own `ELEMENT` table. The table IS the lexer: `get_element` walks it and
/// consumes the first entry that prefixes the remaining infix text; text that
/// matches no entry is never lexed, so `postfix()` stops with
/// `CALC_ERR_SYNTAX` — a COMPILE error (`CLCV != 0`, at record init or at a
/// CALC-field put), not a runtime one.
///
/// This type is that table. It is the single owner of "which symbols this
/// engine has": a symbol absent here cannot be produced by the tokenizer, so
/// no downstream stage needs to filter one out. The previous shape — one
/// shared symbol set plus an opcode-level gate whose `Core(_) => true` arm
/// waved everything through — is what let `FMOD`, `>>>`, the operands `Q`..`U`
/// and `MM`..`UU`, and aCalc `INF`/`NAN` reach engines whose C table has no
/// such element.
///
/// Verified against the real compilers: `postfix.c` (R7-3), `sCalcPostfix.c`
/// and `aCalcPostfix.c` built standalone from their sources and asked. Every
/// symbol below compiles in that engine; every symbol omitted answers
/// `CALC_ERR_SYNTAX` (11).
struct ElementTable {
    /// The table's named elements, `name -> token`, in C's two lists: the
    /// `operands[]` searched while an operand is needed and the `operators[]`
    /// searched after one (`get_element(opnd, ...)`). `-` is in both. Matched case-insensitively
    /// (C `epicsStrnCaseCmp`), longest match wins, with NO identifier boundary:
    /// C lexes `LOG2` as `LOG` then the literal `2`, and `AANDB` as `AA AND B`.
    ///
    /// Symbols carry the C table's `code` column, which is why the same name
    /// can map to different tokens per engine: `DBL` is `TO_DOUBLE` in both
    /// sCalc (string->double) and aCalc (array->double), and those are
    /// different opcodes.
    operands: &'static [(&'static str, Token)],
    operators: &'static [(&'static str, Token)],
    /// Highest single-letter operand: `U` (base, `FETCH_A`..`FETCH_U`,
    /// CALCPERFORM_NARGS = 21) or `P` (sCalc/aCalc, `FETCH_A`..`FETCH_P`).
    last_var: u8,
    /// Highest double-letter operand (`FETCH_AA`..`FETCH_LL`), or `None` for
    /// base, whose table has no double-letter operand at all.
    last_double_var: Option<u8>,
    /// True when the table has `LITERAL_STRING` elements (`"` and `'`) —
    /// sCalcPostfix.c:97-98 only.
    string_literals: bool,
    /// The table's word-shaped `LITERAL_OPERAND` elements: `INF` and `NAN` in
    /// base (`postfix.c:111,125`) and sCalc (`sCalcPostfix.c:149,167`), and
    /// NOTHING in aCalc, whose table has neither — `INF` lexes there as the
    /// operands `I`, `N`, `F`.
    ///
    /// These are elements, not constants. A `LITERAL_OPERAND` name only says
    /// WHERE a literal starts: C rewinds past the name it just matched
    /// (`psrc -= strlen(pel->name)`, postfix.c:258 / sCalcPostfix.c:491) and
    /// re-scans from the first character with strtod, so strtod alone decides
    /// how far the literal runs. `INFINITY` is a literal `inf` because strtod
    /// eats all eight characters; `INFO` is CALC_ERR_SYNTAX because strtod eats
    /// `INF` and the `O` left behind matches no element.
    literal_words: &'static [&'static str],
    /// Which C compiler's literal reader this table belongs to.
    literals: LiteralReader,
    /// The evaluator's value-stack size, which is also the compile-time ceiling
    /// on `runtime_depth`: each compiler rejects an expression whose peak depth
    /// would overrun the stack its OWN `*Perform` allocates —
    /// `if (runtime_depth >= <SIZE>) *perror = CALC_ERR_OVERFLOW`
    /// (`postfix.c:469`, `sCalcPostfix.c:825`, `aCalcPostfix.c:755`).
    ///
    /// The three sizes differ (`CALCPERFORM_STACK` 80, `SCALC_STACKSIZE` 30,
    /// `ACALC_STACKSIZE` 20), so the limit belongs to the FLAVOUR, next to the
    /// table that spells its elements — not to the shared compiler, where one
    /// literal 30 rejected base expressions C accepts (a depth-35 CALC is a
    /// database-load failure on the port, `VAL=35` in C) and accepted aCalc
    /// expressions C rejects.
    stack_size: i32,
    terminator: Terminator,
    store: StoreRule,
}

/// Where C's `STORE_OPERATOR` case accepts a `:=`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum StoreRule {
    /// base `postfix.c:296-300`: only with an empty operator stack, so
    /// `A:=B:=C` and `(A:=1)` are CALC_ERR_BAD_ASSIGNMENT.
    TopLevel,
    /// `sCalcPostfix.c:515-567`, `aCalcPostfix.c:483-531`: wherever the
    /// fetch it rewrites is.
    Anywhere,
}

impl StoreRule {
    /// The `:=` row's in-stack priority: 0 in `postfix.c:162`, 1 in
    /// `sCalcPostfix.c:226` / `aCalcPostfix.c:223`. It decides whether a
    /// later `?` or `:`, which pops only strictly higher entries, flushes it.
    pub(crate) fn in_stack_pri(self) -> u8 {
        match self {
            StoreRule::TopLevel => 0,
            StoreRule::Anywhere => 1,
        }
    }
}

/// What C's `EXPR_TERMINATOR` case does at a `;`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Terminator {
    /// base `postfix.c:439-456`: the `;` ends a whole statement — an open `(`
    /// is CALC_ERR_PAREN_OPEN, an open `?` CALC_ERR_CONDITIONAL, and more than
    /// one value left CALC_ERR_TOOMANY.
    Statement,
    /// `sCalcPostfix.c:786-800`, `aCalcPostfix.c:730-741`: flush down to the
    /// innermost `(` and check nothing, so `A?(0;C:=3.3):B` compiles.
    Flush,
}

/// The `LITERAL_OPERAND` words shared by base and sCalc. aCalc's table has no
/// such element, so it gets `&[]`.
static INF_NAN: &[&str] = &["INF", "NAN"];

/// C's two literal readers. The two compilers differ on BOTH halves of reading a
/// literal, and they differ together — so this is one choice, not two knobs that
/// could be set inconsistently.
#[derive(Clone, Copy, PartialEq)]
enum LiteralReader {
    /// base `postfix.c:261-288`.
    ///
    /// A double goes through `epicsParseDouble`, which FAILS on `errno ==
    /// ERANGE` (`epicsStdlib.c:164`) — so a literal naming a number the format
    /// cannot hold is `CALC_ERR_BAD_LITERAL`, not an infinity or a zero.
    ///
    /// Hex has an element of its own (`{"0X", ..., LITERAL_INT}`, `postfix.c:79`)
    /// parsed with `epicsParseUInt32` (`:283`) — a 32-bit unsigned value, and a
    /// bad literal for anything wider.
    ParseDouble,
    /// sCalc `sCalcPostfix.c:492` / aCalc `aCalcPostfix.c:462`.
    ///
    /// A bare `epicsStrtod`, whose only failure is "converted nothing"
    /// (`pnext == psrc`): `errno` is never read, so `1e400` compiles to an
    /// infinity and `1e-400` to a zero.
    ///
    /// Neither table has a `0X` element. `0x1F` matches the `{"0"}` element, and
    /// `LITERAL_OPERAND` re-scans from the symbol start with the same strtod —
    /// which parses hex itself, at full double width.
    Strtod,
}

/// base `postfix.c:73-179`.
static BASE_TABLE: ElementTable = ElementTable {
    operands: &[
        ("ABS", Token::Func(FuncName::Abs)),
        ("ACOS", Token::Func(FuncName::Acos)),
        ("ASIN", Token::Func(FuncName::Asin)),
        ("ATAN", Token::Func(FuncName::Atan)),
        ("ATAN2", Token::Func(FuncName::Atan2)),
        ("CEIL", Token::Func(FuncName::Ceil)),
        ("COS", Token::Func(FuncName::Cos)),
        ("COSH", Token::Func(FuncName::Cosh)),
        ("D2R", Token::Const(ConstName::D2R)),
        ("EXP", Token::Func(FuncName::Exp)),
        ("FINITE", Token::Func(FuncName::Finite)),
        ("FLOOR", Token::Func(FuncName::Floor)),
        ("FMOD", Token::Func(FuncName::Fmod)),
        ("ISINF", Token::Func(FuncName::IsInf)),
        ("ISNAN", Token::Func(FuncName::IsNan)),
        ("LN", Token::Func(FuncName::Ln)),
        ("LOG", Token::Func(FuncName::Log10)),
        ("LOGE", Token::Func(FuncName::LogE)),
        ("MAX", Token::Func(FuncName::Max)),
        ("MIN", Token::Func(FuncName::Min)),
        ("NINT", Token::Func(FuncName::Nint)),
        ("NOT", Token::Func(FuncName::Not)),
        ("PI", Token::Const(ConstName::Pi)),
        ("R2D", Token::Const(ConstName::R2D)),
        ("RNDM", Token::Rndm),
        ("SIN", Token::Func(FuncName::Sin)),
        ("SINH", Token::Func(FuncName::Sinh)),
        ("SQR", Token::Func(FuncName::Sqr)),
        ("SQRT", Token::Func(FuncName::Sqrt)),
        ("TAN", Token::Func(FuncName::Tan)),
        ("TANH", Token::Func(FuncName::Tanh)),
        ("VAL", Token::FetchVal),
        ("(", Token::LParen),
        ("-", Token::Minus),
        ("!", Token::Bang),
        ("~", Token::Tilde),
    ],
    operators: &[
        // The word forms of the BITWISE operators: C gives `AND`/`OR`/`XOR` the
        // codes `BIT_AND`/`BIT_OR`/`BIT_EXCL_OR`, the same codes `&`/`|`/`^`
        // carry. They are not `&&`/`||`.
        ("AND", Token::BitAnd),
        ("OR", Token::BitOr),
        ("XOR", Token::BitXor),
        // Operators, postfix.c:145-179.
        ("!=", Token::Ne),
        ("#", Token::Ne),
        ("%", Token::Percent),
        ("&", Token::BitAnd),
        ("&&", Token::AndAnd),
        (")", Token::RParen),
        ("*", Token::Star),
        ("**", Token::DoubleStar),
        ("+", Token::Plus),
        (",", Token::Comma),
        ("-", Token::Minus),
        ("/", Token::Slash),
        (":", Token::Colon),
        (":=", Token::Assign),
        (";", Token::Semicolon),
        ("<", Token::Lt),
        ("<<", Token::Shl),
        ("<=", Token::Le),
        ("=", Token::Eq),
        ("==", Token::Eq),
        (">", Token::Gt),
        (">=", Token::Ge),
        (">>", Token::Shr),
        (">>>", Token::ShrLogical),
        ("?", Token::Question),
        ("^", Token::Caret),
        ("|", Token::BitOr),
        ("||", Token::OrOr),
    ],
    last_var: b'U' - b'A',
    last_double_var: None,
    string_literals: false,
    literal_words: INF_NAN,
    literals: LiteralReader::ParseDouble,
    // `postfix.h:31` — `#define CALCPERFORM_STACK 80`.
    stack_size: 80,
    terminator: Terminator::Statement,
    store: StoreRule::TopLevel,
};

/// synApps `sCalcPostfix.c:97-215`.
static SCALC_TABLE: ElementTable = ElementTable {
    operands: &[
        ("ABS", Token::Func(FuncName::Abs)),
        ("ACOS", Token::Func(FuncName::Acos)),
        ("ADD_XOR8", Token::Func(FuncName::AddXor8)),
        ("AMODBUS", Token::Func(FuncName::AModBus)),
        ("ASIN", Token::Func(FuncName::Asin)),
        ("ATAN", Token::Func(FuncName::Atan)),
        ("ATAN2", Token::Func(FuncName::Atan2)),
        ("BYTE", Token::Func(FuncName::Byte)),
        ("CEIL", Token::Func(FuncName::Ceil)),
        ("COS", Token::Func(FuncName::Cos)),
        ("COSH", Token::Func(FuncName::Cosh)),
        ("CRC16", Token::Func(FuncName::Crc16)),
        ("DBL", Token::Func(FuncName::Dbl)),
        ("D2R", Token::Const(ConstName::D2R)),
        ("ESC", Token::Func(FuncName::Esc)),
        // The `$`-spellings are the SAME elements, not new ones — C lists each
        // twice with an identical opcode row (sCalcPostfix.c:136,173-194).
        ("$E", Token::Func(FuncName::Esc)),
        ("$P", Token::Func(FuncName::Printf)),
        ("$R", Token::Func(FuncName::BinRead)),
        ("$S", Token::Func(FuncName::Sscanf)),
        ("$T", Token::Func(FuncName::TrEsc)),
        ("$W", Token::Func(FuncName::BinWrite)),
        ("EXP", Token::Func(FuncName::Exp)),
        ("FINITE", Token::Func(FuncName::Finite)),
        ("FLOOR", Token::Func(FuncName::Floor)),
        // sCalcPostfix.c:150 — `INT` is an ALIAS of `NINT`: it rounds.
        ("INT", Token::Func(FuncName::Int)),
        ("ISINF", Token::Func(FuncName::IsInf)),
        ("ISNAN", Token::Func(FuncName::IsNan)),
        ("LEN", Token::Func(FuncName::Len)),
        ("LN", Token::Func(FuncName::Ln)),
        ("LOG", Token::Func(FuncName::Log10)),
        ("LOGE", Token::Func(FuncName::LogE)),
        ("LRC", Token::Func(FuncName::Lrc)),
        ("MAX", Token::Func(FuncName::Max)),
        ("MIN", Token::Func(FuncName::Min)),
        ("MODBUS", Token::Func(FuncName::ModBus)),
        ("NINT", Token::Func(FuncName::Nint)),
        ("NOT", Token::Func(FuncName::Not)),
        ("NRNDM", Token::Nrndm),
        ("PI", Token::Const(ConstName::Pi)),
        ("PRINTF", Token::Func(FuncName::Printf)),
        ("R2D", Token::Const(ConstName::R2D)),
        ("R2S", Token::Const(ConstName::R2S)),
        ("S2R", Token::Const(ConstName::S2R)),
        // sCalcPostfix.c:180 `{"READ", ..., BIN_READ}` — the C symbol is
        // `READ`, not `BIN_READ`.
        ("READ", Token::Func(FuncName::BinRead)),
        ("RNDM", Token::Rndm),
        ("SIN", Token::Func(FuncName::Sin)),
        ("SINH", Token::Func(FuncName::Sinh)),
        ("SQR", Token::Func(FuncName::Sqr)),
        ("SQRT", Token::Func(FuncName::Sqrt)),
        ("SSCANF", Token::Func(FuncName::Sscanf)),
        ("STR", Token::Func(FuncName::Str)),
        ("SVAL", Token::FetchSval),
        // sCalcPostfix.c:99-100 — sCalc has the dynamic-argument fetches too, at
        // the same priorities as aCalc's (UNARY_OPERATOR, 9/10). `@x` is the
        // scalar argument x indexes, `@@x` the STRING argument.
        ("@", Token::Func(FuncName::SDynFetch)),
        ("@@", Token::Func(FuncName::SDynSFetch)),
        ("TAN", Token::Func(FuncName::Tan)),
        ("TANH", Token::Func(FuncName::Tanh)),
        ("TR_ESC", Token::Func(FuncName::TrEsc)),
        ("UNTIL", Token::UntilKeyword),
        ("VAL", Token::FetchVal),
        ("WRITE", Token::Func(FuncName::BinWrite)),
        ("XOR8", Token::Func(FuncName::Xor8)),
        ("(", Token::LParen),
        ("-", Token::Minus),
        ("!", Token::Bang),
        ("~", Token::Tilde),
    ],
    operators: &[
        // The word forms of the BITWISE operators: C gives `AND`/`OR`/`XOR` the
        // codes `BIT_AND`/`BIT_OR`/`BIT_EXCL_OR`, the same codes `&`/`|`/`^`
        // carry. They are not `&&`/`||`.
        ("AND", Token::BitAnd),
        ("OR", Token::BitOr),
        ("XOR", Token::BitXor),
        // Operators, sCalcPostfix.c:217-255.
        ("!=", Token::Ne),
        ("#", Token::Ne),
        ("%", Token::Percent),
        ("&", Token::BitAnd),
        ("&&", Token::AndAnd),
        (")", Token::RParen),
        ("[", Token::LBracket),
        ("]", Token::RBracket),
        ("{", Token::LBrace),
        ("}", Token::RBrace),
        ("*", Token::Star),
        ("**", Token::DoubleStar),
        ("+", Token::Plus),
        (",", Token::Comma),
        ("-", Token::Minus),
        ("/", Token::Slash),
        (":", Token::Colon),
        (":=", Token::Assign),
        (";", Token::Semicolon),
        ("<", Token::Lt),
        ("<<", Token::Shl),
        ("<=", Token::Le),
        ("=", Token::Eq),
        ("==", Token::Eq),
        (">", Token::Gt),
        (">=", Token::Ge),
        (">>", Token::Shr),
        ("?", Token::Question),
        ("^", Token::Caret),
        ("|", Token::BitOr),
        ("||", Token::OrOr),
        ("|-", Token::PipeMinus),
        // `-|` is NOT a new operator: C gives it the SUB opcode, the same one
        // plain `-` has (sCalcPostfix.c:243 vs :237). It is a second spelling
        // that says "subtract the FIRST occurrence" out loud, the behaviour `-`
        // already has. Compiled sCalc, AA="abcabc" BB="bc": AA-|BB and AA-BB are
        // both "aabc"; only AA|-BB ("abca") differs.
        ("-|", Token::Minus),
        (">?", Token::MaxOp),
        ("<?", Token::MinOp),
    ],
    last_var: b'P' - b'A',
    last_double_var: Some(b'L' - b'A'),
    string_literals: true,
    literal_words: INF_NAN,
    literals: LiteralReader::Strtod,
    // `sCalcPostfixPvt.h:21` — `#define SCALC_STACKSIZE 30`.
    stack_size: 30,
    terminator: Terminator::Flush,
    store: StoreRule::Anywhere,
};

/// synApps `aCalcPostfix.c:99-224`.
static ACALC_TABLE: ElementTable = ElementTable {
    operands: &[
        ("ABS", Token::Func(FuncName::Abs)),
        ("ACOS", Token::Func(FuncName::Acos)),
        ("AMAX", Token::Func(FuncName::AMax)),
        ("AMIN", Token::Func(FuncName::AMin)),
        ("ARNDM", Token::ARndm),
        ("ARR", Token::Func(FuncName::Arr)),
        ("ASIN", Token::Func(FuncName::Asin)),
        ("ATAN", Token::Func(FuncName::Atan)),
        ("ATAN2", Token::Func(FuncName::Atan2)),
        ("AVG", Token::Func(FuncName::Avg)),
        // aCalc-only elements the port had never lexed (aCalcPostfix.c:93-94,
        // 118, 153-154, 199).
        ("@", Token::Func(FuncName::DynFetch)),
        ("@@", Token::Func(FuncName::DynAFetch)),
        ("AVAL", Token::FetchAval),
        ("ANEG", Token::Func(FuncName::ANeg)),
        ("APOS", Token::Func(FuncName::APos)),
        ("LEN", Token::Func(FuncName::ALenNoop)),
        ("CAT", Token::Func(FuncName::Cat)),
        ("CEIL", Token::Func(FuncName::Ceil)),
        ("COS", Token::Func(FuncName::Cos)),
        ("COSH", Token::Func(FuncName::Cosh)),
        ("CUM", Token::Func(FuncName::Cum)),
        // aCalcPostfix.c:133 `{"DBL", ..., TO_DOUBLE}` — the array engine's
        // array->double conversion. The port had this op under an invented
        // `ATOD` symbol, which no C table has.
        ("DBL", Token::Func(FuncName::AToD)),
        ("DERIV", Token::Func(FuncName::Deriv)),
        ("D2R", Token::Const(ConstName::D2R)),
        ("EXP", Token::Func(FuncName::Exp)),
        ("FINITE", Token::Func(FuncName::Finite)),
        ("FITMPOLY", Token::Func(FuncName::FitMPoly)),
        ("FITMQ", Token::Func(FuncName::FitMQ)),
        ("FITPOLY", Token::Func(FuncName::FitPoly)),
        ("FITQ", Token::Func(FuncName::FitQ)),
        ("FLOOR", Token::Func(FuncName::Floor)),
        ("FWHM", Token::Func(FuncName::FwhmFunc)),
        ("INT", Token::Func(FuncName::Int)),
        ("ISINF", Token::Func(FuncName::IsInf)),
        ("ISNAN", Token::Func(FuncName::IsNan)),
        ("IX", Token::ConstIx),
        ("IXMAX", Token::Func(FuncName::IxMax)),
        ("IXMIN", Token::Func(FuncName::IxMin)),
        ("IXNZ", Token::Func(FuncName::IxNz)),
        ("IXZ", Token::Func(FuncName::IxZ)),
        ("LN", Token::Func(FuncName::Ln)),
        ("LOG", Token::Func(FuncName::Log10)),
        ("LOGE", Token::Func(FuncName::LogE)),
        ("MAX", Token::Func(FuncName::Max)),
        ("MIN", Token::Func(FuncName::Min)),
        ("NDERIV", Token::Func(FuncName::NDeriv)),
        ("NINT", Token::Func(FuncName::Nint)),
        ("NOT", Token::Func(FuncName::Not)),
        ("NRNDM", Token::Nrndm),
        ("NSMOO", Token::Func(FuncName::NSmoo)),
        ("PI", Token::Const(ConstName::Pi)),
        ("R2D", Token::Const(ConstName::R2D)),
        ("R2S", Token::Const(ConstName::R2S)),
        ("S2R", Token::Const(ConstName::S2R)),
        ("RNDM", Token::Rndm),
        ("SIN", Token::Func(FuncName::Sin)),
        ("SINH", Token::Func(FuncName::Sinh)),
        ("SMOO", Token::Func(FuncName::Smoo)),
        ("SQR", Token::Func(FuncName::Sqr)),
        ("SQRT", Token::Func(FuncName::Sqrt)),
        ("STD", Token::Func(FuncName::Std)),
        ("SUM", Token::Func(FuncName::Sum)),
        ("TAN", Token::Func(FuncName::Tan)),
        ("TANH", Token::Func(FuncName::Tanh)),
        ("UNTIL", Token::UntilKeyword),
        ("VAL", Token::FetchVal),
        ("(", Token::LParen),
        ("-", Token::Minus),
        ("!", Token::Bang),
        ("~", Token::Tilde),
    ],
    operators: &[
        // The word forms of the BITWISE operators: C gives `AND`/`OR`/`XOR` the
        // codes `BIT_AND`/`BIT_OR`/`BIT_EXCL_OR`, the same codes `&`/`|`/`^`
        // carry. They are not `&&`/`||`.
        ("AND", Token::BitAnd),
        ("OR", Token::BitOr),
        ("XOR", Token::BitXor),
        // Operators, aCalcPostfix.c:226-262. No `|-` (that is sCalc's
        // SUBLAST), no `>>>`.
        ("!=", Token::Ne),
        ("#", Token::Ne),
        ("%", Token::Percent),
        ("&", Token::BitAnd),
        ("&&", Token::AndAnd),
        (")", Token::RParen),
        ("[", Token::LBracket),
        ("]", Token::RBracket),
        ("{", Token::LBrace),
        ("}", Token::RBrace),
        ("*", Token::Star),
        ("**", Token::DoubleStar),
        ("+", Token::Plus),
        (",", Token::Comma),
        ("-", Token::Minus),
        ("/", Token::Slash),
        (":", Token::Colon),
        (":=", Token::Assign),
        (";", Token::Semicolon),
        ("<", Token::Lt),
        ("<<", Token::Shl),
        ("<=", Token::Le),
        ("=", Token::Eq),
        ("==", Token::Eq),
        (">", Token::Gt),
        (">=", Token::Ge),
        (">>", Token::Shr),
        ("?", Token::Question),
        ("^", Token::Caret),
        ("|", Token::BitOr),
        ("||", Token::OrOr),
        (">?", Token::MaxOp),
        ("<?", Token::MinOp),
    ],
    last_var: b'P' - b'A',
    last_double_var: Some(b'L' - b'A'),
    string_literals: false,
    // aCalcPostfix.c:98-108 — the table's only LITERAL_OPERANDs are `.` and the
    // digits. It has no `INF` and no `NAN` element.
    literal_words: &[],
    literals: LiteralReader::Strtod,
    // `aCalcPostfixPvt.h:22` — `#define ACALC_STACKSIZE 20`.
    stack_size: 20,
    terminator: Terminator::Flush,
    store: StoreRule::Anywhere,
};

fn table_for(kind: &ExprKind) -> &'static ElementTable {
    match kind {
        ExprKind::Numeric => &BASE_TABLE,
        ExprKind::String => &SCALC_TABLE,
        ExprKind::Array => &ACALC_TABLE,
    }
}

/// The peak `runtime_depth` this flavour's compiler will accept — C's
/// `if (runtime_depth >= <its stack size>) *perror = CALC_ERR_OVERFLOW`.
///
/// Read from the flavour's own [`ElementTable`], because that is where the C
/// difference lives: `postfix.c` compiles against an 80-deep `calcPerform`
/// stack, `sCalcPostfix.c` against a 30-deep one, `aCalcPostfix.c` against a
/// 20-deep one.
pub(crate) fn runtime_stack_size(kind: ExprKind) -> i32 {
    table_for(&kind).stack_size
}

pub(crate) fn expr_terminator(kind: ExprKind) -> Terminator {
    table_for(&kind).terminator
}

pub(crate) fn store_rule(kind: ExprKind) -> StoreRule {
    table_for(&kind).store
}

/// The element reader `postfix::compile` pulls from, one element at a time,
/// because which C list an element comes from depends on the parser's state.
pub(crate) struct Lexer<'a>(Tokenizer<'a>);

struct Tokenizer<'a> {
    input: &'a [u8],
    pos: usize,
    table: &'static ElementTable,
}

impl<'a> Tokenizer<'a> {
    fn new(input: &'a str, table: &'static ElementTable) -> Self {
        Tokenizer {
            input: input.as_bytes(),
            pos: 0,
            table,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn advance(&mut self) -> Option<u8> {
        let b = self.input.get(self.pos).copied()?;
        self.pos += 1;
        Some(b)
    }

    fn skip_whitespace(&mut self) {
        while self.pos < self.input.len()
            && crate::runtime::stdlib::c_isspace(self.input[self.pos] as char)
        {
            self.pos += 1;
        }
    }

    /// C `LITERAL_STRING` (`sCalcPostfix.c:803-812`), the whole of it:
    ///
    /// ```c
    /// c = psrc[-1];                              /* the " or ' that opened it */
    /// while (*psrc != c && *psrc) *pout++ = *psrc++;
    /// *pout++ = '\0';
    /// if (*psrc) psrc++;                         /* step over the close quote */
    /// ```
    ///
    /// A byte-for-byte copy up to the matching quote or the end of the source.
    /// Three consequences, all of them C's:
    ///
    /// * **No backslash escapes.** The literal keeps its backslashes, and `$T` /
    ///   `TR_ESC` is the ONLY thing that translates them — which is precisely why
    ///   sCalc has that operator. Pre-translating here made `$T` a double
    ///   translation and changed the bytes on every path that does not translate:
    ///   compiled C answers `BYTE("\t")` = 92 (the backslash), `LEN("a\tb")` = 4,
    ///   and `PRINTF("%d\n",5)` = the 3 bytes `5\n` — a literal backslash and `n`,
    ///   which is exactly what a serial-device scalcout then hands to `$T`.
    /// * **The quote character cannot be embedded.** `"a\"b"` closes at the `\"`,
    ///   leaves `b"` behind, and C stops with CALC_ERR_SYNTAX. The port used to
    ///   accept it.
    /// * **An unterminated literal is not an error.** The loop simply stops at the
    ///   NUL and `if (*psrc) psrc++` does nothing. Compiled C: `"abc` compiles and
    ///   evaluates to `abc`.
    fn read_string_literal(&mut self, quote: u8) -> Vec<u8> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b == quote {
                break;
            }
            self.pos += 1;
        }
        let raw = self.input[start..self.pos].to_vec();
        // `if (*psrc) psrc++` — step over the close quote if there was one.
        if self.peek() == Some(quote) {
            self.pos += 1;
        }
        raw
    }

    /// C `get_element`: consume the longest table symbol that prefixes the
    /// remaining text.
    ///
    /// C walks its alphabetically-sorted `ELEMENT` table backwards and takes
    /// the FIRST entry that prefixes the text (`epicsStrnCaseCmp`); because a
    /// contained name always sorts before the name containing it, that is a
    /// longest-prefix match — with NO identifier boundary. Calc has no
    /// identifiers, so there is nothing for a boundary to protect: C lexes
    /// `LOG2` as `LOG` then the literal `2`.
    fn match_symbol(&mut self, symbols: &[(&'static str, Token)]) -> Option<Token> {
        let rem = &self.input[self.pos..];
        let mut best: Option<(usize, &Token)> = None;
        for (name, tok) in symbols {
            if !starts_with_ci(rem, name) {
                continue;
            }
            let n = name.len();
            if best.is_none_or(|(blen, _)| n > blen) {
                best = Some((n, tok));
            }
        }
        let (len, tok) = best?;
        self.pos += len;
        Some(tok.clone())
    }

    /// The table's `OPERAND` entries `FETCH_A..` / `FETCH_AA..`, which C spells
    /// out one element per letter. A letter past the table's last operand is
    /// not in the table at all: sCalc/aCalc stop at `P` (single) and `LL`
    /// (double), so `Q` and `MM` are `CALC_ERR_SYNTAX` there, while base has
    /// `A`..`U` and no double-letter operand whatsoever.
    fn match_var(&mut self) -> Option<Token> {
        let rem = &self.input[self.pos..];
        if let Some(last) = self.table.last_double_var {
            if rem.len() >= 2 {
                let a = rem[0].to_ascii_uppercase();
                let b = rem[1].to_ascii_uppercase();
                if a == b && a.is_ascii_uppercase() && a - b'A' <= last {
                    self.pos += 2;
                    return Some(Token::DoubleVar(a - b'A'));
                }
            }
        }
        let c = rem.first()?.to_ascii_uppercase();
        if c.is_ascii_uppercase() && c - b'A' <= self.table.last_var {
            self.pos += 1;
            return Some(Token::Var(c - b'A'));
        }
        None
    }

    /// Does a `LITERAL_OPERAND` element of this table start here?
    ///
    /// Every table has `.` and `0`..`9` (postfix.c:77-88, sCalcPostfix.c:104-114,
    /// aCalcPostfix.c:98-108); base and sCalc add the words `INF` and `NAN`. This
    /// decides only WHERE the literal starts — `read_literal` decides how far it
    /// runs, exactly as C's rewind-then-strtod does.
    fn at_literal(&self) -> bool {
        let rem = &self.input[self.pos..];
        match rem.first() {
            None => false,
            Some(b) if b.is_ascii_digit() || *b == b'.' => true,
            _ => self
                .table
                .literal_words
                .iter()
                .any(|w| starts_with_ci(rem, w)),
        }
    }

    /// C's `LITERAL_OPERAND` case: rewind to the element's first character and
    /// hand the text to the table's reader ([`LiteralReader`]). The literal
    /// covers exactly the text strtod consumes, and strtod consuming NOTHING is
    /// `CALC_ERR_BAD_LITERAL` (C tests `pnext == psrc`) — which is why a lone `.`
    /// is a bad literal rather than a syntax error.
    ///
    /// The scan itself is `strtod::strtod`, shared with sCalc's `atof` coercion.
    /// A sign or leading whitespace never reaches it here: `at_literal` only
    /// fires on a digit, a `.`, or one of the table's literal words — which is
    /// also why an out-of-range literal is only ever the MAGNITUDE: `-1e400` is
    /// the unary minus applied to the bad literal `1e400`.
    fn read_literal(&mut self) -> Result<f64, CalcError> {
        let start = self.pos;
        let rem = &self.input[start..];

        let is_hex = rem.first() == Some(&b'0')
            && matches!(rem.get(1), Some(b'x' | b'X'))
            && rem.get(2).is_some_and(u8::is_ascii_hexdigit);
        if is_hex && self.table.literals == LiteralReader::ParseDouble {
            let end = 2 + rem[2..]
                .iter()
                .take_while(|b| b.is_ascii_hexdigit())
                .count();
            self.pos = start + end;
            let digits = std::str::from_utf8(&rem[2..end]).unwrap();
            // C stores the `epicsUInt32` bits as `LITERAL_INT` (`postfix.c:283-289`)
            // and `calcPerform` reads them back as `epicsInt32`
            // (`calcPerform.c:68-71`): `0xFFFFFFFF` is -1.
            return u32::from_str_radix(digits, 16)
                .map(|v| f64::from(v as i32))
                .map_err(|_| CalcError::BadLiteral);
        }

        let n = strtod::strtod(rem);
        if n.len == 0 {
            return Err(CalcError::BadLiteral);
        }
        // `epicsParseDouble` returns ERANGE as a failure and base's compiler
        // turns any failure into CALC_ERR_BAD_LITERAL (`postfix.c:263-265`);
        // `epicsStrtod` never looks at errno, so sCalc/aCalc take the infinity
        // or the zero. Compiled: base rejects `1e400`, `1e-400` and `2.2e-308`
        // and accepts `2.3e-308`, `1e308` and `0e999`; sCalcPostfix accepts all
        // six.
        if n.erange && self.table.literals == LiteralReader::ParseDouble {
            return Err(CalcError::BadLiteral);
        }
        self.pos += n.len;
        Ok(n.value)
    }
}

/// C `epicsStrnCaseCmp(text, name, strlen(name)) == 0`: does `name` prefix
/// `text`, ignoring case? This is how `get_element` matches every element.
fn starts_with_ci(text: &[u8], name: &str) -> bool {
    strtod::starts_with_ci(text, name.as_bytes())
}

impl<'a> Lexer<'a> {
    pub(crate) fn new(input: &'a str, kind: ExprKind) -> Self {
        Lexer(Tokenizer::new(input, table_for(&kind)))
    }

    /// C `get_element(opnd, ...)`: the next element from the `operands[]` list
    /// when `operand` is set, else from `operators[]`; `None` at the end of the
    /// text. Text that matches no element of THAT list is `CalcError::Syntax`,
    /// at this position — C's loop stops there and finds `*psrc != '\0'`.
    pub(crate) fn next(&mut self, operand: bool) -> Result<Option<Token>, CalcError> {
        let t = &mut self.0;
        t.skip_whitespace();
        let Some(b) = t.peek() else {
            return Ok(None);
        };
        if !operand {
            return t
                .match_symbol(t.table.operators)
                .map(Some)
                .ok_or(CalcError::Syntax);
        }

        // LITERAL_OPERAND. Checked before the other elements because C's
        // longest-match `get_element` cannot prefer one: no table has a
        // non-literal element that starts with a digit, a `.`, `INF` or `NAN`.
        if t.at_literal() {
            return t.read_literal().map(|n| Some(Token::Number(n)));
        }

        // LITERAL_STRING: sCalcPostfix.c:97-98 only.
        if (b == b'"' || b == b'\'') && t.table.string_literals {
            t.advance();
            return Ok(Some(Token::StringLiteral(t.read_string_literal(b))));
        }

        if let Some(tok) = t.match_symbol(t.table.operands) {
            return Ok(Some(tok));
        }
        t.match_var().map(Some).ok_or(CalcError::Syntax)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pull one element per flag: `o` reads from `operands[]`, `x` from
    /// `operators[]`. Stops at the first error.
    fn lex(kind: ExprKind, expr: &str, lists: &str) -> Result<Vec<Token>, CalcError> {
        let mut lexer = Lexer::new(expr, kind);
        let mut out = Vec::new();
        for list in lists.chars() {
            match lexer.next(list == 'o')? {
                Some(t) => out.push(t),
                None => break,
            }
        }
        assert_eq!(lexer.next(true), Ok(None), "{expr:?} not fully read");
        Ok(out)
    }
    fn base(expr: &str, lists: &str) -> Result<Vec<Token>, CalcError> {
        lex(ExprKind::Numeric, expr, lists)
    }
    fn scalc(expr: &str, lists: &str) -> Result<Vec<Token>, CalcError> {
        lex(ExprKind::String, expr, lists)
    }
    fn acalc(expr: &str, lists: &str) -> Result<Vec<Token>, CalcError> {
        lex(ExprKind::Array, expr, lists)
    }

    #[test]
    fn test_basic_tokens() {
        assert_eq!(
            base("A+B*3", "oxoxo").unwrap(),
            vec![
                Token::Var(0),
                Token::Plus,
                Token::Var(1),
                Token::Star,
                Token::Number(3.0)
            ]
        );
    }

    #[test]
    fn test_functions() {
        assert_eq!(
            base("SIN(A)", "ooox").unwrap(),
            vec![
                Token::Func(FuncName::Sin),
                Token::LParen,
                Token::Var(0),
                Token::RParen,
            ]
        );
    }

    #[test]
    fn test_double_vars() {
        assert_eq!(
            scalc("AA+BB", "oxo").unwrap(),
            vec![Token::DoubleVar(0), Token::Plus, Token::DoubleVar(1)]
        );
        // base postfix.c has no double-letter operand: `AA` is `A`, and the
        // second `A` is in no `operators[]` entry.
        assert_eq!(base("AA", "ox"), Err(CalcError::Syntax));
    }

    #[test]
    fn test_constants() {
        assert_eq!(
            base("PI+D2R", "oxo").unwrap(),
            vec![
                Token::Const(ConstName::Pi),
                Token::Plus,
                Token::Const(ConstName::D2R),
            ]
        );
    }

    #[test]
    fn test_case_insensitive() {
        assert_eq!(
            base("sin(a)+Cos(b)", "oooxxooox").unwrap(),
            vec![
                Token::Func(FuncName::Sin),
                Token::LParen,
                Token::Var(0),
                Token::RParen,
                Token::Plus,
                Token::Func(FuncName::Cos),
                Token::LParen,
                Token::Var(1),
                Token::RParen,
            ]
        );
    }

    #[test]
    fn test_assign() {
        assert_eq!(
            base("A:=5", "oxo").unwrap(),
            vec![Token::Var(0), Token::Assign, Token::Number(5.0)]
        );
    }

    #[test]
    fn test_ternary() {
        assert_eq!(
            base("A?B:C", "oxoxo").unwrap(),
            vec![
                Token::Var(0),
                Token::Question,
                Token::Var(1),
                Token::Colon,
                Token::Var(2),
            ]
        );
    }

    #[test]
    fn test_hex() {
        assert_eq!(base("0xFF", "o").unwrap(), vec![Token::Number(255.0)]);
        assert_eq!(scalc("0xFF", "o").unwrap(), vec![Token::Number(255.0)]);
    }

    #[test]
    fn test_float_literal() {
        assert_eq!(base("3.14e2", "o").unwrap(), vec![Token::Number(314.0)]);
    }

    /// The two lists are disjoint except for `-`: `(` and `!` are operands
    /// only, `)` and the binary operators operators only, and sCalc's `-|` is
    /// in `operators[]` alone, so in operand position it is the unary `-`
    /// followed by a `|` that no operand spells.
    #[test]
    fn test_each_position_reads_its_own_list() {
        assert_eq!(base(")", "o"), Err(CalcError::Syntax));
        assert_eq!(base("A(", "ox"), Err(CalcError::Syntax));
        assert_eq!(base("-", "o").unwrap(), vec![Token::Minus]);
        assert_eq!(base("A-", "ox").unwrap(), vec![Token::Var(0), Token::Minus]);
        assert_eq!(scalc("A-|B", "oxo").unwrap()[1], Token::Minus);
        assert_eq!(scalc("-|A", "oo"), Err(CalcError::Syntax));
    }

    #[test]
    fn test_operand_range_per_table() {
        // base: A..U, and no double-letter operand at all.
        assert_eq!(base("U", "o").unwrap(), vec![Token::Var(20)]);
        // sCalc/aCalc: A..P, AA..LL. `Q` is in no operand table, so C
        // `get_element` returns FALSE and `sCalcPostfix` answers
        // CALC_ERR_SYNTAX.
        assert_eq!(scalc("P", "o").unwrap(), vec![Token::Var(15)]);
        assert_eq!(scalc("Q", "o"), Err(CalcError::Syntax));
        assert_eq!(acalc("U", "o"), Err(CalcError::Syntax));
        assert_eq!(scalc("LL", "o").unwrap(), vec![Token::DoubleVar(11)]);
        // `MM` is not a double operand: C lexes `M` and then fails on the
        // second `M` in operator position.
        assert_eq!(scalc("MM", "ox"), Err(CalcError::Syntax));
    }

    #[test]
    fn test_symbols_outside_a_table() {
        // FMOD and `>>>` are base-only symbols. In sCalc/aCalc no entry spells
        // them: `>>>` reads as `>>`, and the `>` left over is in no
        // `operands[]`. The engine-level assertions live in
        // tests/calc_element_tables.rs.
        assert_eq!(base("FMOD", "o").unwrap()[0], Token::Func(FuncName::Fmod));
        assert_eq!(base("A>>>1", "oxo").unwrap()[1], Token::ShrLogical);
        assert_eq!(scalc("A>>>1", "oxo"), Err(CalcError::Syntax));
        // INF/NAN are LITERAL_OPERANDs of base and sCalc; aCalc has neither, so
        // `INF` reads there as the operand I and then fails at `N` in operator
        // position.
        assert_eq!(
            base("INF", "o").unwrap(),
            vec![Token::Number(f64::INFINITY)]
        );
        assert_eq!(
            scalc("INF", "o").unwrap(),
            vec![Token::Number(f64::INFINITY)]
        );
        assert_eq!(acalc("INF", "ox"), Err(CalcError::Syntax));
        // `DBL` is TO_DOUBLE in both synApps tables, but a different opcode in
        // each; base has no such symbol.
        assert_eq!(
            scalc("DBL", "o").unwrap()[0],
            Token::Func(FuncName::Dbl),
            "sCalc DBL is the string->double op"
        );
        assert_eq!(
            acalc("DBL", "o").unwrap()[0],
            Token::Func(FuncName::AToD),
            "aCalc DBL is the array->double op"
        );
    }
}
