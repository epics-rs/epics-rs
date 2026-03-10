use crate::error::{CompileError, CompileResult, Span};

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    // Keywords
    Program,
    Ss,
    State,
    When,
    Entry,
    Exit,
    Option_,
    Assign,
    To,
    Monitor,
    Sync,
    EvFlag,
    If,
    Else,
    While,
    For,
    Break,
    Return,
    // Types
    Int,
    Short,
    Long,
    Float,
    Double,
    String_,
    Char,
    Unsigned,
    // Literals
    IntLit(i64),
    FloatLit(f64),
    StringLit(String),
    // Identifiers
    Ident(String),
    // Punctuation
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Semi,
    Comma,
    Dot,
    Arrow, // ->
    // Operators
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Eq,      // ==
    Ne,      // !=
    Lt,
    Le,      // <=
    Gt,
    Ge,      // >=
    And,     // &&
    Or,      // ||
    Not,     // !
    BitAnd,  // &
    BitOr,   // |
    BitXor,  // ^
    BitNot,  // ~
    Shl,     // <<
    Shr,     // >>
    Assign_,  // =
    PlusEq,  // +=
    MinusEq, // -=
    StarEq,  // *=
    SlashEq, // /=
    PlusPlus,   // ++
    MinusMinus, // --
    Question,   // ?
    Colon,      // :
    // Special
    Hash,        // #
    DoublePercent, // %%
    EmbeddedLine(String), // %% rest-of-line
    Eof,
}

#[derive(Debug, Clone)]
pub struct SpannedToken {
    pub token: Token,
    pub span: Span,
}

