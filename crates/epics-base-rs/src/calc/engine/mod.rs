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
    /// C's `USES_STRING` marker — the byte `sCalcPostfix` writes at the head of
    /// the postfix, and the ONLY thing `sCalcPerform` looks at to choose between
    /// its two evaluators (`sCalcPerform.c:399`).
    ///
    /// It is a property of the SOURCE, not of the compiled code: C sets it while
    /// looking each element up (`sCalcPostfix.c:447-471`), so it latches on the
    /// element the lexer FOUND — before the compiler has had a chance to rewrite
    /// it. `AA:="x";1` is the case that pins the difference: `AA` is looked up as
    /// `FETCH_AA` (string-typed → marker set) and only afterwards retracted and
    /// rewritten to `STORE_AA` (`:552-557`), which is NOT in the marker's list.
    /// So the program is USES_STRING even though no string-typed opcode survives
    /// in it, and C runs the string evaluator — the one whose STORE_AA writes the
    /// string args.
    ///
    /// A re-scan of the finished opcodes cannot see that, which is why the flag is
    /// recorded once, at the lookup, by its single owner (`postfix::compile`) and
    /// carried here. Always `false` for the numeric and array engines, which have
    /// no such marker.
    pub uses_string: bool,
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
            // C writes `NO_STRING` first and only overwrites it when an element
            // says otherwise (`sCalcPostfix.c:437`); the empty program has no
            // elements. It is refused before the marker is ever read, anyway.
            uses_string: false,
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

