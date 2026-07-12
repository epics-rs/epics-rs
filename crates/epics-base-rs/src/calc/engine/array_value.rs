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

    /// C `to_array(..., setValues=1)` (`aCalcPerform.c:124-143`) — the ONLY way
    /// a scalar becomes an array in aCalc, and it does not simply repeat the
    /// scalar: **a NaN scalar fills the array with 0**, not with NaN
    /// (`:135-138`). Every promotion must go through here so that rule cannot be
    /// bypassed.
    pub fn to_array(self, array_size: usize) -> Vec<f64> {
        match self {
            ArrayStackValue::Double(v) => {
                let fill = if v.is_nan() { 0.0 } else { v };
                vec![fill; array_size]
            }
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
        // C's binary arms promote the LEFT operand only — `toArray(ps,1)`
        // (`aCalcPerform.c:630`, :1338) — and then read the right one as the
        // plain double `ps1->d` (:659-684). The two mixed shapes are therefore
        // NOT mirror images when the scalar is NaN:
        //
        //   array OP NaN -> the NaN goes into every element (no promotion)
        (ArrayStackValue::Array(arr), ArrayStackValue::Double(scalar)) => Ok(
            ArrayStackValue::Array(arr.into_iter().map(|x| f(x, scalar)).collect()),
        ),
        //   NaN OP array -> the NaN is promoted, and `to_array` turns it into 0
        (ArrayStackValue::Double(scalar), ArrayStackValue::Array(arr)) => {
            let len = arr.len();
            let left = ArrayStackValue::Double(scalar).to_array(len);
            Ok(ArrayStackValue::Array(
                left.into_iter().zip(arr).map(|(x, y)| f(x, y)).collect(),
            ))
        }
    }
}
