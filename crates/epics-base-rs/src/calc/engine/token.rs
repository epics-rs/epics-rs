use super::ExprKind;
use super::error::CalcError;

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
    Ix,
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
    ARndm,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConstName {
    Pi,
    D2R,
    R2D,
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

    StringLiteral(String),

    Plus,
    Minus,
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

    // Keyword operators
    AndKeyword, // AND
    OrKeyword,  // OR

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
    /// The table's named symbols, `name -> token`. Matched case-insensitively
    /// (C `epicsStrnCaseCmp`), longest match wins, with NO identifier boundary:
    /// C lexes `LOG2` as `LOG` then the literal `2`, and `AANDB` as `AA AND B`.
    ///
    /// Symbols carry the C table's `code` column, which is why the same name
    /// can map to different tokens per engine: `DBL` is `TO_DOUBLE` in both
    /// sCalc (string->double) and aCalc (array->double), and those are
    /// different opcodes.
    symbols: &'static [(&'static str, Token)],
    /// Highest single-letter operand: `U` (base, `FETCH_A`..`FETCH_U`,
    /// CALCPERFORM_NARGS = 21) or `P` (sCalc/aCalc, `FETCH_A`..`FETCH_P`).
    last_var: u8,
    /// Highest double-letter operand (`FETCH_AA`..`FETCH_LL`), or `None` for
    /// base, whose table has no double-letter operand at all.
    last_double_var: Option<u8>,
    /// True when the table has `LITERAL_STRING` elements (`"` and `'`) —
    /// sCalcPostfix.c:97-98 only.
    string_literals: bool,
    /// How a numeric literal is parsed once its first character has matched.
    hex: HexLiteral,
}

/// C's two literal-parsing paths, one per table shape.
#[derive(Clone, Copy, PartialEq)]
enum HexLiteral {
    /// base `postfix.c:79` has a `{"0X", ..., LITERAL_INT}` element of its own,
    /// parsed with `epicsParseUInt32` (`postfix.c:283`) — a 32-bit unsigned
    /// value, and `CALC_ERR_BAD_LITERAL` for anything wider.
    Uint32Element,
    /// sCalc/aCalc have no `0X` element. `0x1F` matches the `{"0"}` element,
    /// and `LITERAL_OPERAND` then re-scans from the symbol start with
    /// `epicsStrtod` (sCalcPostfix.c:491, aCalcPostfix.c:493), i.e. C99
    /// `strtod` — which parses hex itself, at full double width.
    Strtod,
}

/// base `postfix.c:73-179`.
static BASE_TABLE: ElementTable = ElementTable {
    symbols: &[
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
        ("INF", Token::Number(f64::INFINITY)),
        ("ISINF", Token::Func(FuncName::IsInf)),
        ("ISNAN", Token::Func(FuncName::IsNan)),
        ("LN", Token::Func(FuncName::Ln)),
        ("LOG", Token::Func(FuncName::Log10)),
        ("LOGE", Token::Func(FuncName::LogE)),
        ("MAX", Token::Func(FuncName::Max)),
        ("MIN", Token::Func(FuncName::Min)),
        ("NINT", Token::Func(FuncName::Nint)),
        ("NAN", Token::Number(f64::NAN)),
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
        ("AND", Token::AndKeyword),
        ("OR", Token::OrKeyword),
        ("XOR", Token::BitXor),
        // Operators, postfix.c:145-179.
        ("!=", Token::Ne),
        ("#", Token::Ne),
        ("%", Token::Percent),
        ("&", Token::BitAnd),
        ("&&", Token::AndAnd),
        ("(", Token::LParen),
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
        ("!", Token::Bang),
        ("~", Token::Tilde),
    ],
    last_var: b'U' - b'A',
    last_double_var: None,
    string_literals: false,
    hex: HexLiteral::Uint32Element,
};

