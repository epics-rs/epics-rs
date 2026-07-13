pub mod cast;
pub mod cvt;
pub mod error;
pub mod numeric;
pub mod opcodes;
pub mod postfix;
pub mod random;
pub mod scanf;
pub mod strtod;
pub mod token;

pub mod checksum;
pub mod string;
pub mod value;

pub mod array;
pub mod array_value;

use error::CalcError;
use opcodes::Opcode;
use value::ScalcString;

pub type CalcResult<T> = Result<T, CalcError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// C's `*pout = END_EXPRESSION` buffer — the program a compiler leaves
    /// behind when it has nothing to say.
    ///
    /// Every failure path in all three C compilers writes it before returning
    /// -1 (`postfix.c:238,506`; `sCalcPostfix.c:429,880`;
    /// `aCalcPostfix.c:434,808`), and the empty expression IS it
    /// (`sCalcPostfix.c:432-434`, `aCalcPostfix.c:439-441`). It is a real,
    /// runnable program, not the absence of one — which is why a record never
    /// has to ask whether it has a program. Running it is an error
    /// ([`is_empty`](Self::is_empty)).
    pub fn empty(kind: ExprKind) -> Self {
        Self {
            code: Vec::new(),
            kind,
            loop_pairs: Vec::new(),
        }
    }

    /// C `*post == END_EXPRESSION` — the program has no instruction to run.
    ///
    /// The single definition of that test: all three evaluators refuse such a
    /// program, so a failed or empty compile fails every evaluation instead of
    /// quietly yielding a value.
    pub fn is_empty(&self) -> bool {
        self.code
            .iter()
            .all(|op| matches!(op, Opcode::Core(opcodes::CoreOp::End)))
    }

    /// C's UNTIL ceiling — a STATIC pre-scan of the whole postfix, run before a
    /// single opcode executes (`sCalcPerform.c:341-365`, and `aCalcPerform.c:355-390`
    /// with `MAX_UNTIL_OP` for the literal 10):
    ///
    /// ```c
    /// /* find all UNTIL operators in postfix, noting their locations */
    /// for (i=0, post=postfix; *post != END_EXPRESSION; post++) {
    ///     switch (*post) {
    ///     case UNTIL:
    ///         until_scratch[i].until_loc = post;
    ///         i++;
    ///         if (i>9) { printf("sCalcPerform: too many UNTILs\n"); return(-1); }
    ///         break;
    ///     ...
    /// ```
    ///
    /// The count is of UNTIL **opcodes present**, not of UNTILs executed, so
    /// reachability is irrelevant: compiled C fails
    /// `0?(UNTIL(1)+…ten of them…):7` with -1 even though the conditional never
    /// takes that branch — and runs the nine-UNTIL version to 7.
    ///
    /// Both evaluators used to count at RUN time instead, incrementing as each new
    /// UNTIL was first reached, so the ten-on-a-dead-branch program evaluated
    /// happily. This is the single owner of the ceiling for both of them; the
    /// scratch table each keeps at run time is now only a location map, with no
    /// ceiling of its own to enforce.
    pub fn check_until_ceiling(&self) -> Result<(), CalcError> {
        let untils = self
            .code
            .iter()
            .filter(|op| matches!(op, Opcode::Control(opcodes::ControlOp::Until(_))))
            .count();
        if untils > MAX_UNTILS {
            return Err(CalcError::Overflow);
        }
        Ok(())
    }

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

/// C `isinf` — the value the ISINF operator pushes in all three engines
/// (`calcPerform.c:277`, `sCalcPerform.c:1407`, `aCalcPerform.c:826,1084`,
/// each of which assigns the macro's result straight into a `double`).
///
/// It is a SIGN, not a predicate: glibc expands `isinf` to
/// `__builtin_isinf_sign`, so `-inf` is `-1`, `+inf` is `+1`, everything else
/// (finite, NaN) is `0`. Verified on this host by compiling the macro.
///
/// The three engines share this one definition so the sign cannot drift back to
/// a boolean in one of them.
pub(crate) fn c_isinf(v: f64) -> f64 {
    if v.is_infinite() {
        if v.is_sign_negative() { -1.0 } else { 1.0 }
    } else {
        0.0
    }
}