pub struct Lexer<'a> {
    input: &'a [u8],
    pos: usize,
    line: usize,
    col: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str) -> Self {
        Self {
            input: input.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
        }
    }

    fn span(&self) -> Span {
        Span {
            offset: self.pos,
            line: self.line,
            column: self.col,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<u8> {
        self.input.get(self.pos + 1).copied()
    }

    fn advance(&mut self) -> Option<u8> {
        let ch = self.input.get(self.pos).copied()?;
        self.pos += 1;
        if ch == b'\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(ch)
    }

    fn skip_whitespace_and_comments(&mut self) {
        loop {
            // Skip whitespace
            while self.peek().map_or(false, |c| c.is_ascii_whitespace()) {
                self.advance();
            }

            // Skip // comments
            if self.peek() == Some(b'/') && self.peek2() == Some(b'/') {
                while self.peek().map_or(false, |c| c != b'\n') {
                    self.advance();
                }
                continue;
            }

            // Skip /* */ comments
            if self.peek() == Some(b'/') && self.peek2() == Some(b'*') {
                self.advance();
                self.advance();
                let mut depth = 1;
                while depth > 0 {
                    match self.advance() {
                        Some(b'*') if self.peek() == Some(b'/') => {
                            self.advance();
                            depth -= 1;
                        }
                        Some(b'/') if self.peek() == Some(b'*') => {
                            self.advance();
                            depth += 1;
                        }
                        None => break,
                        _ => {}
                    }
                }
                continue;
            }

            break;
        }
    }

    fn read_string(&mut self) -> CompileResult<String> {
        let span = self.span();
        self.advance(); // consume opening "
        let mut s = String::new();
        loop {
            match self.advance() {
                Some(b'"') => return Ok(s),
                Some(b'\\') => match self.advance() {
                    Some(b'n') => s.push('\n'),
                    Some(b't') => s.push('\t'),
                    Some(b'\\') => s.push('\\'),
                    Some(b'"') => s.push('"'),
                    Some(b'0') => s.push('\0'),
                    Some(c) => {
                        s.push('\\');
                        s.push(c as char);
                    }
                    None => return Err(CompileError::syntax(span, "unterminated string")),
                },
                Some(c) => s.push(c as char),
                None => return Err(CompileError::syntax(span, "unterminated string")),
            }
        }
    }

    fn read_char_literal(&mut self) -> CompileResult<i64> {
        let span = self.span();
        self.advance(); // consume opening '
        let ch = match self.advance() {
            Some(b'\\') => match self.advance() {
                Some(b'n') => b'\n',
                Some(b't') => b'\t',
                Some(b'\\') => b'\\',
                Some(b'\'') => b'\'',
                Some(b'0') => b'\0',
                Some(b'r') => b'\r',
                Some(b'a') => 7, // bell
                Some(b'b') => 8, // backspace
                Some(c) => c,
                None => return Err(CompileError::syntax(span, "unterminated char literal")),
            },
            Some(c) => c,
            None => return Err(CompileError::syntax(span, "unterminated char literal")),
        };
        match self.advance() {
            Some(b'\'') => Ok(ch as i64),
            _ => Err(CompileError::syntax(span, "unterminated char literal")),
        }
    }

    fn read_number(&mut self) -> Token {
        let start = self.pos;
        let mut is_float = false;

        // Handle 0x prefix
        if self.peek() == Some(b'0')
            && self.input.get(self.pos + 1).map_or(false, |&c| c == b'x' || c == b'X')
        {
            self.advance();
            self.advance();
            while self.peek().map_or(false, |c| c.is_ascii_hexdigit()) {
                self.advance();
            }
            let s = std::str::from_utf8(&self.input[start..self.pos]).unwrap();
            return Token::IntLit(i64::from_str_radix(&s[2..], 16).unwrap_or(0));
        }

        while self.peek().map_or(false, |c| c.is_ascii_digit()) {
            self.advance();
        }

        if self.peek() == Some(b'.') && self.peek2().map_or(false, |c| c.is_ascii_digit()) {
            is_float = true;
            self.advance();
            while self.peek().map_or(false, |c| c.is_ascii_digit()) {
                self.advance();
            }
        }

        // Scientific notation
        if self.peek().map_or(false, |c| c == b'e' || c == b'E') {
            is_float = true;
            self.advance();
            if self.peek().map_or(false, |c| c == b'+' || c == b'-') {
                self.advance();
            }
            while self.peek().map_or(false, |c| c.is_ascii_digit()) {
                self.advance();
            }
        }

        // Suffix
        if self.peek().map_or(false, |c| c == b'f' || c == b'F') {
            is_float = true;
            self.advance();
        }

        let s = std::str::from_utf8(&self.input[start..self.pos]).unwrap();
        let s = s.trim_end_matches(|c: char| c == 'f' || c == 'F');

        if is_float {
            Token::FloatLit(s.parse().unwrap_or(0.0))
        } else {
            Token::IntLit(s.parse().unwrap_or(0))
        }
    }

    fn read_ident(&mut self) -> String {
        let start = self.pos;
        while self
            .peek()
            .map_or(false, |c| c.is_ascii_alphanumeric() || c == b'_')
        {
            self.advance();
        }
        std::str::from_utf8(&self.input[start..self.pos])
            .unwrap()
            .to_string()
    }

    pub fn tokenize(&mut self) -> CompileResult<Vec<SpannedToken>> {
        let mut tokens = Vec::new();

        loop {
            self.skip_whitespace_and_comments();

            let span = self.span();

            let Some(ch) = self.peek() else {
                tokens.push(SpannedToken {
                    token: Token::Eof,
                    span,
                });
                break;
            };

            let token = match ch {
                b'(' => { self.advance(); Token::LParen }
                b')' => { self.advance(); Token::RParen }
                b'{' => { self.advance(); Token::LBrace }
                b'}' => { self.advance(); Token::RBrace }
                b'[' => { self.advance(); Token::LBracket }
                b']' => { self.advance(); Token::RBracket }
                b';' => { self.advance(); Token::Semi }
                b',' => { self.advance(); Token::Comma }
                b'.' => { self.advance(); Token::Dot }
                b'~' => { self.advance(); Token::BitNot }
                b'?' => { self.advance(); Token::Question }
                b':' => { self.advance(); Token::Colon }
                b'"' => Token::StringLit(self.read_string()?),
                b'\'' => Token::IntLit(self.read_char_literal()?),
                b'#' => {
                    self.advance();
                    // Skip preprocessor lines
                    let start = self.pos;
                    while self.peek().map_or(false, |c| c != b'\n') {
                        self.advance();
                    }
                    let _line = std::str::from_utf8(&self.input[start..self.pos]).unwrap();
                    continue; // skip preprocessor directives
                }
                b'%' if self.peek2() == Some(b'%') => {
                    self.advance();
                    self.advance();
                    // Read rest of line as embedded code
                    let start = self.pos;
                    while self.peek().map_or(false, |c| c != b'\n') {
                        self.advance();
                    }
                    let code = std::str::from_utf8(&self.input[start..self.pos]).unwrap().to_string();
                    tokens.push(SpannedToken {
                        token: Token::EmbeddedLine(code),
                        span,
                    });
                    continue;
                }
                b'+' => {
                    self.advance();
                    match self.peek() {
                        Some(b'+') => { self.advance(); Token::PlusPlus }
                        Some(b'=') => { self.advance(); Token::PlusEq }
                        _ => Token::Plus,
                    }
                }
                b'-' => {
                    self.advance();
                    match self.peek() {
                        Some(b'-') => { self.advance(); Token::MinusMinus }
                        Some(b'=') => { self.advance(); Token::MinusEq }
                        Some(b'>') => { self.advance(); Token::Arrow }
                        _ => Token::Minus,
                    }
                }
                b'*' => {
                    self.advance();
                    if self.peek() == Some(b'=') { self.advance(); Token::StarEq }
                    else { Token::Star }
                }
                b'/' => {
                    self.advance();
                    if self.peek() == Some(b'=') { self.advance(); Token::SlashEq }
                    else { Token::Slash }
                }
                b'%' if self.peek2() == Some(b'{') => {
                    self.advance();
                    self.advance();
                    // Read until }%
                    let start = self.pos;
                    loop {
                        match self.peek() {
                            Some(b'}') if self.input.get(self.pos + 1) == Some(&b'%') => {
                                let code = std::str::from_utf8(&self.input[start..self.pos]).unwrap().to_string();
                                self.advance();
                                self.advance();
                                tokens.push(SpannedToken {
                                    token: Token::EmbeddedLine(code),
                                    span,
                                });
                                break;
                            }
                            Some(_) => { self.advance(); }
                            None => {
                                return Err(CompileError::syntax(span, "unterminated %{ }% block"));
                            }
                        }
                    }
                    continue;
                }
                b'%' => {
                    self.advance();
                    Token::Percent
                }
                b'=' => {
                    self.advance();
                    if self.peek() == Some(b'=') { self.advance(); Token::Eq }
                    else { Token::Assign_ }
                }
                b'!' => {
                    self.advance();
                    if self.peek() == Some(b'=') { self.advance(); Token::Ne }
                    else { Token::Not }
                }
                b'<' => {
                    self.advance();
                    match self.peek() {
                        Some(b'=') => { self.advance(); Token::Le }
                        Some(b'<') => { self.advance(); Token::Shl }
                        _ => Token::Lt,
                    }
                }
                b'>' => {
                    self.advance();
                    match self.peek() {
                        Some(b'=') => { self.advance(); Token::Ge }
                        Some(b'>') => { self.advance(); Token::Shr }
                        _ => Token::Gt,
                    }
                }
                b'&' => {
                    self.advance();
                    if self.peek() == Some(b'&') { self.advance(); Token::And }
                    else { Token::BitAnd }
                }
                b'|' => {
                    self.advance();
                    if self.peek() == Some(b'|') { self.advance(); Token::Or }
                    else { Token::BitOr }
                }
                b'^' => { self.advance(); Token::BitXor }
                c if c.is_ascii_digit() => self.read_number(),
                c if c.is_ascii_alphabetic() || c == b'_' => {
                    let ident = self.read_ident();
                    match ident.as_str() {
                        "program" => Token::Program,
                        "ss" => Token::Ss,
                        "state" => Token::State,
                        "when" => Token::When,
                        "entry" => Token::Entry,
                        "exit" => Token::Exit,
                        "option" => Token::Option_,
                        "assign" => Token::Assign,
                        "to" => Token::To,
                        "monitor" => Token::Monitor,
                        "sync" => Token::Sync,
                        "evflag" => Token::EvFlag,
                        "if" => Token::If,
                        "else" => Token::Else,
                        "while" => Token::While,
                        "for" => Token::For,
                        "break" => Token::Break,
                        "return" => Token::Return,
                        "int" => Token::Int,
                        "short" => Token::Short,
                        "long" => Token::Long,
                        "float" => Token::Float,
                        "double" => Token::Double,
                        "string" => Token::String_,
                        "char" => Token::Char,
                        "unsigned" => Token::Unsigned,
                        "TRUE" | "true" => Token::IntLit(1),
                        "FALSE" | "false" => Token::IntLit(0),
                        _ => Token::Ident(ident),
                    }
                }
                _ => {
                    return Err(CompileError::syntax(
                        span,
                        format!("unexpected character: '{}'", ch as char),
                    ));
                }
            };

            tokens.push(SpannedToken { token, span });
        }

        Ok(tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(input: &str) -> Vec<Token> {
        Lexer::new(input)
            .tokenize()
            .unwrap()
            .into_iter()
            .map(|st| st.token)
            .collect()
    }

    #[test]
    fn test_keywords() {
        let tokens = lex("program ss state when entry exit");
        assert_eq!(
            tokens,
            vec![
                Token::Program,
                Token::Ss,
                Token::State,
                Token::When,
                Token::Entry,
                Token::Exit,
                Token::Eof,
            ]
        );
    }

    #[test]
    fn test_operators() {
        let tokens = lex("+ - * / == != <= >= && || ++ -- += -=");
        assert_eq!(
            tokens,
            vec![
                Token::Plus, Token::Minus, Token::Star, Token::Slash,
                Token::Eq, Token::Ne, Token::Le, Token::Ge,
                Token::And, Token::Or, Token::PlusPlus, Token::MinusMinus,
                Token::PlusEq, Token::MinusEq, Token::Eof,
            ]
        );
    }

    #[test]
    fn test_numbers() {
        let tokens = lex("42 3.14 0xFF 1e5");
        assert_eq!(
            tokens,
            vec![
                Token::IntLit(42),
                Token::FloatLit(3.14),
                Token::IntLit(255),
                Token::FloatLit(1e5),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn test_string() {
        let tokens = lex(r#""hello\nworld""#);
        assert_eq!(
            tokens,
            vec![Token::StringLit("hello\nworld".to_string()), Token::Eof]
        );
    }

    #[test]
    fn test_comment_skip() {
        let tokens = lex("a /* comment */ b // line\nc");
        assert_eq!(
            tokens,
            vec![
                Token::Ident("a".into()),
                Token::Ident("b".into()),
                Token::Ident("c".into()),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn test_simple_program() {
        let tokens = lex(r#"
            program test
            option +s;
            double x;
            assign x to "PV:x";
            monitor x;
        "#);
        assert_eq!(tokens[0], Token::Program);
        assert_eq!(tokens[1], Token::Ident("test".into()));
        assert_eq!(tokens[2], Token::Option_);
    }

    #[test]
    fn test_preprocessor_skipped() {
        let tokens = lex("#include \"foo.h\"\nint x;");
        assert_eq!(
            tokens,
            vec![Token::Int, Token::Ident("x".into()), Token::Semi, Token::Eof]
        );
    }

    #[test]
    fn test_char_literal() {
        let tokens = lex("'A' '\\n' '\\0'");
        assert_eq!(
            tokens,
            vec![
                Token::IntLit(65),
                Token::IntLit(10),
                Token::IntLit(0),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn test_embedded_line() {
        let tokens = lex("%% use std::io;\nint x;");
        assert_eq!(tokens.len(), 5); // EmbeddedLine, Int, Ident, Semi, Eof
        assert!(matches!(&tokens[0], Token::EmbeddedLine(s) if s.contains("use std::io")));
    }

    #[test]
    fn test_embedded_block() {
        let tokens = lex("%{ some code }%\nint x;");
        assert_eq!(tokens.len(), 5); // EmbeddedLine, Int, Ident, Semi, Eof
        assert!(matches!(&tokens[0], Token::EmbeddedLine(s) if s.contains("some code")));
    }

    #[test]
    fn test_true_false() {
        let tokens = lex("TRUE FALSE true false");
        assert_eq!(
            tokens,
            vec![
                Token::IntLit(1), Token::IntLit(0),
                Token::IntLit(1), Token::IntLit(0),
                Token::Eof,
            ]
        );
    }
}
