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
    Log2,
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    Atan,
    Atan2,
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
    Var(u8),       // A=0..P=15
    DoubleVar(u8), // AA=0..LL=11
    Rndm,
    Nrndm,

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

    AndAnd, // &&
    OrOr,   // ||
    BitAnd, // &
    BitOr,  // |
    BitXor, // XOR
    Tilde,  // ~
    Shl,    // <<
    Shr,    // >>

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

struct Tokenizer<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Tokenizer<'a> {
    fn new(input: &'a str) -> Self {
        Tokenizer {
            input: input.as_bytes(),
            pos: 0,
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

    fn remaining_upper(&self) -> Vec<u8> {
        self.input[self.pos..]
            .iter()
            .map(|b| b.to_ascii_uppercase())
            .collect()
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

    fn try_keyword(&mut self) -> Option<Token> {
        let rem = self.remaining_upper();
        if rem.is_empty() {
            return None;
        }

        // Build the keyword table, including feature-gated entries
        let mut keywords: Vec<(&[u8], Token)> = vec![
            // Functions (longest first to avoid prefix issues)
            (b"NORMAL_RNDM", Token::Nrndm),
            (b"NRNDM", Token::Nrndm),
            (b"FINITE", Token::Func(FuncName::Finite)),
            (b"FLOOR", Token::Func(FuncName::Floor)),
            (b"ISNAN", Token::Func(FuncName::IsNan)),
            (b"ISINF", Token::Func(FuncName::IsInf)),
            (b"ATAN2", Token::Func(FuncName::Atan2)),
            (b"SQRT", Token::Func(FuncName::Sqrt)),
            (b"ACOS", Token::Func(FuncName::Acos)),
            (b"ASIN", Token::Func(FuncName::Asin)),
            (b"ATAN", Token::Func(FuncName::Atan)),
            (b"COSH", Token::Func(FuncName::Cosh)),
            (b"SINH", Token::Func(FuncName::Sinh)),
            (b"TANH", Token::Func(FuncName::Tanh)),
            (b"CEIL", Token::Func(FuncName::Ceil)),
            (b"NINT", Token::Func(FuncName::Nint)),
            (b"LOGE", Token::Func(FuncName::LogE)),
            (b"LOG2", Token::Func(FuncName::Log2)),
            (b"RNDM", Token::Rndm),
            (b"ABS", Token::Func(FuncName::Abs)),
            (b"SQR", Token::Func(FuncName::Sqr)),
            (b"EXP", Token::Func(FuncName::Exp)),
            (b"LOG", Token::Func(FuncName::Log10)),
            (b"SIN", Token::Func(FuncName::Sin)),
            (b"COS", Token::Func(FuncName::Cos)),
            (b"TAN", Token::Func(FuncName::Tan)),
            (b"MAX", Token::Func(FuncName::Max)),
            (b"MIN", Token::Func(FuncName::Min)),
            (b"NOT", Token::Func(FuncName::Not)),
            (b"INT", Token::Func(FuncName::Int)),
            (b"LN", Token::Func(FuncName::Ln)),
            // Constants
            (b"D2R", Token::Const(ConstName::D2R)),
            (b"R2D", Token::Const(ConstName::R2D)),
            (b"PI", Token::Const(ConstName::Pi)),
            // Keyword operators
            (b"AND", Token::AndKeyword),
            (b"XOR", Token::BitXor),
            (b"OR", Token::OrKeyword),
            // Special literals
            (b"INF", Token::Number(f64::INFINITY)),
            (b"NAN", Token::Number(f64::NAN)),
        ];

        // String function keywords
        {
            keywords.extend_from_slice(&[
                (b"BIN_READ" as &[u8], Token::Func(FuncName::BinRead)),
                (b"BIN_WRITE", Token::Func(FuncName::BinWrite)),
                (b"ADD_XOR8", Token::Func(FuncName::AddXor8)),
                (b"AMODBUS", Token::Func(FuncName::AModBus)),
                (b"MODBUS", Token::Func(FuncName::ModBus)),
                (b"PRINTF", Token::Func(FuncName::Printf)),
                (b"SSCANF", Token::Func(FuncName::Sscanf)),
                (b"TR_ESC", Token::Func(FuncName::TrEsc)),
                (b"CRC16", Token::Func(FuncName::Crc16)),
                (b"UNTIL", Token::UntilKeyword),
                (b"BYTE", Token::Func(FuncName::Byte)),
                (b"XOR8", Token::Func(FuncName::Xor8)),
                (b"DBL", Token::Func(FuncName::Dbl)),
                (b"ESC", Token::Func(FuncName::Esc)),
                (b"LEN", Token::Func(FuncName::Len)),
                (b"STR", Token::Func(FuncName::Str)),
                (b"LRC", Token::Func(FuncName::Lrc)),
            ]);
        }

        // Array function keywords
        {
            keywords.extend_from_slice(&[
                (b"FITMPOLY" as &[u8], Token::Func(FuncName::FitMPoly)),
                (b"FITPOLY", Token::Func(FuncName::FitPoly)),
                (b"NDERIV", Token::Func(FuncName::NDeriv)),
                (b"NSMOO", Token::Func(FuncName::NSmoo)),
                (b"ARNDM", Token::Func(FuncName::ARndm)),
                (b"DERIV", Token::Func(FuncName::Deriv)),
                (b"FITMQ", Token::Func(FuncName::FitMQ)),
                (b"FITQ", Token::Func(FuncName::FitQ)),
                (b"FWHM", Token::Func(FuncName::FwhmFunc)),
                (b"IXMAX", Token::Func(FuncName::IxMax)),
                (b"IXMIN", Token::Func(FuncName::IxMin)),
                (b"IXNZ", Token::Func(FuncName::IxNz)),
                (b"ATOD", Token::Func(FuncName::AToD)),
                (b"SMOO", Token::Func(FuncName::Smoo)),
                (b"AMAX", Token::Func(FuncName::AMax)),
                (b"AMIN", Token::Func(FuncName::AMin)),
                (b"AVG", Token::Func(FuncName::Avg)),
                (b"STD", Token::Func(FuncName::Std)),
                (b"SUM", Token::Func(FuncName::Sum)),
                (b"CUM", Token::Func(FuncName::Cum)),
                (b"CAT", Token::Func(FuncName::Cat)),
                (b"ARR", Token::Func(FuncName::Arr)),
                (b"IXZ", Token::Func(FuncName::IxZ)),
                (b"IX", Token::Func(FuncName::Ix)),
            ]);
        }

        // Sort keywords longest first for correct matching
        keywords.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

        // Double-letter variables (AA..LL)
        if rem.len() >= 2 {
            let first = rem[0];
            let second = rem[1];
            if first == second && first >= b'A' && first <= b'L' {
                // Make sure it's not a longer keyword
                let is_keyword = keywords.iter().any(|(kw, _)| {
                    kw.len() > 2 && rem.len() >= kw.len() && rem[..kw.len()] == **kw
                });
                if !is_keyword {
                    self.pos += 2;
                    return Some(Token::DoubleVar((first - b'A') as u8));
                }
            }
        }

        for (kw, tok) in &keywords {
            if rem.len() >= kw.len() && rem[..kw.len()] == **kw {
                let is_literal = matches!(tok, Token::Number(_));
                if !is_literal
                    && kw.len() < rem.len()
                    && (rem[kw.len()].is_ascii_alphanumeric() || rem[kw.len()] == b'_')
                {
                    continue;
                }
                self.pos += kw.len();
                return Some(tok.clone());
            }
        }

        // Single-letter variables (A..P)
        if rem[0] >= b'A' && rem[0] <= b'P' {
            self.pos += 1;
            return Some(Token::Var((rem[0] - b'A') as u8));
        }

        None
    }

    fn read_number(&mut self) -> Result<f64, CalcError> {
        let start = self.pos;
        while self.pos < self.input.len()
            && (self.input[self.pos].is_ascii_digit() || self.input[self.pos] == b'.')
        {
            self.pos += 1;
        }
        if self.pos < self.input.len()
            && (self.input[self.pos] == b'e' || self.input[self.pos] == b'E')
        {
            self.pos += 1;
            if self.pos < self.input.len()
                && (self.input[self.pos] == b'+' || self.input[self.pos] == b'-')
            {
                self.pos += 1;
            }
            while self.pos < self.input.len() && self.input[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
        }
        if self.pos == start + 1
            && self.input[start] == b'0'
            && self.pos < self.input.len()
            && (self.input[self.pos] == b'x' || self.input[self.pos] == b'X')
        {
            self.pos += 1;
            while self.pos < self.input.len() && self.input[self.pos].is_ascii_hexdigit() {
                self.pos += 1;
            }
            let s = std::str::from_utf8(&self.input[start + 2..self.pos]).unwrap();
            return u64::from_str_radix(s, 16)
                .map(|v| v as f64)
                .map_err(|_| CalcError::BadLiteral);
        }

        let s = std::str::from_utf8(&self.input[start..self.pos]).unwrap();
        s.parse::<f64>().map_err(|_| CalcError::BadLiteral)
    }
}

pub fn tokenize(input: &str) -> Result<Vec<Token>, CalcError> {
    let mut tokenizer = Tokenizer::new(input);
    let mut tokens = Vec::new();

    loop {
        tokenizer.skip_whitespace();
        if tokenizer.peek().is_none() {
            break;
        }

        let b = tokenizer.peek().unwrap();

        // Numbers
        if b.is_ascii_digit()
            || (b == b'.'
                && tokenizer
                    .input
                    .get(tokenizer.pos + 1)
                    .map_or(false, |c| c.is_ascii_digit()))
        {
            let n = tokenizer.read_number()?;
            tokens.push(Token::Number(n));
            continue;
        }

        // String literals
        if b == b'"' || b == b'\'' {
            tokenizer.advance();
            let s = tokenizer.read_string_literal(b)?;
            tokens.push(Token::StringLiteral(s));
            continue;
        }

        // Try keywords/variables (alphabetic start)
        if b.is_ascii_alphabetic() {
            if let Some(tok) = tokenizer.try_keyword() {
                tokens.push(tok);
                continue;
            }
            return Err(CalcError::Syntax);
        }

        // Operators and punctuation
        tokenizer.advance();
        match b {
            b'+' => tokens.push(Token::Plus),
            b'-' => tokens.push(Token::Minus),
            b'/' => tokens.push(Token::Slash),
            b'%' => tokens.push(Token::Percent),
            b'^' => tokens.push(Token::Caret),
            b'~' => tokens.push(Token::Tilde),
            b'(' => tokens.push(Token::LParen),
            b')' => tokens.push(Token::RParen),
            b',' => tokens.push(Token::Comma),
            b';' => tokens.push(Token::Semicolon),
            b'?' => tokens.push(Token::Question),
            b'#' => tokens.push(Token::Ne),
            b'[' => tokens.push(Token::LBracket),
            b']' => tokens.push(Token::RBracket),
            b'{' => tokens.push(Token::LBrace),
            b'}' => tokens.push(Token::RBrace),
            b'*' => {
                if tokenizer.peek() == Some(b'*') {
                    tokenizer.advance();
                    tokens.push(Token::DoubleStar);
                } else {
                    tokens.push(Token::Star);
                }
            }
            b'!' => {
                if tokenizer.peek() == Some(b'=') {
                    tokenizer.advance();
                    tokens.push(Token::Ne);
                } else {
                    tokens.push(Token::Bang);
                }
            }
            b'=' => {
                if tokenizer.peek() == Some(b'=') {
                    tokenizer.advance();
                }
                tokens.push(Token::Eq);
            }
            b'<' => {
                if tokenizer.peek() == Some(b'=') {
                    tokenizer.advance();
                    tokens.push(Token::Le);
                } else if tokenizer.peek() == Some(b'<') {
                    tokenizer.advance();
                    tokens.push(Token::Shl);
                } else if tokenizer.peek() == Some(b'?') {
                    tokenizer.advance();
                    tokens.push(Token::MinOp);
                } else {
                    tokens.push(Token::Lt);
                }
            }
            b'>' => {
                if tokenizer.peek() == Some(b'=') {
                    tokenizer.advance();
                    tokens.push(Token::Ge);
                } else if tokenizer.peek() == Some(b'>') {
                    tokenizer.advance();
                    tokens.push(Token::Shr);
                } else if tokenizer.peek() == Some(b'?') {
                    tokenizer.advance();
                    tokens.push(Token::MaxOp);
                } else {
                    tokens.push(Token::Gt);
                }
            }
            b'&' => {
                if tokenizer.peek() == Some(b'&') {
                    tokenizer.advance();
                    tokens.push(Token::AndAnd);
                } else {
                    tokens.push(Token::BitAnd);
                }
            }
            b'|' => {
                if tokenizer.peek() == Some(b'-') {
                    tokenizer.advance();
                    tokens.push(Token::PipeMinus);
                    continue;
                }
                if tokenizer.peek() == Some(b'|') {
                    tokenizer.advance();
                    tokens.push(Token::OrOr);
                } else {
                    tokens.push(Token::BitOr);
                }
            }
            b':' => {
                if tokenizer.peek() == Some(b'=') {
                    tokenizer.advance();
                    tokens.push(Token::Assign);
                } else {
                    tokens.push(Token::Colon);
                }
            }
            _ => return Err(CalcError::Syntax),
        }
    }

    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_tokens() {
        let tokens = tokenize("A+B*3").unwrap();
        assert_eq!(
            tokens,
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
        let tokens = tokenize("SIN(A)").unwrap();
        assert_eq!(
            tokens,
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
        let tokens = tokenize("AA+BB").unwrap();
        assert_eq!(
            tokens,
            vec![Token::DoubleVar(0), Token::Plus, Token::DoubleVar(1),]
        );
    }

    #[test]
    fn test_constants() {
        let tokens = tokenize("PI+D2R").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Const(ConstName::Pi),
                Token::Plus,
                Token::Const(ConstName::D2R),
            ]
        );
    }

    #[test]
    fn test_case_insensitive() {
        let tokens = tokenize("sin(a)+Cos(b)").unwrap();
        assert_eq!(
            tokens,
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
        let tokens = tokenize("A:=5").unwrap();
        assert_eq!(
            tokens,
            vec![Token::Var(0), Token::Assign, Token::Number(5.0),]
        );
    }

    #[test]
    fn test_ternary() {
        let tokens = tokenize("A?B:C").unwrap();
        assert_eq!(
            tokens,
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
        let tokens = tokenize("0xFF").unwrap();
        assert_eq!(tokens, vec![Token::Number(255.0)]);
    }

    #[test]
    fn test_float_literal() {
        let tokens = tokenize("3.14e2").unwrap();
        assert_eq!(tokens, vec![Token::Number(314.0)]);
    }
}