/// synApps `sCalcPostfix.c:97-215`.
static SCALC_TABLE: ElementTable = ElementTable {
    symbols: &[
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
        ("EXP", Token::Func(FuncName::Exp)),
        ("FINITE", Token::Func(FuncName::Finite)),
        ("FLOOR", Token::Func(FuncName::Floor)),
        ("INF", Token::Number(f64::INFINITY)),
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
        ("NAN", Token::Number(f64::NAN)),
        ("NINT", Token::Func(FuncName::Nint)),
        ("NOT", Token::Func(FuncName::Not)),
        ("NRNDM", Token::Nrndm),
        ("PI", Token::Const(ConstName::Pi)),
        ("PRINTF", Token::Func(FuncName::Printf)),
        ("R2D", Token::Const(ConstName::R2D)),
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
        ("TAN", Token::Func(FuncName::Tan)),
        ("TANH", Token::Func(FuncName::Tanh)),
        ("TR_ESC", Token::Func(FuncName::TrEsc)),
        ("UNTIL", Token::UntilKeyword),
        ("VAL", Token::FetchVal),
        ("WRITE", Token::Func(FuncName::BinWrite)),
        ("XOR8", Token::Func(FuncName::Xor8)),
        ("AND", Token::AndKeyword),
        ("OR", Token::OrKeyword),
        ("XOR", Token::BitXor),
        // Operators, sCalcPostfix.c:217-255.
        ("!=", Token::Ne),
        ("#", Token::Ne),
        ("%", Token::Percent),
        ("&", Token::BitAnd),
        ("&&", Token::AndAnd),
        ("(", Token::LParen),
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
        (">?", Token::MaxOp),
        ("<?", Token::MinOp),
        ("!", Token::Bang),
        ("~", Token::Tilde),
    ],
    last_var: b'P' - b'A',
    last_double_var: Some(b'L' - b'A'),
    string_literals: true,
    hex: HexLiteral::Strtod,
};

/// synApps `aCalcPostfix.c:99-224`.
static ACALC_TABLE: ElementTable = ElementTable {
    symbols: &[
        ("ABS", Token::Func(FuncName::Abs)),
        ("ACOS", Token::Func(FuncName::Acos)),
        ("AMAX", Token::Func(FuncName::AMax)),
        ("AMIN", Token::Func(FuncName::AMin)),
        ("ARNDM", Token::Func(FuncName::ARndm)),
        ("ARR", Token::Func(FuncName::Arr)),
        ("ASIN", Token::Func(FuncName::Asin)),
        ("ATAN", Token::Func(FuncName::Atan)),
        ("ATAN2", Token::Func(FuncName::Atan2)),
        ("AVG", Token::Func(FuncName::Avg)),
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
        ("IX", Token::Func(FuncName::Ix)),
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
        ("AND", Token::AndKeyword),
        ("OR", Token::OrKeyword),
        ("XOR", Token::BitXor),
        // Operators, aCalcPostfix.c:226-262. No `|-` (that is sCalc's
        // SUBLAST), no `>>>`.
        ("!=", Token::Ne),
        ("#", Token::Ne),
        ("%", Token::Percent),
        ("&", Token::BitAnd),
        ("&&", Token::AndAnd),
        ("(", Token::LParen),
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
        ("!", Token::Bang),
        ("~", Token::Tilde),
    ],
    last_var: b'P' - b'A',
    last_double_var: Some(b'L' - b'A'),
    string_literals: false,
    hex: HexLiteral::Strtod,
};

