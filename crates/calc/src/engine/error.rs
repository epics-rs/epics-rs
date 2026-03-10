use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum CalcError {
    TooMany,
    BadLiteral,
    ParenNotOpen,
    ParenOpen,
    Conditional,
    Incomplete,
    Underflow,
    Overflow,
    Syntax,
    NullArg,
    Internal,
    DivisionByZero,
    BadSeparator,
    BadAssignment,
    TypeMismatch,
    LengthMismatch,
    InvalidFormat,
    LoopLimitExceeded,
    EmptyArray,
    InvalidSubrange,
    BracketNotOpen,
    BraceNotOpen,
}

impl fmt::Display for CalcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CalcError::TooMany => write!(f, "Too many results returned"),
            CalcError::BadLiteral => write!(f, "Badly formed numeric literal"),
            CalcError::ParenNotOpen => write!(f, "Close parenthesis found without open"),
            CalcError::ParenOpen => write!(f, "Parenthesis still open at end of expression"),
            CalcError::Conditional => write!(f, "Unbalanced conditional ?: operators"),
            CalcError::Incomplete => write!(f, "Incomplete expression, operand missing"),
            CalcError::Underflow => write!(f, "Not enough operands provided"),
            CalcError::Overflow => write!(f, "Runtime stack would overflow"),
            CalcError::Syntax => write!(f, "Syntax error, unknown operator/operand"),
            CalcError::NullArg => write!(f, "NULL or empty input argument"),
            CalcError::Internal => write!(f, "Internal error"),
            CalcError::DivisionByZero => write!(f, "Division by zero"),
            CalcError::BadSeparator => write!(f, "Comma without enclosing parentheses"),
            CalcError::BadAssignment => write!(f, "Bad assignment target"),
            CalcError::TypeMismatch => write!(f, "Type mismatch: mixed numeric/string operation"),
            CalcError::LengthMismatch => write!(f, "Array length mismatch in binary operation"),
            CalcError::InvalidFormat => write!(f, "Invalid format string"),
            CalcError::LoopLimitExceeded => write!(f, "Loop iteration limit exceeded"),
            CalcError::EmptyArray => write!(f, "Operation on empty array"),
            CalcError::InvalidSubrange => write!(f, "Invalid subrange specification"),
            CalcError::BracketNotOpen => write!(f, "Close bracket found without open"),
            CalcError::BraceNotOpen => write!(f, "Close brace found without open"),
        }
    }
}

impl std::error::Error for CalcError {}
