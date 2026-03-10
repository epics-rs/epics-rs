use crate::engine::error::CalcError;

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

    pub fn as_f64(&self) -> Result<f64, CalcError> {
        match self {
            StackValue::Double(v) => Ok(*v),
            StackValue::Str(_) => Err(CalcError::TypeMismatch),
        }
    }

    pub fn as_str_ref(&self) -> Result<&str, CalcError> {
        match self {
            StackValue::Str(s) => Ok(s.as_str()),
            StackValue::Double(_) => Err(CalcError::TypeMismatch),
        }
    }

    pub fn into_f64_lossy(self) -> f64 {
        match self {
            StackValue::Double(v) => v,
            StackValue::Str(s) => s.parse::<f64>().unwrap_or(0.0),
        }
    }

    pub fn into_string_value(self) -> String {
        match self {
            StackValue::Str(s) => s,
            StackValue::Double(v) => format!("{}", v),
        }
    }
}