/// The value the ISINF operator pushes in all three engines — a BOOLEAN
/// predicate: 1 for either infinity, 0 for anything else (finite or NaN).
///
/// DEVIATION from C, deliberate — CBUG-A3. All three C engines assign the
/// `isinf` macro's result straight into a double (`calcPerform.c:277`,
/// `sCalcPerform.c:703,1407`, `aCalcPerform.c:826,1084`), and on glibc `isinf`
/// expands to `__builtin_isinf_sign`, which returns the SIGN: `-inf` yields
/// **-1**. So `ISINF(A)` stores -1 for a negative infinity, and a downstream
/// `ISINF(A) == 1` test misfires. That is not what the record documents —
/// `calcRecord.dbd.pod:263` defines `ISINF (arg)` as "returns non-zero if any
/// argument is Inf", a predicate — and it is not portable either: an IOC whose
/// `isinf` resolves to the C99 macro gets +1 for -Inf, so C's own answer here
/// depends on the libc it was built against. The intended value is the boolean.
///
/// The three engines share this one definition so the answer cannot drift apart
/// between them.
pub(crate) fn isinf(v: f64) -> f64 {
    if v.is_infinite() { 1.0 } else { 0.0 }
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
/// `string::subrange_bounds`.
pub(crate) fn subrange_bounds(i: i64, j: i64, k: i64) -> (i64, i64) {
    let wrap = |v: i64| if v < 0 { v + k } else { v };
    (wrap(i).clamp(0, k), wrap(j).min(k))
}

/// `ABS_VAL` as the two synApps engines implement it — NOT `fabs`.
///
/// All four sCalc/aCalc sites are the same conditional negate:
///
/// ```c
/// case ABS_VAL: if (*pd < 0) *pd *= -1; break;   /* sCalcPerform.c:513-515 */
/// case ABS_VAL: toDouble(ps); if (ps->d < 0) ps->d *= -1; break;  /* :1046-1049 */
/// case ABS_VAL: for (i=0;i<arraySize;i++) {if (ps->a[i] < 0) ps->a[i] *= -1;}
///                                                /* aCalcPerform.c:771 (array) */
/// case ABS_VAL: if (ps->d < 0) {ps->d *= -1;}    /* aCalcPerform.c:1040 (scalar) */
/// ```
///
/// `-0.0 < 0` is FALSE, so a negative zero is left alone: compiled sCalc/aCalc
/// answer `ABS(0*(0-1))` with `-0.0`. `fabs` would clear the sign bit.
///
/// Base's `calc` is the dialect that genuinely calls `fabs` (`calcPerform.c:174-176`)
/// and must keep doing so — the two C dialects disagree here, and this helper
/// exists so the disagreement is stated once instead of being re-decided at each
/// operator site.
pub(crate) fn abs_val(x: f64) -> f64 {
    if x < 0.0 { -x } else { x }
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

    /// How many of `vars` the CALLER actually supplied — the count sCalc and
    /// aCalc take as `numArgs` / `num_dArgs` and base's `calcPerform` does NOT.
    ///
    /// C's `calcPerform(double *parg, ...)` takes a bare pointer and indexes all
    /// `CALCPERFORM_NARGS` (21) args out of it unconditionally, so the caller is
    /// on its honour to own 21 doubles at `parg`. `swaitRecord` does not:
    /// `swaitRecord.dbd:250-331` declares A..L and then LA..LL, so
    /// `calcPerform(&pwait->a, ...)` (`swaitRecord.c:409`) makes `parg[12]` alias
    /// `&pwait->la`. In C, `CALC="M"` reads the record's previous A and
    /// `CALC="M:=5"` overwrites LA — corrupting the record's own change-detection
    /// latch. That is an upstream defect (CBUG-G3), not a contract, and the port
    /// does not reproduce it: a variable the caller did not supply does not
    /// exist, so it fetches 0 and a store into it is a no-op — the rule sCalc and
    /// aCalc already state explicitly for their own args.
    ///
    /// The count travels WITH the args because the callers disagree: calc,
    /// calcout and the CALC-evaluating links supply all 21; swait supplies 12.
    num_args: usize,
}

impl NumericInputs {
    /// Every arg present — the caller supplies all [`CALC_NARGS`] of them.
    pub fn new() -> Self {
        Self::with_counts(CALC_NARGS)
    }

    /// C's `calcPerform(parg, ...)` plus the count C omits: the args and the
    /// bound that says how many of them are real, handed over together.
    ///
    /// A count above [`CALC_NARGS`] clamps to it — the engine's array is the whole
    /// world it can reach, so a caller cannot promise more args than exist. That
    /// clamp is what makes `num_args <= vars.len()` hold by construction, so the
    /// accessors below need only the one test.
    pub fn with_counts(num_args: usize) -> Self {
        NumericInputs {
            vars: [0.0; CALC_NARGS],
            prev_val: 0.0,
            num_args: num_args.min(CALC_NARGS),
        }
    }

    pub fn with_vars(vars: [f64; CALC_NARGS]) -> Self {
        NumericInputs {
            vars,
            prev_val: 0.0,
            num_args: CALC_NARGS,
        }
    }

    /// The one read path for an arg. `None` is "the caller never supplied this
    /// one", which the engine turns into 0 rather than into the caller's next
    /// field.
    pub(crate) fn num_arg(&self, i: usize) -> Option<f64> {
        (i < self.num_args).then(|| self.vars[i])
    }

    /// The one write path for an arg. `None` means the store is a no-op.
    pub(crate) fn num_arg_mut(&mut self, i: usize) -> Option<&mut f64> {
        (i < self.num_args).then(|| &mut self.vars[i])
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

    /// C's `numArgs` (`sCalcPerform.c:313`) — how many of the numeric args the
    /// CALLER actually supplied, which is NOT the size of the engine's array.
    /// C's `parg` is a bare `double *` into the caller's record, so every access
    /// is guarded against this count and the slots past it do not exist: a fetch
    /// gives 0 (`:421-427`, `:858-864`), a store is a silent no-op (`:432-438`,
    /// `:882-886`), and the same bound covers the computed `@n` forms (`:444`,
    /// `:732`, `:902`).
    ///
    /// The callers disagree, which is why the count has to travel with the args:
    /// scalcout supplies 12 (`MAX_FIELDS`, `sCalcoutRecord.c:357,768`), transform
    /// supplies 16 (`transformRecord.c:593`).
    num_args: usize,
    /// C's `numSArgs` — the same for the STRING args (`psarg`), guarding
    /// `FETCH_AA..LL` (`:871`), `STORE_AA..LL` (`:891`) and `@@n` (`:914`,
    /// `:1471`). scalcout supplies 12 (`STRING_MAX_FIELDS`); transform supplies
    /// **zero** and a NULL `psarg` (`transformRecord.c:593`) — under transform,
    /// no string arg exists at all, so `AA` reads empty and `AA:=` writes nowhere.
    num_sargs: usize,
}

impl StringInputs {
    /// Every arg present — the caller supplies all [`CALC_NARGS`] of both kinds.
    pub fn new() -> Self {
        Self::with_counts(CALC_NARGS, CALC_NARGS)
    }

    /// C's `sCalcPerform(parg, numArgs, psarg, numSArgs, ...)` — the args and the
    /// counts that bound them, handed over together.
    ///
    /// Counts above [`CALC_NARGS`] clamp to it: the engine's arrays are the whole
    /// world it can reach, so a caller cannot promise more args than exist. That
    /// clamp is what makes `num_args <= num_vars.len()` hold by construction, so
    /// the accessors below need only the one test.
    pub fn with_counts(num_args: usize, num_sargs: usize) -> Self {
        StringInputs {
            num_vars: [0.0; CALC_NARGS],
            str_vars: std::array::from_fn(|_| ScalcString::new()),
            prev_val: 0.0,
            prev_sval: ScalcString::new(),
            num_args: num_args.min(CALC_NARGS),
            num_sargs: num_sargs.min(CALC_NARGS),
        }
    }

    /// The one read path for a numeric arg. `None` is C's "caller didn't supply a
    /// large enough array" — the engine turns it into 0.
    pub(crate) fn num_arg(&self, i: usize) -> Option<f64> {
        (i < self.num_args).then(|| self.num_vars[i])
    }

    /// The one write path for a numeric arg. `None` means the store is a no-op,
    /// as C's un-taken `if (numArgs > ...)` branch is.
    pub(crate) fn num_arg_mut(&mut self, i: usize) -> Option<&mut f64> {
        (i < self.num_args).then(|| &mut self.num_vars[i])
    }

    /// The one read path for a string arg; `None` reads as the empty string.
    pub(crate) fn str_arg(&self, i: usize) -> Option<&ScalcString> {
        (i < self.num_sargs).then(|| &self.str_vars[i])
    }

    /// The one write path for a string arg; `None` stores nothing.
    pub(crate) fn str_arg_mut(&mut self, i: usize) -> Option<&mut ScalcString> {
        (i < self.num_sargs).then(|| &mut self.str_vars[i])
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

    /// C's `num_dArgs` (`aCalcPerform.c:298`) — how many SCALAR args the caller
    /// supplied. Guards `FETCH_A..P` (`:432`), `STORE_A..P` (`:462`), `@n`
    /// (`:499`, `:1469`) and the FITQ/FITMQ coefficient write-back (`:1230-1232`,
    /// `:1279-1281`). acalcout supplies 12 (`MAX_FIELDS`, `aCalcoutRecord.c:1283`).
    num_dargs: usize,
    /// C's `num_aArgs` — the same for the ARRAY args, guarding `FETCH_AA..LL`
    /// (`:446`), `STORE_AA..LL` (`:471`) and `@@n` (`:510`, `:1485`). acalcout
    /// supplies 12 (`ARRAY_MAX_FIELDS`).
    num_aargs: usize,
}

impl ArrayInputs {
    /// Every arg present — the caller supplies all [`CALC_NARGS`] of both kinds.
    pub fn new(array_size: usize) -> Self {
        Self::with_counts(array_size, CALC_NARGS, CALC_NARGS)
    }

    /// C's `aCalcPerform(p_dArg, num_dArgs, pp_aArg, num_aArgs, arraySize, ...)`
    /// — the args and the counts that bound them, handed over together. Counts
    /// clamp to [`CALC_NARGS`] for the reason [`StringInputs::with_counts`] gives.
    pub fn with_counts(array_size: usize, num_dargs: usize, num_aargs: usize) -> Self {
        ArrayInputs {
            num_vars: [0.0; CALC_NARGS],
            arrays: vec![Vec::new(); CALC_NARGS],
            array_size,
            prev_val: 0.0,
            prev_aval: vec![0.0; array_size],
            amask: 0,
            num_dargs: num_dargs.min(CALC_NARGS),
            num_aargs: num_aargs.min(CALC_NARGS),
        }
    }

    /// The one read path for a scalar arg; `None` is C's missing arg, which fetches 0.
    pub(crate) fn num_arg(&self, i: usize) -> Option<f64> {
        (i < self.num_dargs).then(|| self.num_vars[i])
    }

    /// The one write path for a scalar arg; `None` stores nothing.
    pub(crate) fn num_arg_mut(&mut self, i: usize) -> Option<&mut f64> {
        (i < self.num_dargs).then(|| &mut self.num_vars[i])
    }

    /// The one read path for an array arg. `None` is C's missing arg — which,
    /// like an arg the record never allocated, reads as an `array_size` buffer of
    /// zeros, not a scalar (`aCalcPerform.c:443-454`, `:1485-1493`).
    pub(crate) fn array_arg(&self, i: usize) -> Option<&Vec<f64>> {
        (i < self.num_aargs).then(|| &self.arrays[i])
    }

    /// The one write path for an array arg — `STORE_AA..LL` (`:466-491`) and
    /// `@@n :=` (`:506-525`), which are the same store with a different index.
    ///
    /// It owns [`Self::amask`]: C sets `*amask |= 1<<i` INSIDE the `num_aArgs`
    /// guard, so a refused store must not flag the field. Routing both opcodes
    /// through here is what keeps "a store cannot land without its bit, and a bit
    /// cannot be set without a store" true by construction.
    pub(crate) fn store_array(&mut self, i: usize, buf: Vec<f64>) {
        if i < self.num_aargs {
            self.arrays[i] = buf;
            self.amask |= 1 << i;
        }
    }
}

impl Default for ArrayInputs {
    fn default() -> Self {
        Self::new(1)
    }
}
