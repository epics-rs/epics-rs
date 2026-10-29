//! The calls of base's `epicsUnitTest.c` that the validate programs make,
//! under the Perl harness (`HARNESS_ACTIVE`, which every validate `.t`
//! sets): the TAP lines on stdout, and `testDone`'s status without its
//! summary.
//!
//! Each result is also kept for the harness. A `testOk1` result has no
//! description: C's is the stringified condition of the generated C, which
//! has no counterpart in the generated Rust.

use std::ffi::{c_char, c_int};

use epics_base_rs::runtime::stdio::stdout_bytes;
use epics_seq_rs::SeqVar;
use epics_seq_rs::c::{CArg, CSrc, sprintf};
use parking_lot::Mutex;

/// One `ok` / `not ok` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestResult {
    pub ok: bool,
    /// What follows ` - `; `None` for `testOk1`.
    pub desc: Option<String>,
}

/// What the program has reported so far.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// `testPlan`'s argument; 0 before it is called.
    pub planned: c_int,
    pub results: Vec<TestResult>,
    /// `testDiag`'s lines, without the `# `.
    pub diags: Vec<String>,
    /// `testDone`'s return, once it is called.
    pub done: Option<c_int>,
}

// The names `epicsUnitTest.h` gives them, which the programs call.
pub use self::{
    test_diag as testDiag, test_done as testDone, test_fail as testFail, test_ok as testOk,
    test_ok1 as testOk1, test_pass as testPass, test_plan as testPlan,
};

static REPORT: Mutex<Report> = Mutex::new(Report {
    planned: 0,
    results: Vec::new(),
    diags: Vec::new(),
    done: None,
});

/// The report as it stands.
pub fn report() -> Report {
    REPORT.lock().clone()
}

/// `testPlan`.
pub fn test_plan(plan: c_int) {
    let mut r = REPORT.lock();
    // C's resets its counts; a diagnostic is output, not a count.
    *r = Report {
        planned: plan,
        diags: std::mem::take(&mut r.diags),
        ..Report::default()
    };
    if plan != 0 {
        stdout_bytes(format!("1..{plan}\n").as_bytes());
    }
}

/// `testOkV`, its description already formatted.
fn test_ok_v(pass: bool, desc: Option<String>) -> c_int {
    let mut r = REPORT.lock();
    let tested = r.results.len() + 1;
    let result = if pass { "ok" } else { "not ok" };
    let shown = desc.as_deref().unwrap_or("testOk1");
    stdout_bytes(format!("{result} {tested:2} - {shown}\n").as_bytes());
    r.results.push(TestResult { ok: pass, desc });
    pass.into()
}

/// `vprintf` into a string.
fn format(fmt: impl CSrc, args: &[CArg<'_>]) -> String {
    let buf = SeqVar::new([0 as c_char; 1024]);
    let len = sprintf(&buf, fmt, args);
    assert!((0..1024).contains(&len), "description of {len} bytes");
    let bytes: Vec<u8> = buf.get()[..len as usize].iter().map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// `testOk`.
pub fn test_ok(pass: c_int, fmt: impl CSrc, args: &[CArg<'_>]) -> c_int {
    test_ok_v(pass != 0, Some(format(fmt, args)))
}

/// `testOk1`: C's `testOk(cond, "%s", #cond)`.
pub fn test_ok1(pass: c_int) -> c_int {
    test_ok_v(pass != 0, None)
}

/// `testPass`.
pub fn test_pass(fmt: impl CSrc, args: &[CArg<'_>]) {
    test_ok_v(true, Some(format(fmt, args)));
}

/// `testFail`.
pub fn test_fail(fmt: impl CSrc, args: &[CArg<'_>]) {
    test_ok_v(false, Some(format(fmt, args)));
}

/// `testDiag`.
pub fn test_diag(fmt: impl CSrc, args: &[CArg<'_>]) -> c_int {
    let diag = format(fmt, args);
    let mut r = REPORT.lock();
    stdout_bytes(format!("# {diag}\n").as_bytes());
    r.diags.push(diag);
    0
}

/// `testDone` under the Perl harness: 2 when the count is not the plan, 1
/// when a test failed.
pub fn test_done() -> c_int {
    let mut r = REPORT.lock();
    let tested = r.results.len() as c_int;
    let mut status = 0;
    if r.planned == 0 {
        stdout_bytes(format!("1..{tested}\n").as_bytes());
    } else if tested != r.planned {
        status = 2;
    }
    if r.results.iter().any(|t| !t.ok) {
        status |= 1;
    }
    r.done = Some(status);
    status
}
