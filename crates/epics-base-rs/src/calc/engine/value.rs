use super::error::CalcError;

#[derive(Debug, Clone, PartialEq)]
pub enum StackValue {
    Double(f64),
    Str(String),
}

impl StackValue {
    pub fn is_double(&self) -> bool {
        matches!(self, StackValue::Double(_))
    }

    pub fn is_string(&self) -> bool {
        matches!(self, StackValue::Str(_))
    }

    /// C `toDouble` (sCalcPerform.c:80-83): a numeric operand position COERCES a
    /// string, it never rejects one —
    ///
    /// ```c
    /// #define toDouble(ps)  {if (isString(ps)) to_double(ps);}
    /// #define to_double(ps) {(ps)->d = atof((ps)->s); (ps)->s = NULL;}
    /// ```
    ///
    /// and `atof` is `strtod`, so it takes the leading numeric prefix and
    /// answers 0 when there is none. Compiled sCalc: `AA+1` is 13 for AA="12",
    /// 13 for AA="12abc", 1 for AA="abc", and 1001 for AA="1e3".
    ///
    /// This is the ONLY way to read a numeric operand off the sCalc stack.
    /// There is deliberately no fallible accessor: C has no type error in a
    /// numeric position, so the port must not be able to raise one. (The
    /// reverse — a STRING position handed a double — is a real C error for
    /// BIN_READ/BIN_WRITE/SSCANF, and that is what `as_str_ref` is for.)
    pub fn to_double(&self) -> f64 {
        match self {
            StackValue::Double(v) => *v,
            StackValue::Str(s) => super::strtod::strtod(s.as_bytes()).0,
        }
    }

    pub fn as_str_ref(&self) -> Result<&str, CalcError> {
        match self {
            StackValue::Str(s) => Ok(s.as_str()),
            StackValue::Double(_) => Err(CalcError::TypeMismatch),
        }
    }

    pub fn into_string_value(self) -> String {
        match self {
            StackValue::Str(s) => s,
            StackValue::Double(v) => format!("{}", v),
        }
    }
}