fn table_for(kind: &ExprKind) -> &'static ElementTable {
    match kind {
        ExprKind::Numeric => &BASE_TABLE,
        ExprKind::String => &SCALC_TABLE,
        ExprKind::Array => &ACALC_TABLE,
    }
}

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
        while self.pos < self.input.len() && self.input[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn read_string_literal(&mut self, quote: u8) -> Result<String, CalcError> {
        let mut result = String::new();
        loop {
            match self.advance() {
                None => return Err(CalcError::Syntax), // unterminated string
                Some(b) if b == quote => return Ok(result),
                Some(b'\\') => match self.advance() {
                    Some(b'n') => result.push('\n'),
                    Some(b't') => result.push('\t'),
                    Some(b'r') => result.push('\r'),
                    Some(b'\\') => result.push('\\'),
                    Some(b) if b == quote => result.push(b as char),
                    Some(b) => {
                        result.push('\\');
                        result.push(b as char);
                    }
                    None => return Err(CalcError::Syntax),
                },
                Some(b) => result.push(b as char),
            }
        }
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
    fn match_symbol(&mut self) -> Option<Token> {
        let rem = &self.input[self.pos..];
        let mut best: Option<(usize, &Token)> = None;
        for (name, tok) in self.table.symbols {
            let n = name.len();
            if rem.len() < n {
                continue;
            }
            if !rem[..n].eq_ignore_ascii_case(name.as_bytes()) {
                continue;
            }
            if best.map_or(true, |(blen, _)| n > blen) {
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

    /// A `LITERAL_OPERAND` element (`.`, `0`..`9`, and base's own `0X`).
    fn read_number(&mut self) -> Result<f64, CalcError> {
        let start = self.pos;
        let is_hex = self.input[start] == b'0'
            && matches!(self.input.get(start + 1), Some(b'x' | b'X'))
            && self.input.get(start + 2).is_some_and(u8::is_ascii_hexdigit);

        if is_hex && self.table.hex == HexLiteral::Uint32Element {
            // base postfix.c:79 has a `{"0X", ..., LITERAL_INT}` element, and
            // postfix.c:283 parses it with `epicsParseUInt32` — a 32-bit
            // unsigned value; anything wider is CALC_ERR_BAD_LITERAL.
            self.pos = start + 2;
            while self.pos < self.input.len() && self.input[self.pos].is_ascii_hexdigit() {
                self.pos += 1;
            }
            let s = std::str::from_utf8(&self.input[start + 2..self.pos]).unwrap();
            return u32::from_str_radix(s, 16)
                .map(|v| v as f64)
                .map_err(|_| CalcError::BadLiteral);
        }
        if is_hex {
            // sCalc/aCalc have no `0X` element: `0x1F` matches `{"0"}` and is
            // then re-scanned by `epicsStrtod` from the `0`, i.e. C99 strtod,
            // which parses hex at full double width (`0x1FFFFFFFFF` is
            // 137438953471, not a bad literal).
            return Ok(self.read_hex_strtod(start));
        }

        while self.pos < self.input.len()
            && (self.input[self.pos].is_ascii_digit() || self.input[self.pos] == b'.')
        {
            self.pos += 1;
        }
        if self.pos < self.input.len()
            && (self.input[self.pos] == b'e' || self.input[self.pos] == b'E')
        {
            let exp_start = self.pos;
            self.pos += 1;
            if self.pos < self.input.len()
                && (self.input[self.pos] == b'+' || self.input[self.pos] == b'-')
            {
                self.pos += 1;
            }
            let digits = self.pos;
            while self.pos < self.input.len() && self.input[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
            if self.pos == digits {
                // `1E` with no exponent digits: strtod stops before the `E`.
                self.pos = exp_start;
            }
        }

        let s = std::str::from_utf8(&self.input[start..self.pos]).unwrap();
        s.parse::<f64>().map_err(|_| CalcError::BadLiteral)
    }

    /// C99 `strtod` on a `0x` prefix: hex mantissa, optional hex fraction,
    /// optional binary (`p`) exponent.
    fn read_hex_strtod(&mut self, start: usize) -> f64 {
        self.pos = start + 2;
        let mut value = 0.0f64;
        while self.pos < self.input.len() && self.input[self.pos].is_ascii_hexdigit() {
            value = value * 16.0 + hex_digit(self.input[self.pos]) as f64;
            self.pos += 1;
        }
        if self.pos < self.input.len() && self.input[self.pos] == b'.' {
            self.pos += 1;
            let mut scale = 1.0 / 16.0;
            while self.pos < self.input.len() && self.input[self.pos].is_ascii_hexdigit() {
                value += hex_digit(self.input[self.pos]) as f64 * scale;
                scale /= 16.0;
                self.pos += 1;
            }
        }
        if self.pos < self.input.len() && matches!(self.input[self.pos], b'p' | b'P') {
            let exp_start = self.pos;
            self.pos += 1;
            let mut negative = false;
            if self.pos < self.input.len()
                && (self.input[self.pos] == b'+' || self.input[self.pos] == b'-')
            {
                negative = self.input[self.pos] == b'-';
                self.pos += 1;
            }
            let digits = self.pos;
            let mut exp: i32 = 0;
            while self.pos < self.input.len() && self.input[self.pos].is_ascii_digit() {
                exp = exp
                    .saturating_mul(10)
                    .saturating_add((self.input[self.pos] - b'0') as i32);
                self.pos += 1;
            }
            if self.pos == digits {
                self.pos = exp_start; // no exponent digits: `p` is not consumed
            } else {
                value *= 2.0f64.powi(if negative { -exp } else { exp });
            }
        }
        value
    }
}

fn hex_digit(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        _ => b - b'A' + 10,
    }
}

/// Lex `input` with the `ELEMENT` table of the C compiler `kind` names.
///
/// A symbol outside that table is `CalcError::Syntax` — C's `CALC_ERR_SYNTAX`,
/// raised at compile time, because C's `get_element` simply cannot produce it.
pub fn tokenize(input: &str, kind: ExprKind) -> Result<Vec<Token>, CalcError> {
    let table = table_for(&kind);
    let mut tokenizer = Tokenizer::new(input, table);
    let mut tokens = Vec::new();

    loop {
        tokenizer.skip_whitespace();
        let Some(b) = tokenizer.peek() else { break };

        // LITERAL_OPERAND: `.` and `0`..`9` (and base's `0X`).
        if b.is_ascii_digit()
            || (b == b'.'
                && tokenizer
                    .input
                    .get(tokenizer.pos + 1)
                    .is_some_and(u8::is_ascii_digit))
        {
            let n = tokenizer.read_number()?;
            tokens.push(Token::Number(n));
            continue;
        }

        // LITERAL_STRING: sCalcPostfix.c:97-98 only.
        if (b == b'"' || b == b'\'') && table.string_literals {
            tokenizer.advance();
            let s = tokenizer.read_string_literal(b)?;
            tokens.push(Token::StringLiteral(s));
            continue;
        }

        if let Some(tok) = tokenizer.match_symbol() {
            tokens.push(tok);
            continue;
        }
        if let Some(tok) = tokenizer.match_var() {
            tokens.push(tok);
            continue;
        }

        return Err(CalcError::Syntax);
    }

    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(expr: &str) -> Result<Vec<Token>, CalcError> {
        tokenize(expr, ExprKind::Numeric)
    }
    fn scalc(expr: &str) -> Result<Vec<Token>, CalcError> {
        tokenize(expr, ExprKind::String)
    }
    fn acalc(expr: &str) -> Result<Vec<Token>, CalcError> {
        tokenize(expr, ExprKind::Array)
    }

    #[test]
    fn test_basic_tokens() {
        assert_eq!(
            base("A+B*3").unwrap(),
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
            base("SIN(A)").unwrap(),
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
            scalc("AA+BB").unwrap(),
            vec![Token::DoubleVar(0), Token::Plus, Token::DoubleVar(1)]
        );
        // base postfix.c has no double-letter operand: `AA` is `A` then `A`.
        assert_eq!(base("AA").unwrap(), vec![Token::Var(0), Token::Var(0)]);
    }

    #[test]
    fn test_constants() {
        assert_eq!(
            base("PI+D2R").unwrap(),
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
            base("sin(a)+Cos(b)").unwrap(),
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
            base("A:=5").unwrap(),
            vec![Token::Var(0), Token::Assign, Token::Number(5.0)]
        );
    }

    #[test]
    fn test_ternary() {
        assert_eq!(
            base("A?B:C").unwrap(),
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
        assert_eq!(base("0xFF").unwrap(), vec![Token::Number(255.0)]);
        assert_eq!(scalc("0xFF").unwrap(), vec![Token::Number(255.0)]);
    }

    #[test]
    fn test_float_literal() {
        assert_eq!(base("3.14e2").unwrap(), vec![Token::Number(314.0)]);
    }

    #[test]
    fn test_operand_range_per_table() {
        // base: A..U, and no double-letter operand at all.
        assert_eq!(base("U").unwrap(), vec![Token::Var(20)]);
        // sCalc/aCalc: A..P, AA..LL. `Q` is in no operand table and in no
        // symbol table, so the lexer itself stops — C `get_element` returns
        // FALSE and `sCalcPostfix` answers CALC_ERR_SYNTAX.
        assert_eq!(scalc("P").unwrap(), vec![Token::Var(15)]);
        assert_eq!(scalc("Q"), Err(CalcError::Syntax));
        assert_eq!(acalc("U"), Err(CalcError::Syntax));
        assert_eq!(scalc("LL").unwrap(), vec![Token::DoubleVar(11)]);
        // `MM` is not a double operand: C lexes `M` and then fails on the
        // second `M` in operator position. The port reaches the same
        // CALC_ERR_SYNTAX one stage later, in `postfix::compile`.
        assert_eq!(scalc("MM").unwrap(), vec![Token::Var(12), Token::Var(12)]);
    }

    #[test]
    fn test_symbols_outside_a_table() {
        // FMOD and `>>>` are base-only symbols. In sCalc/aCalc no entry spells
        // them, so the text lexes as other elements (`F`,`M`,`O`,`D` operands;
        // `>>` then `>`) and `postfix::compile` is where CALC_ERR_SYNTAX lands
        // — exactly as C, whose `get_element` does the same and then finds no
        // operand for `>`. The engine-level assertions live in
        // tests/calc_element_tables.rs.
        assert_eq!(base("FMOD(A,B)").unwrap()[0], Token::Func(FuncName::Fmod));
        assert_eq!(base("A>>>1").unwrap()[1], Token::ShrLogical);
        assert_eq!(scalc("A>>>1").unwrap()[1], Token::Shr);
        // INF/NAN are LITERAL_OPERANDs of base and sCalc; aCalc has neither, so
        // `INF` lexes there as the operands I, N, F (C does the same and then
        // fails at `N` in operator position).
        assert_eq!(base("INF").unwrap(), vec![Token::Number(f64::INFINITY)]);
        assert_eq!(scalc("INF").unwrap(), vec![Token::Number(f64::INFINITY)]);
        assert_eq!(
            acalc("INF").unwrap(),
            vec![Token::Var(8), Token::Var(13), Token::Var(5)]
        );
        // `DBL` is TO_DOUBLE in both synApps tables, but a different opcode in
        // each; base has no such symbol.
        assert_eq!(
            scalc("DBL(AA)").unwrap()[0],
            Token::Func(FuncName::Dbl),
            "sCalc DBL is the string->double op"
        );
        assert_eq!(
            acalc("DBL(AA)").unwrap()[0],
            Token::Func(FuncName::AToD),
            "aCalc DBL is the array->double op"
        );
    }
}
