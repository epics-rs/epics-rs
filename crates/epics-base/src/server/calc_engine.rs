use crate::error::{CaError, CaResult};

/// Token types for the calc expression parser.
#[derive(Debug, Clone)]
enum Token {
    Number(f64),
    Variable(usize), // 0=A, 1=B, ..., 11=L
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Power,    // **
    Lt,
    Gt,
    Le,
    Ge,
    Eq,       // = (comparison in EPICS calc)
    Ne,       // !=
    And,      // &&
    Or,       // ||
    Not,      // !
    Question, // ?
    Colon,    // : and ,
    LParen,
    RParen,
    Func(String),
}

/// Tokenize a calc expression.
fn tokenize(expr: &str) -> CaResult<Vec<Token>> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = expr.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            ' ' | '\t' | '\n' | '\r' => { i += 1; }
            '0'..='9' | '.' if matches!(chars.get(i), Some('0'..='9') | Some('.')) => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.' || chars[i] == 'e' || chars[i] == 'E'
                    || ((chars[i] == '+' || chars[i] == '-') && i > 0 && (chars[i-1] == 'e' || chars[i-1] == 'E'))) {
                    i += 1;
                }
                let num_str: String = chars[start..i].iter().collect();
                let val = num_str.parse::<f64>()
                    .map_err(|_| CaError::CalcError(format!("invalid number: {num_str}")))?;
                tokens.push(Token::Number(val));
            }
            'A'..='L' if i + 1 >= chars.len() || !chars[i + 1].is_ascii_alphanumeric() => {
                tokens.push(Token::Variable((chars[i] as u8 - b'A') as usize));
                i += 1;
            }
            'a'..='l' if i + 1 >= chars.len() || !chars[i + 1].is_ascii_alphanumeric() => {
                tokens.push(Token::Variable((chars[i] as u8 - b'a') as usize));
                i += 1;
            }
            '+' => { tokens.push(Token::Plus); i += 1; }
            '-' => { tokens.push(Token::Minus); i += 1; }
            '/' => { tokens.push(Token::Slash); i += 1; }
            '%' => { tokens.push(Token::Percent); i += 1; }
            '(' => { tokens.push(Token::LParen); i += 1; }
            ')' => { tokens.push(Token::RParen); i += 1; }
            '?' => { tokens.push(Token::Question); i += 1; }
            ':' | ',' => { tokens.push(Token::Colon); i += 1; }
            '*' => {
                if i + 1 < chars.len() && chars[i + 1] == '*' {
                    tokens.push(Token::Power);
                    i += 2;
                } else {
                    tokens.push(Token::Star);
                    i += 1;
                }
            }
            '<' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    tokens.push(Token::Le);
                    i += 2;
                } else {
                    tokens.push(Token::Lt);
                    i += 1;
                }
            }
            '>' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    tokens.push(Token::Ge);
                    i += 2;
                } else {
                    tokens.push(Token::Gt);
                    i += 1;
                }
            }
            '!' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    tokens.push(Token::Ne);
                    i += 2;
                } else {
                    tokens.push(Token::Not);
                    i += 1;
                }
            }
            '=' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    tokens.push(Token::Eq);
                    i += 2;
                } else {
                    tokens.push(Token::Eq);
                    i += 1;
                }
            }
            '&' => {
                if i + 1 < chars.len() && chars[i + 1] == '&' {
                    tokens.push(Token::And);
                    i += 2;
                } else {
                    i += 1; // skip single &
                }
            }
            '|' => {
                if i + 1 < chars.len() && chars[i + 1] == '|' {
                    tokens.push(Token::Or);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            c if c.is_ascii_alphabetic() => {
                let start = i;
                while i < chars.len() && chars[i].is_ascii_alphanumeric() {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                tokens.push(Token::Func(name.to_uppercase()));
            }
            _ => {
                return Err(CaError::CalcError(format!("unexpected character: '{}'", chars[i])));
            }
        }
    }

    Ok(tokens)
}

