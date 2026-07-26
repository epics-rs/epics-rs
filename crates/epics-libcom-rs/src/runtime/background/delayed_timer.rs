//! Delayed-callback timer facility — RTEMS-safe port of
//! `callbackRequestDelayed` and its `epicsTimerQueue` usage in
//! `modules/database/src/ioc/db/callback.c`.
//!
//! # C parity
//!
//! `callbackInit` allocates one timer queue for delayed requests
//! (`timerQueue = epicsTimerQueueAllocate(...)`, `callback.c:300`).
//! `callbackRequestDelayed(pcallback, seconds)` (`callback.c:410-419`) starts a
//! per-callback `epicsTimer` on that queue; when the timer expires, the queue's
//! worker calls `notify` (`callback.c:404-408`), which simply hands the
//! callback to `callbackRequest` — i.e. the timer's only job is to defer the
//! enqueue by `seconds`, after which the normal callback executor runs it.
//!
//! The Rust port keeps that split of responsibility: **one timer thread** owns
//! a deadline-ordered queue and, on expiry, submits the callback into the
//! [`CallbackHandle`] executor pool. It uses `Condvar::wait_timeout` on the
//! nearest deadline — plain `std`, no tokio timer wheel — so it runs on RTEMS.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::callback_executor::{Callback, CallbackHandle, CallbackPriority};
use super::facility::{recover, run_facility_loop, run_isolated};
use crate::runtime::task::{MandatoryThread, ThreadPriority};

/// What this facility is called when it has to report something about itself.
const FACILITY: &str = "delayed-callback timer";

/// How a due timer entry is run when its deadline arrives.
enum TimerAction {
    /// Run the callback **inline on the timer thread**. For a non-blocking
    /// wakeup only — a `sleep`/`interval` waker (`Thread::unpark` for a
    /// `park_on` driver, a `future_exec` task re-enqueue, or a tokio
    /// task-schedule). A wakeup needs no worker, so it does not take one: the
    /// timer thread runs it directly rather than queueing it on a callback
    /// band. That keeps a band's sole job "run futures" and never "wake them",
    /// which is what stopped the sleep-wake self-deadlock a `spawn`ed future
    /// awaiting `sleep` used to hit (`bug_pattern
    /// rtems-exec-sleep-wake-band-deadlock`; the executor no longer parks a
    /// worker per future, but routing wakes off the band is still the rule).
    /// The callback must never block or do real work: it runs on the single
    /// timer thread and delays every later deadline until it returns.
    Inline,
    /// Hand the callback to the executor pool at this band — C
    /// `callbackRequestDelayed` → `callbackRequest` (`callback.c:404-419`). For
    /// genuine deferred *work* (ODLY watchdog, SDLY reprocess) that must run on a
    /// callback-band worker rather than the timer thread.
    Pool(CallbackPriority),
}

/// One scheduled callback awaiting its deadline.
struct TimerEntry {
    deadline: Instant,
    /// Tie-breaker so equal deadlines fire in submission order and `Ord` is a
    /// total order (the callback itself is not comparable).
    seq: u64,
    action: TimerAction,
    cb: Callback,
}

// `BinaryHeap` is a max-heap; order so the *earliest* deadline is "greatest"
// and therefore sits at the top. Ties break on the lower `seq` first.
impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.seq == other.seq
    }
}
impl Eq for TimerEntry {}
impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .deadline
            .cmp(&self.deadline)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}
impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

struct TimerState {
    heap: BinaryHeap<TimerEntry>,
    next_seq: u64,
    shutdown: bool,
}

struct Inner {
    state: Mutex<TimerState>,
    wake: Condvar,
    sink: CallbackHandle,
}

