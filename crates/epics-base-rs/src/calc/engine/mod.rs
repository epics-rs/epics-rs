pub mod cast;
pub mod error;
pub mod numeric;
pub mod opcodes;
pub mod postfix;
pub mod strtod;
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

impl CompiledExpr {
    /// Compute which input arguments this expression reads and which it
    /// stores into, equivalent to C `calcArgUsage` (`calcPerform.c:429-507`).
    ///
    /// Returns `(inputs, stores)` as bitmaps over args A..U (bit 0 = A).
    /// As in C, an argument that is stored to before any read is not
    /// reported as an input (`inputs |= bit & ~stores`).
    pub fn arg_usage(&self) -> (u32, u32) {
        use opcodes::CoreOp;
        let mut inputs: u32 = 0;
        let mut stores: u32 = 0;
        for op in &self.code {
            match op {
                Opcode::Core(CoreOp::End) => break,
                Opcode::Core(CoreOp::PushVar(idx)) | Opcode::Core(CoreOp::PushDoubleVar(idx)) => {
                    if (*idx as usize) < CALC_NARGS {
                        inputs |= (1u32 << *idx) & !stores;
                    }
                }
                Opcode::Core(CoreOp::StoreVar(idx)) | Opcode::Core(CoreOp::StoreDoubleVar(idx)) => {
                    if (*idx as usize) < CALC_NARGS {
                        stores |= 1u32 << *idx;
                    }
                }
                _ => {}
            }
        }
        (inputs, stores)
    }

    /// Produce a human-readable disassembly of the compiled postfix stream,
    /// one opcode per line, analogous to C `calcExprDump` (`postfix.c:541-654`).
    /// Diagnostics only.
    pub fn disassemble(&self) -> String {
        let mut out = String::new();
        for (i, op) in self.code.iter().enumerate() {
            out.push_str(&format!("{:4}: {:?}\n", i, op));
        }
        out
    }
}

/// Number of named scalar inputs accepted by the calc engine.
/// Mirrors `CALCPERFORM_NARGS` in epics-base after PR #655 (12 → 21,
/// fields A..U). Doubled-letter previous-value slots (LA..LU) and any
/// per-record array slots scale to the same size.
pub const CALC_NARGS: usize = 21;

#[derive(Debug, Clone)]
pub struct NumericInputs {
    pub vars: [f64; CALC_NARGS],
    /// Previous calculation result, read by the `VAL` token (C `FETCH_VAL`,
    /// `*presult`). Defaults to 0.0 for a fresh evaluation.
    pub prev_val: f64,
}

impl NumericInputs {
    pub fn new() -> Self {
        NumericInputs {
            vars: [0.0; CALC_NARGS],
            prev_val: 0.0,
        }
    }

    pub fn with_vars(vars: [f64; CALC_NARGS]) -> Self {
        NumericInputs {
            vars,
            prev_val: 0.0,
        }
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
    /// Previous calculation result, read by the `VAL` token (C `FETCH_VAL`).
    pub prev_val: f64,
    /// Previous *string* calculation result, read by the `SVAL` token
    /// (C `FETCH_SVAL`, sCalcPerform.c:927-932, which pushes `psresult`).
    /// Empty for a fresh evaluation, and for callers whose C counterpart
    /// passes no `psresult` (numeric `calcPerform`).
    pub prev_sval: String,
}

impl StringInputs {
    pub fn new() -> Self {
        StringInputs {
            num_vars: [0.0; CALC_NARGS],
            str_vars: std::array::from_fn(|_| String::new()),
            prev_val: 0.0,
            prev_sval: String::new(),
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
    /// Previous calculation result, read by the `VAL` token (C `FETCH_VAL`).
    pub prev_val: f64,
}

impl ArrayInputs {
    pub fn new(array_size: usize) -> Self {
        ArrayInputs {
            num_vars: [0.0; CALC_NARGS],
            arrays: vec![Vec::new(); CALC_NARGS],
            array_size,
            prev_val: 0.0,
        }
    }
}

impl Default for ArrayInputs {
    fn default() -> Self {
        Self::new(1)
    }
}