/// Recursive descent parser for calc expressions.
struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) -> Option<Token> {
        if self.pos < self.tokens.len() {
            let t = self.tokens[self.pos].clone();
            self.pos += 1;
            Some(t)
        } else {
            None
        }
    }

    /// ternary: or_expr ('?' ternary ':' ternary)?
    fn parse_ternary(&mut self, vars: &[f64; 12]) -> CaResult<f64> {
        let cond = self.parse_or(vars)?;
        if matches!(self.peek(), Some(Token::Question)) {
            self.advance();
            let then_val = self.parse_ternary(vars)?;
            match self.advance() {
                Some(Token::Colon) => {}
                _ => return Err(CaError::CalcError("expected ':' in ternary".into())),
            }
            let else_val = self.parse_ternary(vars)?;
            Ok(if cond != 0.0 { then_val } else { else_val })
        } else {
            Ok(cond)
        }
    }

    fn parse_or(&mut self, vars: &[f64; 12]) -> CaResult<f64> {
        let mut left = self.parse_and(vars)?;
        while matches!(self.peek(), Some(Token::Or)) {
            self.advance();
            let right = self.parse_and(vars)?;
            left = if left != 0.0 || right != 0.0 { 1.0 } else { 0.0 };
        }
        Ok(left)
    }

    fn parse_and(&mut self, vars: &[f64; 12]) -> CaResult<f64> {
        let mut left = self.parse_comparison(vars)?;
        while matches!(self.peek(), Some(Token::And)) {
            self.advance();
            let right = self.parse_comparison(vars)?;
            left = if left != 0.0 && right != 0.0 { 1.0 } else { 0.0 };
        }
        Ok(left)
    }

    fn parse_comparison(&mut self, vars: &[f64; 12]) -> CaResult<f64> {
        let left = self.parse_additive(vars)?;
        match self.peek() {
            Some(Token::Lt) => { self.advance(); let r = self.parse_additive(vars)?; Ok(if left < r { 1.0 } else { 0.0 }) }
            Some(Token::Gt) => { self.advance(); let r = self.parse_additive(vars)?; Ok(if left > r { 1.0 } else { 0.0 }) }
            Some(Token::Le) => { self.advance(); let r = self.parse_additive(vars)?; Ok(if left <= r { 1.0 } else { 0.0 }) }
            Some(Token::Ge) => { self.advance(); let r = self.parse_additive(vars)?; Ok(if left >= r { 1.0 } else { 0.0 }) }
            Some(Token::Eq) => { self.advance(); let r = self.parse_additive(vars)?; Ok(if (left - r).abs() < f64::EPSILON { 1.0 } else { 0.0 }) }
            Some(Token::Ne) => { self.advance(); let r = self.parse_additive(vars)?; Ok(if (left - r).abs() >= f64::EPSILON { 1.0 } else { 0.0 }) }
            _ => Ok(left),
        }
    }

    fn parse_additive(&mut self, vars: &[f64; 12]) -> CaResult<f64> {
        let mut left = self.parse_multiplicative(vars)?;
        loop {
            match self.peek() {
                Some(Token::Plus) => { self.advance(); left += self.parse_multiplicative(vars)?; }
                Some(Token::Minus) => { self.advance(); left -= self.parse_multiplicative(vars)?; }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self, vars: &[f64; 12]) -> CaResult<f64> {
        let mut left = self.parse_power(vars)?;
        loop {
            match self.peek() {
                Some(Token::Star) => { self.advance(); left *= self.parse_power(vars)?; }
                Some(Token::Slash) => { self.advance(); left /= self.parse_power(vars)?; }
                Some(Token::Percent) => {
                    self.advance();
                    let r = self.parse_power(vars)?;
                    left = left % r;
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_power(&mut self, vars: &[f64; 12]) -> CaResult<f64> {
        let base = self.parse_unary(vars)?;
        if matches!(self.peek(), Some(Token::Power)) {
            self.advance();
            let exp = self.parse_unary(vars)?;
            Ok(base.powf(exp))
        } else {
            Ok(base)
        }
    }

    fn parse_unary(&mut self, vars: &[f64; 12]) -> CaResult<f64> {
        match self.peek() {
            Some(Token::Minus) => {
                self.advance();
                Ok(-self.parse_unary(vars)?)
            }
            Some(Token::Not) => {
                self.advance();
                let v = self.parse_unary(vars)?;
                Ok(if v == 0.0 { 1.0 } else { 0.0 })
            }
            Some(Token::Plus) => {
                self.advance();
                self.parse_unary(vars)
            }
            _ => self.parse_primary(vars),
        }
    }

    fn parse_primary(&mut self, vars: &[f64; 12]) -> CaResult<f64> {
        match self.advance() {
            Some(Token::Number(v)) => Ok(v),
            Some(Token::Variable(idx)) => Ok(vars[idx]),
            Some(Token::LParen) => {
                let v = self.parse_ternary(vars)?;
                match self.advance() {
                    Some(Token::RParen) => Ok(v),
                    _ => Err(CaError::CalcError("expected ')'".into())),
                }
            }
            Some(Token::Func(name)) => self.parse_func_call(&name, vars),
            other => Err(CaError::CalcError(format!("unexpected token: {other:?}"))),
        }
    }

    fn parse_func_call(&mut self, name: &str, vars: &[f64; 12]) -> CaResult<f64> {
        // Expect '('
        match self.advance() {
            Some(Token::LParen) => {}
            _ => return Err(CaError::CalcError(format!("expected '(' after function {name}"))),
        }

        // Parse arguments
        let mut args = Vec::new();
        if !matches!(self.peek(), Some(Token::RParen)) {
            args.push(self.parse_ternary(vars)?);
            while matches!(self.peek(), Some(Token::Colon)) {
                // EPICS calc uses , but some use :, handle both
                self.advance();
                args.push(self.parse_ternary(vars)?);
            }
        }

        match self.advance() {
            Some(Token::RParen) => {}
            _ => return Err(CaError::CalcError(format!("expected ')' after function args"))),
        }

        match name {
            "ABS" => {
                let a = args.first().copied().unwrap_or(0.0);
                Ok(a.abs())
            }
            "SQRT" => {
                let a = args.first().copied().unwrap_or(0.0);
                Ok(a.sqrt())
            }
            "SQR" | "SQ" => {
                let a = args.first().copied().unwrap_or(0.0);
                Ok(a * a)
            }
            "EXP" => {
                let a = args.first().copied().unwrap_or(0.0);
                Ok(a.exp())
            }
            "LOG" | "LOG10" => {
                let a = args.first().copied().unwrap_or(0.0);
                Ok(a.log10())
            }
            "LN" | "LOGE" => {
                let a = args.first().copied().unwrap_or(0.0);
                Ok(a.ln())
            }
            "MIN" => {
                if args.len() >= 2 {
                    Ok(args[0].min(args[1]))
                } else {
                    Ok(args.first().copied().unwrap_or(0.0))
                }
            }
            "MAX" => {
                if args.len() >= 2 {
                    Ok(args[0].max(args[1]))
                } else {
                    Ok(args.first().copied().unwrap_or(0.0))
                }
            }
            "SIN" => Ok(args.first().copied().unwrap_or(0.0).sin()),
            "COS" => Ok(args.first().copied().unwrap_or(0.0).cos()),
            "TAN" => Ok(args.first().copied().unwrap_or(0.0).tan()),
            "ASIN" => Ok(args.first().copied().unwrap_or(0.0).asin()),
            "ACOS" => Ok(args.first().copied().unwrap_or(0.0).acos()),
            "ATAN" => Ok(args.first().copied().unwrap_or(0.0).atan()),
            "CEIL" => Ok(args.first().copied().unwrap_or(0.0).ceil()),
            "FLOOR" => Ok(args.first().copied().unwrap_or(0.0).floor()),
            _ => Err(CaError::CalcError(format!("unknown function: {name}"))),
        }
    }
}

/// Evaluate a calc expression with variables A-L.
pub fn evaluate(expr: &str, vars: &[f64; 12]) -> CaResult<f64> {
    let tokens = tokenize(expr)?;
    if tokens.is_empty() {
        return Ok(0.0);
    }
    let mut parser = Parser::new(tokens);
    let result = parser.parse_ternary(vars)?;

    if result.is_nan() || result.is_infinite() {
        return Err(CaError::CalcError("result is NaN or Inf".into()));
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(expr: &str) -> f64 {
        evaluate(expr, &[0.0; 12]).unwrap()
    }

    fn eval_vars(expr: &str, vars: &[f64; 12]) -> f64 {
        evaluate(expr, vars).unwrap()
    }

    #[test]
    fn test_basic_arithmetic() {
        assert!((eval("2+3") - 5.0).abs() < 1e-10);
        assert!((eval("10-3") - 7.0).abs() < 1e-10);
        assert!((eval("4*5") - 20.0).abs() < 1e-10);
        assert!((eval("15/3") - 5.0).abs() < 1e-10);
        assert!((eval("7%3") - 1.0).abs() < 1e-10);
        assert!((eval("2**3") - 8.0).abs() < 1e-10);
    }

    #[test]
    fn test_precedence() {
        assert!((eval("2+3*4") - 14.0).abs() < 1e-10);
        assert!((eval("(2+3)*4") - 20.0).abs() < 1e-10);
    }

    #[test]
    fn test_comparison() {
        assert!((eval("3>2") - 1.0).abs() < 1e-10);
        assert!((eval("2>3") - 0.0).abs() < 1e-10);
        assert!((eval("3>=3") - 1.0).abs() < 1e-10);
        assert!((eval("2=2") - 1.0).abs() < 1e-10);
        assert!((eval("2!=3") - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_logical() {
        assert!((eval("1&&1") - 1.0).abs() < 1e-10);
        assert!((eval("1&&0") - 0.0).abs() < 1e-10);
        assert!((eval("0||1") - 1.0).abs() < 1e-10);
        assert!((eval("!0") - 1.0).abs() < 1e-10);
        assert!((eval("!5") - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_ternary() {
        assert!((eval("1?10:20") - 10.0).abs() < 1e-10);
        assert!((eval("0?10:20") - 20.0).abs() < 1e-10);
    }

    #[test]
    fn test_variables() {
        let mut vars = [0.0; 12];
        vars[0] = 10.0; // A
        vars[1] = 3.0;  // B
        assert!((eval_vars("A+B", &vars) - 13.0).abs() < 1e-10);
        assert!((eval_vars("A*B+1", &vars) - 31.0).abs() < 1e-10);
    }

    #[test]
    fn test_functions() {
        assert!((eval("ABS(-5)") - 5.0).abs() < 1e-10);
        assert!((eval("SQRT(9)") - 3.0).abs() < 1e-10);
        assert!((eval("SQR(4)") - 16.0).abs() < 1e-10);
        assert!((eval("MIN(3:7)") - 3.0).abs() < 1e-10);
        assert!((eval("MAX(3:7)") - 7.0).abs() < 1e-10);
        // Comma separator (standard EPICS syntax)
        assert!((eval("MIN(3,7)") - 3.0).abs() < 1e-10);
        assert!((eval("MAX(3,7)") - 7.0).abs() < 1e-10);
        assert!((eval("MAX(0,B-A)") - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_unary_minus() {
        assert!((eval("-5") - (-5.0)).abs() < 1e-10);
        assert!((eval("-(3+2)") - (-5.0)).abs() < 1e-10);
    }

    #[test]
    fn test_nan_error() {
        assert!(evaluate("0/0", &[0.0; 12]).is_err());
    }
}
