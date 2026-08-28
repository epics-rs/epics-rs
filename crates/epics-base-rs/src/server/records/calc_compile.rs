//! Single owner of "a put landed on a CALC-class field (`CALC`, `OCAL`) →
//! compile it → apply the record type's C disposition".
//!
//! C runs the compile from `special()` (`SPC_CALC`, or the field-index switch
//! in the synApps records), i.e. *after* `dbPut` has already stored the new
//! string. Two dispositions hang off that one hook, and they are not the same:
//!
//! - **calcRecord** (`calcRecord.c:139-155`): a `postfix()` failure is returned
//!   to the caller as `S_db_badField`. `dbPut` then skips the field's monitor
//!   post (`dbAccess.c:1399-1405` — `if (status) goto done`) and `dbPutField`
//!   skips the `pp(TRUE)` process, so the client's write FAILS
//!   (rsrv `write_action` → `ECA_PUTFAIL`). The uncompilable string stays
//!   stored and `RPCL` is left as an empty program.
//! - **calcoutRecord** (`calcoutRecord.c:326-345`), **sCalcoutRecord**
//!   (`sCalcoutRecord.c:462-480`) and **aCalcoutRecord**
//!   (`aCalcoutRecord.c:469-491`): the `postfix()` *return value* is stored in
//!   the `DBF_LONG` `CLCV`/`OCLV` field, `DBE_VALUE` is posted for it, and
//!   `special()` returns 0 — the put SUCCEEDS.
//!
//! Both dispositions consume the same two things: the compiled program and
//! `postfix()`'s return status. That status is **0 on success and -1 on
//! failure** (`postfix.c:239,507`; `sCalcPostfix.c:430,873-881`;
//! `aCalcPostfix.c:437,801-809`) — never the `CALC_ERR_*` code, which only
//! reaches the errlog line. So `caget calcout.CLCV` on a C IOC reads back 0 or
//! -1, and this module is what makes both records agree on that.
//!
//! The three engines differ on the empty expression, and the difference is
//! wire-visible: base `postfix("")` is `CALC_ERR_NULL_ARG` → -1
//! (`postfix.c:235-240`), while `sCalcPostfix("")` / `aCalcPostfix("")` return
//! 0 with an empty program (`sCalcPostfix.c:432-434`, `aCalcPostfix.c:439-441`).
//! They do NOT differ on what that program then does: it fails every evaluation
//! (see [`CompiledExpr::empty`]).
//!
//! # Invariant
//!
//! **A CALC-class field always owns a program.** Success, empty and failure all
//! produce one — C never leaves the `RPCL` buffer absent, it writes
//! `END_EXPRESSION` into it on every failure path. So no caller may ask whether
//! a program is present; a broken CALC is not "nothing to run", it is
//! "something that always fails", and that is precisely how C makes it alarm on
//! every process instead of only at compile time.

use crate::calc::{CalcError, CompiledExpr, ExprKind, calc_error_str};

/// C `postfix()` success status.
pub(crate) const POSTFIX_OK: i32 = 0;
/// C `postfix()` failure status — what lands in `CLCV`/`OCLV`.
pub(crate) const POSTFIX_ERR: i32 = -1;

/// What a CALC-class compile produced: C's `RPCL`/`ORPC` program plus the
/// `postfix()` return status.
pub(crate) struct CalcCompile {
    /// The compiled program. Never absent — a failed compile carries C's empty
    /// `END_EXPRESSION` program, which every engine refuses to run.
    pub program: CompiledExpr,
    /// C `postfix()`'s return value: [`POSTFIX_OK`] or [`POSTFIX_ERR`].
    pub status: i32,
    /// The `CALC_ERR_*` condition `postfix()` reports through its
    /// `short *perror` out-parameter. Distinct from `status`, which is only
    /// 0/-1: this is what `calcErrorStr` turns into the record's own errlog
    /// line, and it never reaches `CLCV`. Private, and read only through
    /// [`CalcCompile::error_str`], so no caller can pair it with the wrong
    /// `status`.
    error: Option<CalcError>,
}

impl CalcCompile {
    fn ok(program: CompiledExpr) -> Self {
        Self {
            program,
            status: POSTFIX_OK,
            error: None,
        }
    }

    /// C's `bad:` label — `*pdest = END_EXPRESSION; return -1;`.
    fn failed(kind: ExprKind, record: &str, field: &str, expr: &str, err: &CalcError) -> Self {
        // C `errlogPrintf("%s.CALC: %s in expression \"%s\"\n", prec->name,
        // calcErrorStr(error_number), prec->calc)` — calcRecord.c:150-151,
        // calcoutRecord.c:331-333.
        tracing::error!(
            target: "epics_base_rs::record",
            "{record}.{field}: {} in expression \"{expr}\"",
            calc_error_str(err.code()).unwrap_or("Unknown error")
        );
        Self {
            program: CompiledExpr::empty(kind),
            status: POSTFIX_ERR,
            error: Some(err.clone()),
        }
    }

    /// C `calcErrorStr(error_number)` for a failed compile, `None` for one that
    /// succeeded — C `if (prec->clcv)` and the errlog line's `%s` in one ask,
    /// so a caller cannot test one and render the other.
    pub fn error_str(&self) -> Option<&'static str> {
        self.error
            .as_ref()
            .map(|e| calc_error_str(e.code()).unwrap_or("Unknown error"))
    }
}

/// C base `postfix()` (`postfix.c`) — the numeric engine behind calc, calcout
/// and swait. An empty expression is `CALC_ERR_NULL_ARG`, status -1
/// (`crate::calc::compile` owns that rule).
pub(crate) fn postfix(record: &str, field: &str, expr: &str) -> CalcCompile {
    match crate::calc::compile(expr) {
        Ok(program) => CalcCompile::ok(program),
        Err(e) => CalcCompile::failed(ExprKind::Numeric, record, field, expr, &e),
    }
}

/// C `sCalcPostfix()` (`sCalcPostfix.c:410-434`) — the string engine behind
/// scalcout. An empty expression is a valid empty program, status 0, unlike
/// base `postfix()`.
pub(crate) fn scalc_postfix(record: &str, field: &str, expr: &str) -> CalcCompile {
    match crate::calc::scalc_compile(expr) {
        Ok(program) => CalcCompile::ok(program),
        Err(e) => CalcCompile::failed(ExprKind::String, record, field, expr, &e),
    }
}

/// C `aCalcPostfix()` (`aCalcPostfix.c:410-441`) — the array engine behind
/// acalcout. An empty expression is a valid empty program, status 0.
pub(crate) fn acalc_postfix(record: &str, field: &str, expr: &str) -> CalcCompile {
    match crate::calc::acalc_compile(expr) {
        Ok(program) => CalcCompile::ok(program),
        Err(e) => CalcCompile::failed(ExprKind::Array, record, field, expr, &e),
    }
}
