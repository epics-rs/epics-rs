//! `testSupport.c`, and the end of a test program's process.
//!
//! In C, `seq_test_done` arms `epicsAtThreadExit(seq_test_at_thread_exit)`,
//! so the process `epicsExit`s once the program's thread is done with its
//! cleanup, and stop.st's failure path calls `exit(0)` at once. Here the
//! process is the harness's: either end leaves [`exited`] the report as the
//! process would have left it.
//!
//! `run_seq_test` and the priority it adapts are vxWorks-only; a host
//! `seq_test_adapt_priority` is 0, so `seq_test_init` is `testPlan`.

use std::cell::RefCell;
use std::ffi::c_int;

use parking_lot::Mutex;

use crate::unit_test::{Report, report, test_done, test_plan};

/// The report at `epicsExit` or `exit`, once either ran.
static EXITED: Mutex<Option<Report>> = Mutex::new(None);

fn epics_exit() {
    EXITED.lock().get_or_insert_with(report);
}

/// `epicsAtThreadExit(seq_test_at_thread_exit, 0)`: runs as the thread's
/// locals are destroyed.
struct AtThreadExit;

impl Drop for AtThreadExit {
    fn drop(&mut self) {
        epics_exit();
    }
}

thread_local! {
    static AT_THREAD_EXIT: RefCell<Option<AtThreadExit>> = const { RefCell::new(None) };
}

/// `seq_test_init`.
pub fn seq_test_init(num_tests: c_int) {
    test_plan(num_tests);
}

/// `seq_test_done`.
pub fn seq_test_done() {
    test_done();
    AT_THREAD_EXIT.with(|at| *at.borrow_mut() = Some(AtThreadExit));
}

/// `exit`: the process ends here, so the calling thread never returns.
pub fn exit(_status: c_int) -> ! {
    epics_exit();
    loop {
        std::thread::park();
    }
}

/// The report the process exited with, once it has.
pub fn exited() -> Option<Report> {
    EXITED.lock().clone()
}
