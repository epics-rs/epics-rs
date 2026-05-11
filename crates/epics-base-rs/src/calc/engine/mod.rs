pub mod error;
pub mod numeric;
pub mod opcodes;
pub mod postfix;
pub mod token;

pub mod checksum;
pub mod string;
pub mod value;

pub mod array;
pub mod array_value;

use error::CalcError;
use opcodes::Opcode;

pub type CalcResult<T> = Result<T, CalcError>;

#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    Numeric,
    String,
    Array,
}

#[derive(Debug, Clone)]
pub struct CompiledExpr {
    pub code: Vec<Opcode>,
    pub kind: ExprKind,
    pub loop_pairs: Vec<(usize, usize)>,
}

/// Number of named scalar inputs accepted by the calc engine.
/// Mirrors `CALCPERFORM_NARGS` in epics-base after PR #655 (12 → 21,
/// fields A..U). Doubled-letter previous-value slots (LA..LU) and any
/// per-record array slots scale to the same size.
pub const CALC_NARGS: usize = 21;

#[derive(Debug, Clone)]
pub struct NumericInputs {
    pub vars: [f64; CALC_NARGS],
}

impl NumericInputs {
    pub fn new() -> Self {
        NumericInputs {
            vars: [0.0; CALC_NARGS],
        }
    }

    pub fn with_vars(vars: [f64; CALC_NARGS]) -> Self {
        NumericInputs { vars }
    }
}

impl Default for NumericInputs {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct StringInputs {
    pub num_vars: [f64; CALC_NARGS],    // A..U
    pub str_vars: [String; CALC_NARGS], // AA..UU
}

impl StringInputs {
    pub fn new() -> Self {
        StringInputs {
            num_vars: [0.0; CALC_NARGS],
            str_vars: std::array::from_fn(|_| String::new()),
        }
    }
}

impl Default for StringInputs {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct ArrayInputs {
    pub num_vars: [f64; CALC_NARGS],
    pub arrays: Vec<Vec<f64>>, // len CALC_NARGS (AA..UU)
    pub array_size: usize,
}

impl ArrayInputs {
    pub fn new(array_size: usize) -> Self {
        ArrayInputs {
            num_vars: [0.0; CALC_NARGS],
            arrays: vec![Vec::new(); CALC_NARGS],
            array_size,
        }
    }
}

impl Default for ArrayInputs {
    fn default() -> Self {
        Self::new(1)
    }
}
