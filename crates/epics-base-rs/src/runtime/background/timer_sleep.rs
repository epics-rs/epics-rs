//! Timer-backed `sleep` / `sleep_until` futures — the RTEMS backend for
//! [`crate::runtime::task::sleep`] / [`sleep_until`] (decision A2, increment
//! W3b item 4).
//!
//! [`crate::runtime::task::sleep`]:
//! # Model
//!
//! A hosted build sleeps via `tokio::time::sleep`, whose waker is driven by the
//! tokio timer wheel. RTEMS has no such wheel, so a [`Sleep`] future arms a
//! one-shot entry on the [`DelayedTimer`](super::delayed_timer::DelayedTimer):
//! on its first poll it schedules a callback for its deadline; when that
//! callback fires (on a callback-pool worker) it wakes the future's stored
//! waker, and the next poll — now past the deadline — returns `Ready`. This is
//! the same deadline-ordered timer thread that backs C `callbackRequestDelayed`
//! (`callback.c:410-419`); a `Sleep` is just that facility with the "callback"
//! being "wake this future".
//!
//! # Lazy arming and drop-cancel
//!
//! The deadline is fixed when the [`Sleep`] is constructed (`now + dur` for
//! [`sleep`], the given instant for [`sleep_until`]), matching tokio, but the
//! timer entry is armed lazily on the **first poll** — a `Sleep` that is
//! created and dropped without ever being awaited schedules nothing.
//!
//! The [`DelayedTimer`] has no cancel handle (C `callbackRequestDelayed` starts
//! a fire-and-forget `epicsTimer`), so a dropped-before-deadline `Sleep` cannot
//! un-schedule its entry. Instead [`Sleep`]'s `Drop` clears the stored waker, so
//! when the orphaned timer callback eventually fires it finds no waker and wakes
//! nobody — a clean cancel with no stale wakeup. The one wasted timer tick is
//! the cost of the runtime-free design and matches how C leaves an already-armed
//! `epicsTimer` to expire harmlessly.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use super::callback_executor::CallbackPriority;
use super::delayed_timer::TimerHandle;

/// Band the timer wakeup runs on. A sleep wakeup only calls `waker.wake()`, so
/// it rides the middle band (C `priorityMedium`, `callback.h:42`) like any
/// other general deferred tail.
const SLEEP_WAKE_PRIORITY: CallbackPriority = CallbackPriority::Medium;

/// Shared between a [`Sleep`] and its armed timer callback.
struct SleepState {
    /// Set by the timer callback once the deadline has fired.
    fired: bool,
    /// Waker of the task awaiting the [`Sleep`]. Cleared on drop so an orphaned
    /// timer callback wakes nobody.
    waker: Option<Waker>,
}

/// A future that completes at a fixed deadline, driven by the delayed-callback
/// timer — the RTEMS-side mirror of `tokio::time::Sleep`.
pub struct Sleep {
    deadline: Instant,
    timer: TimerHandle,
    state: Arc<Mutex<SleepState>>,
    /// Whether the timer entry has been armed (arming is lazy, on first poll).
    armed: bool,
}

/// A future completing `dur` from now — mirrors `tokio::time::sleep`. The
/// deadline is fixed at construction; the timer entry arms on first poll.
pub fn sleep(timer: &TimerHandle, dur: Duration) -> Sleep {
    sleep_until(timer, Instant::now() + dur)
}