/// C's subrange bound rule for a pair of NUMERIC bounds over a container of
/// length `k` — aCalc's `[` over `arraySize` (`aCalcPerform.c:1527-1534`), and
/// sCalc's `isDouble` branch over `strlen(ps->s)` (`sCalcPerform.c:1876-1886`),
/// which is the same arithmetic:
///
/// ```c
/// i = (int)ps1->d;  if (i < 0) i += k;
/// j = (int)ps2->d;  if (j < 0) j += k;
/// i = myMAX(myMIN(i,k),0);  j = myMIN(j,k);
/// ```
///
/// So: a negative bound counts back from the end, `j` is INCLUSIVE, and only `i`
/// is clamped from below — C leaves a `j` that is still negative alone, which is
/// what makes an inverted range select nothing.
///
/// sCalc alone can also take a STRING bound, which is searched for rather than
/// counted; that branch (and the wrap it must NOT do) lives with the operator, in
/// [`string::subrange_bounds`].
pub(crate) fn subrange_bounds(i: i64, j: i64, k: i64) -> (i64, i64) {
    let wrap = |v: i64| if v < 0 { v + k } else { v };
    (wrap(i).clamp(0, k), wrap(j).min(k))
}

/// Number of named scalar inputs accepted by the calc engine.
/// Mirrors `CALCPERFORM_NARGS` in epics-base after PR #655 (12 → 21,
/// fields A..U). Doubled-letter previous-value slots (LA..LU) and any
/// per-record array slots scale to the same size.
pub const CALC_NARGS: usize = 21;

/// C's `until_scratch[10]` guarded by `if (i>9) return(-1)` (`sCalcPerform.c:329`,
/// `:356-360`) and aCalc's `MAX_UNTIL_OP 10` with `if (i > (MAX_UNTIL_OP-1))`
/// (`aCalcPerform.c:297`, `:369-373`). The TENTH distinct UNTIL fails the perform,
/// so the usable ceiling is nine — the same in both engines.
const MAX_UNTILS: usize = 9;

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
    pub num_vars: [f64; CALC_NARGS], // A..U
    /// AA..UU. C's are `char *psarg[]`, pointing at the record's `char[40]`
    /// string fields (`sCalcoutRecord.c:357`), and `FETCH_AA` copies one into
    /// the 40-byte stack element — so an input, like every other string in the
    /// engine, is a [`ScalcString`].
    pub str_vars: [ScalcString; CALC_NARGS],
    /// Previous calculation result, read by the `VAL` token (C `FETCH_VAL`).
    pub prev_val: f64,
    /// Previous *string* calculation result, read by the `SVAL` token
    /// (C `FETCH_SVAL`, sCalcPerform.c:927-932, which pushes `psresult`).
    /// Empty for a fresh evaluation, and for callers whose C counterpart
    /// passes no `psresult` (numeric `calcPerform`).
    pub prev_sval: ScalcString,
}

impl StringInputs {
    pub fn new() -> Self {
        StringInputs {
            num_vars: [0.0; CALC_NARGS],
            str_vars: std::array::from_fn(|_| ScalcString::new()),
            prev_val: 0.0,
            prev_sval: ScalcString::new(),
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
    /// Previous ARRAY result — C's `p_aresult`, read by the `AVAL` token
    /// (`FETCH_AVAL`, aCalcPerform.c:534-539). The array counterpart of
    /// [`Self::prev_val`].
    pub prev_aval: Vec<f64>,
    /// C's `*amask` (`aCalcPerform.c:300`) — bit `i` marks that the expression
    /// STORED into array variable `i` (AA..LL) during this run.
    ///
    /// C hands aCalcPerform a pointer to the record's own arrays, so a store lands
    /// in the record directly and this mask is how the caller learns WHICH ones
    /// changed: `afterCalc` posts exactly the flagged fields (`aCalcoutRecord.c:293-297`).
    ///
    /// The engine OWNS it. It is reset at the top of every run (`:326`, `*amask = 0`)
    /// and set only by the array-store opcodes (`:487`, `:524`), so a caller cannot
    /// see a stale bit and a store cannot land without its bit. Read it AFTER the
    /// run, never set it before.
    pub amask: u32,
}

impl ArrayInputs {
    pub fn new(array_size: usize) -> Self {
        ArrayInputs {
            num_vars: [0.0; CALC_NARGS],
            arrays: vec![Vec::new(); CALC_NARGS],
            array_size,
            prev_val: 0.0,
            prev_aval: vec![0.0; array_size],
            amask: 0,
        }
    }
}

impl Default for ArrayInputs {
    fn default() -> Self {
        Self::new(1)
    }
}
