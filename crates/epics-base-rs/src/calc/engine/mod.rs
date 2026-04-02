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

#[derive(Debug, Clone)]
pub struct NumericInputs {
    pub vars: [f64; 16],
}

impl NumericInputs {
    pub fn new() -> Self {
        NumericInputs { vars: [0.0; 16] }
    }

    pub fn with_vars(vars: [f64; 16]) -> Self {
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
    pub num_vars: [f64; 16],    // A..P
    pub str_vars: [String; 12], // AA..LL
}

impl StringInputs {
    pub fn new() -> Self {
        StringInputs {
            num_vars: [0.0; 16],
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
    pub num_vars: [f64; 16],
    pub arrays: Vec<Vec<f64>>, // len 12 (AA..LL)
    pub array_size: usize,
}

impl ArrayInputs {
    pub fn new(array_size: usize) -> Self {
        ArrayInputs {
            num_vars: [0.0; 16],
            arrays: vec![Vec::new(); 12],
            array_size,
        }
    }
}

impl Default for ArrayInputs {
    fn default() -> Self {
        Self::new(1)
    }
}