impl Inner {
    /// Insert a scheduled callback and wake the timer thread so it can
    /// recompute its sleep deadline. Port of `epicsTimerStartDelay`
    /// (`callback.c:418`).
    fn schedule(&self, delay: Duration, action: TimerAction, cb: Callback) {
        let deadline = Instant::now() + delay;
        let mut st = recover(FACILITY, self.state.lock());
        if st.shutdown {
            // Timer thread stopped: match C dropping late delayed requests
            // during shutdown. Drop `cb` (never scheduled or fired) instead of
            // pushing it onto a heap no worker will ever drain.
            drop(st);
            tracing::trace!(
                target: "epics_base_rs::runtime::delayed_timer",
                "callbackRequestDelayed after shutdown dropped"
            );
            return;
        }
        let seq = st.next_seq;
        st.next_seq += 1;
        st.heap.push(TimerEntry {
            deadline,
            seq,
            action,
            cb,
        });
        drop(st);
        self.wake.notify_one();
    }
}

/// Port of the timer-queue worker: sleep until the nearest deadline, then hand
/// every due callback to the executor pool via `notify`
/// (`callback.c:404-408`).
fn timer_loop(inner: &Inner) {
    let mut st = recover(FACILITY, inner.state.lock());
    loop {
        if st.shutdown {
            return;
        }
        let now = Instant::now();
        // `deadline` is `Copy`, so this releases the peek borrow immediately.
        match st.heap.peek().map(|e| e.deadline) {
            Some(deadline) if deadline <= now => {
                // Due: run it. A wakeup (`Inline`) runs here on the timer thread
                // — it only unparks a driver, needs no worker, and must not queue
                // on the callback pool (that is the sleep-wake self-deadlock). A
                // deferred-work callback (`Pool`) is handed to the executor pool.
                let entry = st.heap.pop().unwrap();
                drop(st);
                match entry.action {
                    TimerAction::Inline => {
                        run_isolated(FACILITY, entry.cb);
                    }
                    TimerAction::Pool(priority) => {
                        let _ = inner.sink.request(priority, entry.cb);
                    }
                }
                st = recover(FACILITY, inner.state.lock());
            }
            Some(deadline) => {
                // Not yet due: sleep until it is, or until a nearer request
                // wakes us.
                let wait = deadline.saturating_duration_since(now);
                let (guard, _timeout) = recover(FACILITY, inner.wake.wait_timeout(st, wait));
                st = guard;
            }
            None => {
                // Nothing scheduled: sleep until a request or shutdown.
                st = recover(FACILITY, inner.wake.wait(st));
            }
        }
    }
}

/// Cheap, clonable submission side of a [`DelayedTimer`] — the seam route for
/// deferred (SDLY/ODLY/watchdog-style) hand-offs.
#[derive(Clone)]
pub struct TimerHandle {
    inner: Arc<Inner>,
}

impl TimerHandle {
    /// Schedule deferred *work* `cb` to be enqueued on `priority` after `delay`.
    /// Returns immediately — the callback runs on a pool worker no earlier than
    /// `delay` from now. Port of `callbackRequestDelayed` (`callback.c:410`).
    pub fn schedule(&self, delay: Duration, priority: CallbackPriority, cb: Callback) {
        self.inner.schedule(delay, TimerAction::Pool(priority), cb);
    }

    /// Schedule a **non-blocking wakeup** `cb` to run inline on the timer thread
    /// after `delay` — the `sleep`/`interval` waker path. `cb` MUST be trivial
    /// and non-blocking (only a waker `wake()`: an `unpark` or a tokio
    /// task-schedule); it runs on the single timer thread and delays every later
    /// deadline until it returns.
    ///
    /// This exists so a `spawn`ed future that awaits `sleep` is never blocked by
    /// its own wake: running it on the timer thread frees the band from the dual
    /// role of "run futures" AND "wake them". It is what closed the sleep-wake
    /// self-deadlock (`bug_pattern rtems-exec-sleep-wake-band-deadlock`), back
    /// when `future_exec` parked a worker per future for the future's whole
    /// life; that executor is now cooperative and holds a worker only across a
    /// single poll, but a wake still costs no worker here.
    pub fn schedule_wake(&self, delay: Duration, cb: Callback) {
        self.inner.schedule(delay, TimerAction::Inline, cb);
    }
}

