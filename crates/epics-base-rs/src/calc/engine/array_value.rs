use super::error::CalcError;

#[derive(Debug, Clone, PartialEq)]
pub enum ArrayStackValue {
    Double(f64),
    Array(Vec<f64>),
}

impl ArrayStackValue {
    pub fn is_double(&self) -> bool {
        matches!(self, ArrayStackValue::Double(_))
    }

    pub fn is_array(&self) -> bool {
        matches!(self, ArrayStackValue::Array(_))
    }

    pub fn as_f64(&self) -> Result<f64, CalcError> {
        match self {
            ArrayStackValue::Double(v) => Ok(*v),
            ArrayStackValue::Array(arr) => Ok(arr.first().copied().unwrap_or(0.0)),
        }
    }

    pub fn as_array(&self) -> Result<&[f64], CalcError> {
        match self {
            ArrayStackValue::Array(arr) => Ok(arr),
            ArrayStackValue::Double(_) => Err(CalcError::TypeMismatch),
        }
    }

    pub fn broadcast(self, target_len: usize) -> Vec<f64> {
        match self {
            ArrayStackValue::Double(v) => vec![v; target_len],
            ArrayStackValue::Array(arr) => arr,
        }
    }

    pub fn map<F: Fn(f64) -> f64>(self, f: F) -> ArrayStackValue {
        match self {
            ArrayStackValue::Double(v) => ArrayStackValue::Double(f(v)),
            ArrayStackValue::Array(arr) => ArrayStackValue::Array(arr.into_iter().map(f).collect()),
        }
    }
}

pub fn zip_map<F: Fn(f64, f64) -> f64>(
    a: ArrayStackValue,
    b: ArrayStackValue,
    f: F,
) -> Result<ArrayStackValue, CalcError> {
    match (a, b) {
        (ArrayStackValue::Double(x), ArrayStackValue::Double(y)) => {
            Ok(ArrayStackValue::Double(f(x, y)))
        }
        (ArrayStackValue::Array(a), ArrayStackValue::Array(b)) => {
            if a.len() != b.len() {
                return Err(CalcError::LengthMismatch);
            }
            Ok(ArrayStackValue::Array(
                a.into_iter()
                    .zip(b.into_iter())
                    .map(|(x, y)| f(x, y))
                    .collect(),
            ))
        }
        (ArrayStackValue::Array(arr), ArrayStackValue::Double(scalar)) => Ok(
            ArrayStackValue::Array(arr.into_iter().map(|x| f(x, scalar)).collect()),
        ),
        (ArrayStackValue::Double(scalar), ArrayStackValue::Array(arr)) => Ok(
            ArrayStackValue::Array(arr.into_iter().map(|y| f(scalar, y)).collect()),
        ),
    }
}
