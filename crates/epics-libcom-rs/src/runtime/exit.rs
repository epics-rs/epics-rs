//! `epicsExit` — the process's one shutdown owner.
//!
//! This is C `libCom/misc/epicsExit.c`: a process-wide list of callbacks, and
//! the one call that runs them. `softIoc`'s `main` reaches every one of its
//! exits through `epicsExit(status)` (`softMain.cpp:167`, `:172`, `:251`,
//! `:265`, `:270`, `:277`), and `epicsExit` runs the list before handing the
//! status to `exit()` (`epicsExit.c:172-177`). Anything with teardown to do —
//! a driver that owes its device a goodbye frame, a file that must be flushed,
//! a thread that must be joined — registers here and stays ignorant of when
//! shutdown happens or who triggered it.
//!
//! Without it every such teardown is written but never reached: the fix
//! compiles, the `Drop` is correct, and the process exits around it. That was
//! the shape of the defect this module closes — an MQTT driver whose `Drop`
//! sends DISCONNECT while nothing at IOC exit ever dropped the driver.
//!
//! # Semantics, all of them C's
//!
//! * **LIFO.** [`call_at_exits`] pops from the tail (`ellLast`,
//!   `epicsExit.c:88`), so a subsystem is torn down before whatever it was
//!   built on.
//! * **Once.** It takes the list out of the static before running it
//!   (`pExitPvtPerProcess = 0` under the lock, `:104-108`), so a second call —
//!   or a concurrent one — runs nothing. Each callback is therefore
//!   `FnOnce`, which is what C's "call, unlink, free" (`:93-95`) amounts to.
//! * **Unlocked while running.** C releases `exitPvtLock` before calling a
//!   single callback (`:106-113`), which is what lets a callback register
//!   another one, or call [`call_at_exits`] itself, without deadlocking.
//! * **Never removed.** C has no `epicsRemoveAtExit`; a registration lasts for
//!   the process. A caller whose subject may already be gone by exit time
//!   registers something that copes with that, as asyn's `destroyPortDriver`
//!   does by looking the port up by name (`asynManager.c:2026-2043`).
//!
//! C's per-thread half (`epicsAtThreadExit`, `epicsExitCallAtThreadExits`) has
//! no caller here and is not ported: Rust's thread-local `Drop` already runs
//! at thread exit, which is the service that half exists to provide.

use std::sync::Mutex;

/// One registered callback, with the name that identifies it in diagnostics —
/// C's `exitNode` (`epicsExit.c:38-43`), whose `name[]` exists for the same
/// reason (`atExit %s(%p)`, `:90`).
struct ExitNode {
    name: String,
    func: Box<dyn FnOnce() + Send>,
}

/// C's `pExitPvtPerProcess` (`epicsExit.c:53`) behind its `exitPvtLock`
/// (`:54`). `Vec` rather than a list because the only two operations are
/// "append" and "drain from the tail".
static EXIT_LIST: Mutex<Vec<ExitNode>> = Mutex::new(Vec::new());

/// Register `func` to run at process shutdown — C `epicsAtExit3`
/// (`epicsExit.c:158-171`).
///
/// `name` is the diagnostic label C carries in the node; give it the subject,
/// not the verb (`"asynPort SERIAL1"`, not `"close the port"`), because it is
/// what a wedged shutdown is reported against.
///
/// Registrations are never removed, so a callback must be safe to run against a
/// subject that has since gone away.
pub fn at_exit<F>(name: impl Into<String>, func: F)
where
    F: FnOnce() + Send + 'static,
{
    let node = ExitNode {
        name: name.into(),
        func: Box::new(func),
    };
    // Poison-tolerant throughout this module: a thread that panicked holding
    // this lock must not be able to disarm every teardown in the process.
    EXIT_LIST
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(node);
}

/// Run every registered callback, most recent first, and clear the list — C
/// `epicsExitCallAtExits` (`epicsExit.c:100-115`).
///
/// The list is taken under the lock and released before the first callback
/// runs, so a callback may register another (it will not run) or call this
/// again (it will find nothing). Calling it a second time is a no-op, which is
/// what makes it safe to put on more than one exit path.
///
/// This is the IOC's shutdown, not the process's `exit()`: it returns, and the
/// caller decides what happens next. [`exit`] is the pairing C's `main` uses.
pub fn call_at_exits() {
    let nodes = {
        let mut list = EXIT_LIST.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *list)
    };
    for node in nodes.into_iter().rev() {
        tracing::debug!(target: "epics_libcom_rs::exit", name = %node.name, "atExit");
        (node.func)();
    }
}

/// Run the exit callbacks and end the process — C `epicsExit`
/// (`epicsExit.c:172-177`).
///
/// The pause before `exit()` is C's `epicsThreadSleep(0.1)` (`:175`): a
/// callback that asked a thread to stop has, by then, only asked. Nothing here
/// joins those threads, so the pause is all the grace they get — a callback
/// that needs its subject actually gone must wait for it itself.
pub fn exit(status: i32) -> ! {
    call_at_exits();
    std::thread::sleep(std::time::Duration::from_millis(100));
    std::process::exit(status)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex as StdMutex};

    use super::*;

    /// The exit list is the process's, and `call_at_exits` drains all of it —
    /// so two tests running at once would each sweep up the other's
    /// registrations. Every test below takes this first and holds it for its
    /// whole body, which makes each one the process's only exit-list user
    /// while it runs.
    static ONE_AT_A_TIME: StdMutex<()> = StdMutex::new(());

    /// C runs the list from `ellLast` backwards (`epicsExit.c:88`): a
    /// subsystem registered later — and so possibly built on an earlier one —
    /// is torn down first.
    #[test]
    fn callbacks_run_in_reverse_registration_order_and_only_once() {
        let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let log: Arc<StdMutex<Vec<&'static str>>> = Arc::new(StdMutex::new(Vec::new()));

        for name in ["first", "second", "third"] {
            let sink = log.clone();
            at_exit(name, move || sink.lock().unwrap().push(name));
        }

        call_at_exits();
        assert_eq!(
            *log.lock().unwrap(),
            vec!["third", "second", "first"],
            "C pops the exit list from its tail (epicsExit.c:88), so the last \
             registration runs first"
        );

        // C empties the list as it runs it and takes it out of the static
        // first (`pExitPvtPerProcess = 0`, :104-108), so a second call — an
        // IOC that shuts down twice, a signal arriving during shutdown — must
        // not run a single callback again.
        call_at_exits();
        assert_eq!(
            *log.lock().unwrap(),
            vec!["third", "second", "first"],
            "a second call_at_exits must run nothing"
        );
    }

    /// The lock is released before the first callback runs (C :106-113), so a
    /// callback that reaches back into this module cannot deadlock the
    /// shutdown it is part of.
    #[test]
    fn a_callback_may_register_another_without_deadlocking() {
        let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        at_exit("reentrant", move || {
            at_exit("registered from inside a callback", || {});
            let _ = tx.send(());
        });

        // Drain on a worker with a bound: a deadlock here is the failure this
        // test exists to catch, and a deadlocked assertion never reports.
        std::thread::spawn(call_at_exits);
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok(),
            "call_at_exits must not hold the list lock while a callback runs"
        );
    }
}