/// The delayed-callback timer: one thread draining a deadline-ordered queue
/// into the callback executor pool. Port of the `timerQueue` half of
/// `callback.c`.
///
/// Dropping the timer stops and joins its thread.
pub struct DelayedTimer {
    inner: Arc<Inner>,
    worker: Option<JoinHandle<()>>,
}

impl DelayedTimer {
    /// Create a timer that fires callbacks into `sink` (the callback executor
    /// pool). Mirrors `callbackInit`'s single `epicsTimerQueueAllocate`
    /// (`callback.c:300`) whose `notify` routes to `callbackRequest`.
    pub fn new(sink: CallbackHandle) -> Self {
        let inner = Arc::new(Inner {
            state: Mutex::new(TimerState {
                heap: BinaryHeap::new(),
                next_seq: 0,
                shutdown: false,
            }),
            wake: Condvar::new(),
            sink,
        });
        let worker_inner = Arc::clone(&inner);
        // Every timed facility in this runtime — `sleep`, `interval`, scan
        // periods, `callbackRequestDelayed` — funnels through the loop below,
        // on this one thread. Losing it stops all of them at once while the IOC
        // goes on serving, so its loss is the one that must never be inferred
        // from work that quietly stops happening — neither after start-up
        // (`run_facility_loop`) nor at start-up (`MandatoryThread`).
        let worker = MandatoryThread::new(
            "cbTimer",
            // callback.c:300 — the delayed-callback queue is allocated with
            // `epicsTimerQueueAllocate(0, epicsThreadPriorityScanHigh)`. That
            // puts the timer above the Low (59) and Medium (64) bands it feeds
            // but just below High (71), matching C: a due deadline preempts
            // ordinary callback work, and a High callback still preempts the
            // timer. Best effort, and only when the RT switch is on
            // (`runtime::task::RT_PRIORITY_ENV`).
            ThreadPriority::ScanHigh,
            // C allocates the timer queue's thread with
            // `epicsThreadGetStackSize(epicsThreadStackMedium)`
            // (`libcom/src/timer/timerQueueActive.cpp:48`). This thread only
            // fires expirations onto the callback bands; the arbitrary work
            // runs on those, which are Big.
            crate::runtime::task::StackSizeClass::Medium,
        )
        .spawn(move || {
            run_facility_loop(
                FACILITY,
                || timer_loop(&worker_inner),
                || recover(FACILITY, worker_inner.state.lock()).shutdown = true,
            );
        });
        DelayedTimer {
            inner,
            worker: Some(worker),
        }
    }

