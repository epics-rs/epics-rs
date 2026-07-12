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
    InvalidFormat,
    EmptyArray,
    InvalidSubrange,
    BracketNotOpen,
    BraceNotOpen,
    DomainError,
    NonFiniteResult,
    EmptyProgram,
    /// aCalc's polynomial fit failed — fewer than three points in the window, or a
    /// singular normal matrix (`calcUtil.c:271`, `:297`). C's `fitpoly` returns -1,
    /// which DERIV/NDERIV/FITPOLY/FITMPOLY/FITQ/FITMQ assign to `status`
    /// (`aCalcPerform.c:613`, `:985`, `:1008`, `:1029`, `:1221`, `:1270`).
    FitFailed,
    /// The expression did not end with EXACTLY one value on the value stack.
    ///
    /// C's `if (ps != top) { freeStack(...); return(-1); }` (`aCalcPerform.c:1607-1618`).
    /// `ps` starts one BELOW `top` (`:418-419`, `top = ps = &stack[1]; ps--;`), so
    /// `ps == top` means depth exactly 1 — an expression that leaked an operand and
    /// one that consumed too many are both a hard -1, and C writes neither
    /// `p_dresult` nor `p_aresult`.
    ///
    /// This is an INVARIANT, not a reachable divergence: a program that
    /// `aCalcPostfix` accepts always balances, and no expression has been found that
    /// trips it while the port returns a value. It exists so that a future opcode
    /// which forgets to pop cannot silently publish the wrong stack cell as VAL/AVAL.
    StackLeak,
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
            CalcError::InvalidFormat => write!(f, "Invalid format string"),
            CalcError::EmptyArray => write!(f, "Operation on empty array"),
            CalcError::InvalidSubrange => write!(f, "Invalid subrange specification"),
            CalcError::BracketNotOpen => write!(f, "Close bracket found without open"),
            CalcError::BraceNotOpen => write!(f, "Close brace found without open"),
            CalcError::DomainError => write!(f, "Operand outside the operator's domain"),
            CalcError::NonFiniteResult => write!(f, "Result is not a finite number"),
            CalcError::EmptyProgram => write!(f, "Empty postfix program"),
            CalcError::FitFailed => write!(f, "Polynomial fit failed"),
            CalcError::StackLeak => write!(f, "Too many results returned"),
        }
    }
}

impl std::error::Error for CalcError {}

impl CalcError {
    /// Return the epics-base `CALC_ERR_*` integer code for this error
    /// (`postfix.h:83-109`). Variants that have no epics-base equivalent
    /// (synApps string/array extensions) map to the nearest C code so that
    /// record-level `perror` reporting stays within the documented range.
    pub fn code(&self) -> i16 {
        match self {
            // CALC_ERR_TOOMANY        = 1. `StackLeak` is the RUNTIME form of the
            // same thing — an expression that ended with an operand left over — so
            // it reports the same code and the same string.
            CalcError::TooMany | CalcError::StackLeak => 1,
            // CALC_ERR_BAD_LITERAL    = 2
            CalcError::BadLiteral => 2,
            // CALC_ERR_BAD_ASSIGNMENT = 3
            CalcError::BadAssignment => 3,
            // CALC_ERR_BAD_SEPERATOR  = 4
            CalcError::BadSeparator => 4,
            // CALC_ERR_PAREN_NOT_OPEN = 5
            CalcError::ParenNotOpen | CalcError::BracketNotOpen | CalcError::BraceNotOpen => 5,
            // CALC_ERR_PAREN_OPEN     = 6
            CalcError::ParenOpen => 6,
            // CALC_ERR_CONDITIONAL    = 7
            CalcError::Conditional => 7,
            // CALC_ERR_INCOMPLETE     = 8
            CalcError::Incomplete => 8,
            // CALC_ERR_UNDERFLOW      = 9
            CalcError::Underflow => 9,
            // CALC_ERR_OVERFLOW       = 10
            CalcError::Overflow => 10,
            // CALC_ERR_SYNTAX         = 11
            CalcError::Syntax
            | CalcError::DivisionByZero
            | CalcError::TypeMismatch
            | CalcError::InvalidFormat
            | CalcError::EmptyArray
            | CalcError::InvalidSubrange
            | CalcError::DomainError
            | CalcError::FitFailed
            | CalcError::NonFiniteResult => 11,
            // CALC_ERR_NULL_ARG       = 12. `EmptyProgram` is an *evaluation*
            // failure — C's `perform()` returns a bare -1 with no error code
            // (`calcPerform.c:419-420`, `sCalcPerform.c:396`,
            // `aCalcPerform.c:312-314`) — so it has no C code of its own; 12 is
            // the nearest, and is what base `postfix()` calls the empty
            // expression that produces such a program.
            CalcError::NullArg | CalcError::EmptyProgram => 12,
            // CALC_ERR_INTERNAL       = 13
            CalcError::Internal => 13,
        }
    }
}

/// Return a message string for an epics-base `CALC_ERR_*` integer code,
/// equivalent to C `calcErrorStr` (`postfix.c:515-538`). Returns `None`
/// for codes outside `0..=13`.
pub fn calc_error_str(error: i16) -> Option<&'static str> {
    const ERR_STRS: [&str; 14] = [
        "No error",
        "Too many results returned",
        "Badly formed numeric literal",
        "Bad assignment target",
        "Comma without enclosing parentheses",
        "Close parenthesis found without open",
        "Parenthesis still open at end of expression",
        "Unbalanced conditional ?: operators",
        "Incomplete expression, operand missing",
        "Not enough operands provided",
        "Runtime stack overflow",
        "Syntax error, unknown operator/operand",
        "NULL or empty input argument to postfix()",
        "Internal error, unknown element type",
    ];
    if !(0..=13).contains(&error) {
        return None;
    }
    Some(ERR_STRS[error as usize])
}