/// A future completing at `deadline` — mirrors `tokio::time::sleep_until`. A
/// deadline already in the past makes the future ready on its first poll
/// without arming a timer entry.
pub fn sleep_until(timer: &TimerHandle, deadline: Instant) -> Sleep {
    Sleep {
        deadline,
        timer: timer.clone(),
        state: Arc::new(Mutex::new(SleepState {
            fired: false,
            waker: None,
        })),
        armed: false,
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // `Sleep` holds no self-referential state, so it is `Unpin` and we can
        // take a plain `&mut` to it.
        let this = self.get_mut();

        {
            let mut st = this.state.lock().unwrap();
            if st.fired {
                return Poll::Ready(());
            }
            // Deadline already reached (past-deadline construction, or the
            // clock crossed it before the timer callback landed): complete now.
            if Instant::now() >= this.deadline {
                st.fired = true;
                return Poll::Ready(());
            }
            st.waker = Some(cx.waker().clone());
        }

        if !this.armed {
            let delay = this.deadline.saturating_duration_since(Instant::now());
            let cb_state = Arc::clone(&this.state);
            this.timer.schedule(
                delay,
                SLEEP_WAKE_PRIORITY,
                Box::new(move || {
                    let mut st = cb_state.lock().unwrap();
                    st.fired = true;
                    if let Some(w) = st.waker.take() {
                        w.wake();
                    }
                }),
            );
            this.armed = true;
        }
        Poll::Pending
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        // Clear the waker so an already-armed (uncancellable) timer callback
        // wakes nobody when it fires. Leaving `fired` untouched is fine — the
        // future is gone.
        self.state.lock().unwrap().waker = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::background::callback_executor::CallbackPool;
    use crate::runtime::background::delayed_timer::DelayedTimer;
    use crate::runtime::task::park_on_interruptible as drive;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::task::Wake;

    const T: Duration = Duration::from_secs(5);

    /// A waker that counts how often it is woken — lets a test prove a dropped
    /// sleep's orphaned timer callback wakes nobody.
    struct CountWaker(Arc<AtomicUsize>);
    impl Wake for CountWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn sleep_completes_no_earlier_than_delay() {
        let pool = CallbackPool::new();
        let timer = DelayedTimer::new(pool.handle());
        let delay = Duration::from_millis(60);
        let start = Instant::now();
        // Real path: park-driver polls, parks, the timer callback wakes it
        // cross-thread, the next poll returns Ready.
        drive(sleep(&timer.handle(), delay), || false).unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed >= delay,
            "sleep returned after {elapsed:?}, earlier than the {delay:?} delay"
        );
    }

    #[test]
    fn sleep_until_past_deadline_is_immediately_ready() {
        let pool = CallbackPool::new();
        let timer = DelayedTimer::new(pool.handle());
        let past = Instant::now() - Duration::from_secs(1);

        let count = Arc::new(AtomicUsize::new(0));
        let waker = Waker::from(Arc::new(CountWaker(Arc::clone(&count))));
        let mut cx = Context::from_waker(&waker);

        let mut s = Box::pin(sleep_until(&timer.handle(), past));
        assert!(s.as_mut().poll(&mut cx).is_ready());
        // Nothing was armed, so nothing ever wakes the waker.
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn drop_before_deadline_wakes_nobody() {
        let pool = CallbackPool::new();
        let timer = DelayedTimer::new(pool.handle());

        let count = Arc::new(AtomicUsize::new(0));
        let waker = Waker::from(Arc::new(CountWaker(Arc::clone(&count))));
        let mut cx = Context::from_waker(&waker);

        let mut s = Box::pin(sleep(&timer.handle(), Duration::from_millis(60)));
        // First poll arms the timer entry and registers the CountWaker.
        assert!(s.as_mut().poll(&mut cx).is_pending());
        drop(s); // clears the registered waker

        // Wait well past the deadline: the orphaned timer callback fires but
        // must find no waker and wake nobody.
        std::thread::sleep(Duration::from_millis(140));
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "a dropped sleep must not wake a stale waker"
        );
    }

    #[test]
    fn concurrent_sleepers_complete_in_deadline_order() {
        // The future layer must not serialize sleepers: a later deadline must
        // not hold back an earlier one.
        let pool = CallbackPool::new();
        let timer = DelayedTimer::new(pool.handle());
        let (tx, rx) = mpsc::channel();

        let th_long = timer.handle();
        let tx_long = tx.clone();
        let long = std::thread::spawn(move || {
            drive(sleep(&th_long, Duration::from_millis(150)), || false).unwrap();
            tx_long.send("long").unwrap();
        });
        let th_short = timer.handle();
        let short = std::thread::spawn(move || {
            drive(sleep(&th_short, Duration::from_millis(30)), || false).unwrap();
            tx.send("short").unwrap();
        });

        assert_eq!(rx.recv_timeout(T).unwrap(), "short");
        assert_eq!(rx.recv_timeout(T).unwrap(), "long");
        long.join().unwrap();
        short.join().unwrap();
    }
}