    /// A cheap, clonable scheduling handle (see [`TimerHandle`]).
    pub fn handle(&self) -> TimerHandle {
        TimerHandle {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Schedule `cb` after `delay` — convenience wrapper over
    /// [`TimerHandle::schedule`] (pool-dispatched deferred work).
    pub fn schedule(&self, delay: Duration, priority: CallbackPriority, cb: Callback) {
        self.inner.schedule(delay, TimerAction::Pool(priority), cb);
    }
}

impl Drop for DelayedTimer {
    fn drop(&mut self) {
        {
            let mut st = recover(FACILITY, self.inner.state.lock());
            st.shutdown = true;
        }
        self.inner.wake.notify_all();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::background::callback_executor::CallbackPool;
    use std::sync::mpsc;

    const T: Duration = Duration::from_secs(5);

    #[test]
    fn delayed_callback_fires_no_earlier_than_delay() {
        let pool = CallbackPool::new();
        let timer = DelayedTimer::new(pool.handle());

        let delay = Duration::from_millis(80);
        let (tx, rx) = mpsc::channel();
        let start = Instant::now();
        timer.schedule(
            delay,
            CallbackPriority::High,
            Box::new(move || tx.send(Instant::now()).unwrap()),
        );

        let fired_at = rx.recv_timeout(T).unwrap();
        assert!(
            fired_at.duration_since(start) >= delay,
            "callback fired after {:?}, earlier than the {:?} delay",
            fired_at.duration_since(start),
            delay
        );
    }

    #[test]
    fn earlier_deadline_fires_before_later_one() {
        // Invariant: the queue is deadline-ordered, not insertion-ordered.
        let pool = CallbackPool::new();
        let timer = DelayedTimer::new(pool.handle());
        let (tx, rx) = mpsc::channel();

        // Schedule the *longer* delay first, then a shorter one.
        let tx_long = tx.clone();
        timer.schedule(
            Duration::from_millis(150),
            CallbackPriority::Medium,
            Box::new(move || tx_long.send("long").unwrap()),
        );
        timer.schedule(
            Duration::from_millis(30),
            CallbackPriority::Medium,
            Box::new(move || tx.send("short").unwrap()),
        );

        assert_eq!(rx.recv_timeout(T).unwrap(), "short");
        assert_eq!(rx.recv_timeout(T).unwrap(), "long");
    }

    /// Boundary: a wake that panics. It runs inline on the timer thread, so
    /// before this it took every later deadline in the IOC with it — sleep,
    /// interval, scan periods, `callbackRequestDelayed` — and said nothing.
    #[test]
    fn a_panicking_wake_does_not_stop_the_timer() {
        let pool = CallbackPool::new();
        let timer = DelayedTimer::new(pool.handle());
        let h = timer.handle();

        h.schedule_wake(
            Duration::from_millis(10),
            Box::new(|| panic!("a waker panicked on the timer thread")),
        );

        let (tx, rx) = mpsc::channel();
        timer.schedule(
            Duration::from_millis(40),
            CallbackPriority::High,
            Box::new(move || tx.send(()).unwrap()),
        );

        assert_eq!(
            rx.recv_timeout(T),
            Ok(()),
            "the deadline after a panicking wake never fired: the timer thread died with it"
        );
    }

    /// Boundary: the state mutex is poisoned. Every scheduling path in this
    /// file took it with `.unwrap()`, so one panic anywhere under the lock
    /// stopped all timed work.
    #[test]
    fn a_poisoned_state_still_schedules_and_fires() {
        let pool = CallbackPool::new();
        let timer = DelayedTimer::new(pool.handle());

        let inner = Arc::clone(&timer.inner);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = inner.state.lock().expect("first lock");
            panic!("poison the timer state");
        }));
        assert!(
            timer.inner.state.lock().is_err(),
            "the state mutex must actually be poisoned for this to test anything"
        );

        let (tx, rx) = mpsc::channel();
        timer.schedule(
            Duration::from_millis(10),
            CallbackPriority::High,
            Box::new(move || tx.send(()).unwrap()),
        );
        assert_eq!(
            rx.recv_timeout(T),
            Ok(()),
            "a poisoned state stopped the timer facility"
        );
    }

    #[test]
    fn schedule_after_shutdown_never_fires() {
        // Boundary: a TimerHandle that outlives the timer must drop late
        // requests silently — the callback must never reach the sink pool.
        let pool = CallbackPool::new();
        let timer = DelayedTimer::new(pool.handle());
        let h = timer.handle();
        drop(timer); // sets shutdown, joins the timer thread.

        let (tx, rx) = mpsc::channel::<()>();
        h.schedule(
            Duration::from_millis(0),
            CallbackPriority::High,
            Box::new(move || tx.send(()).unwrap()),
        );
        // The callback (and its `tx`) is dropped synchronously inside
        // schedule()'s shutdown branch — never scheduled, never fired. The
        // receiver therefore sees the sender gone (Disconnected), and never a
        // delivered `()`.
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(200)),
            Err(mpsc::RecvTimeoutError::Disconnected),
            "a delayed callback fired after shutdown; it must be dropped"
        );
    }
}
