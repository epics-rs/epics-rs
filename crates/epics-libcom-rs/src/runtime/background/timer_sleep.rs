//! Timer-backed `sleep` / `sleep_until` futures — the RTEMS backend for
//! [`crate::runtime::task::sleep`] / [`crate::runtime::task::sleep_until`]
//! (decision A2, increment W3b item 4).
//!
//! # Model
//!
//! A hosted build sleeps via `tokio::time::sleep`, whose waker is driven by the
//! tokio timer wheel. RTEMS has no such wheel, so a [`Sleep`] future arms a
//! one-shot entry on the [`DelayedTimer`](super::delayed_timer::DelayedTimer):
//! on its first poll it schedules a wakeup for its deadline; when that wakeup
//! fires it wakes the future's stored waker, and the next poll — now past the
//! deadline — returns `Ready`. This is the same deadline-ordered timer thread
//! that backs C `callbackRequestDelayed` (`callback.c:410-419`); a `Sleep` is
//! just that facility with the "callback" being "wake this future".
//!
//! # Why the wakeup runs on the timer thread, not the callback pool
//!
//! The wakeup is armed via [`TimerHandle::schedule_wake`], so it runs **inline
//! on the timer thread** rather than being dispatched to the callback pool.
//! Waking is a non-blocking `waker.wake()` (an `unpark` for a `park_on` driver,
//! a task re-enqueue for [`super::future_exec`], or a tokio task-schedule) and
//! needs no worker, so it does not take one. Routing the wake off the band keeps
//! the band's sole job "run futures" and makes the wake uniform for every
//! sleeper — bare `spawn`ed tails and periodic-scan `interval` alike.
//!
//! This is also what closed the sleep-wake self-deadlock (`bug_pattern
//! rtems-exec-sleep-wake-band-deadlock`): back when `future_exec` parked a pool
//! worker for a spawned future's whole life, a wake dispatched to the same
//! single-worker band sat behind the very worker it had to wake. That executor
//! is cooperative now and releases its worker at every suspension, so the wake
//! would no longer starve — but a wake that costs a worker is still the wrong
//! shape, and `Inline` remains the rule.
//!
//! # Lazy arming and drop-cancel
//!
//! The deadline is fixed when the [`Sleep`] is constructed (`now + dur` for
//! [`sleep`], the given instant for [`sleep_until`]), matching tokio, but the
//! timer entry is armed lazily on the **first poll** — a `Sleep` that is
//! created and dropped without ever being awaited schedules nothing.
//!
//! A [`Sleep`] **owns** the queue entry it arms: [`TimerHandle::schedule_wake`]
//! hands back a [`WakeKey`], and [`Sleep`]'s `Drop` both clears the stored waker
//! and cancels that key. Clearing the waker is what makes the cancel clean — a
//! wake that races the drop finds no waker and wakes nobody — and cancelling the
//! key is what makes it *free*: the entry holds a clone of the shared
//! `Arc<Mutex<SleepState>>`, so leaving it queued keeps that cell and the OS
//! mutex inside it alive for the entire remaining delay.
//!
//! That retention is not theoretical and not small. A `select!` arm holding a
//! long-period `interval` tick re-arms a fresh `Sleep` on every loop iteration
//! and drops it when another arm wins, so an uncancellable entry accumulates at
//! the loop's iteration rate for the whole period. Measured on VxWorks 7 against
//! the PVA search engine's 180 s `BEACON_CLEAN_INTERVAL` tick: ~124 live entries
//! at ~184 B each, released in one batch every 180 s.
//!
//! Cancellation is a property of the *wake* path only.
//! [`TimerHandle::schedule`] — C `callbackRequestDelayed`
//! (`callback.c:410-419`) — stays fire-and-forget, because there the caller
//! keeps no handle and the queue is the only owner.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use super::delayed_timer::{TimerHandle, WakeKey};

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
    /// Whether the first poll has run. Arming is lazy and attempted exactly
    /// once; a timer already shut down when that poll ran queues nothing, and
    /// this is what stops every later poll from retrying.
    armed: bool,
    /// The queue entry this `Sleep` owns — `Some` exactly while one is queued,
    /// and the thing `Drop` gives back. Separate from `armed` so neither field
    /// has to mean two things: "we tried" and "we hold one" are different
    /// facts, and it is the second that governs the memory.
    entry: Option<WakeKey>,
}

/// A future completing `dur` from now — mirrors `tokio::time::sleep`. The
/// deadline is fixed at construction; the timer entry arms on first poll.
pub fn sleep(timer: &TimerHandle, dur: Duration) -> Sleep {
    sleep_until(timer, crate::runtime::time::deadline_from_now(dur))
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
        entry: None,
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
            // Inline wakeup on the timer thread — see the module docs: a sleep
            // wake is a non-blocking `waker.wake()`, and dispatching it to the
            // callback pool would deadlock a `spawn`ed future that awaits it.
            this.entry = this.timer.schedule_wake(
                delay,
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
        // Give the queue entry back first. It holds a clone of `state`, so
        // until it goes the shared cell — and the OS mutex std lazily creates
        // inside it — stays alive for the whole remaining delay. Cancelling an
        // entry that already fired is a no-op, so no ordering is owed here.
        if let Some(key) = self.entry.take() {
            self.timer.cancel_wake(key);
        }
        // Clear the waker so a wake that raced the cancel finds nobody. Leaving
        // `fired` untouched is fine — the future is gone.
        self.state.lock().unwrap().waker = None;
    }
}

/// A periodic ticker over the delayed-callback timer — the RTEMS backend for
/// [`crate::runtime::task::interval`]. Mirrors `tokio::time::Interval` with its
/// default `MissedTickBehavior::Burst`: the first tick is immediate and tick
/// deadlines are anchored at construction (`start + period`, `start + 2·period`,
/// …), so an overdue tick fires immediately and successive overdue ticks burst
/// back-to-back until the schedule is caught up.
pub struct TimerInterval {
    timer: TimerHandle,
    period: Duration,
    /// Next tick deadline, anchored at construction so catch-up is Burst.
    next: Instant,
    /// The first tick completes immediately (tokio parity).
    first: bool,
}

/// Build a periodic ticker firing every `period`, backed by `timer` — the
/// runtime-free mirror of `tokio::time::interval`.
pub fn interval(timer: &TimerHandle, period: Duration) -> TimerInterval {
    TimerInterval {
        timer: timer.clone(),
        period,
        next: crate::runtime::time::deadline_from_now(period),
        first: true,
    }
}

impl TimerInterval {
    /// Complete at the next tick. The first tick is immediate; thereafter each
    /// tick waits until its (construction-anchored) deadline, with Burst
    /// catch-up when the caller has fallen behind.
    pub async fn tick(&mut self) {
        if self.first {
            self.first = false;
            return;
        }
        sleep_until(&self.timer, self.next).await;
        // Advance by a whole period from the previous deadline (not from now),
        // so overdue deadlines stay in the past and the next tick bursts.
        self.next = crate::runtime::time::deadline_after(self.next, self.period);
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

    /// A delay past `Duration`'s representable range must never fire
    /// rather than unwind the task. `duration_from_secs` maps `+inf`,
    /// `NaN` and `1e300` to `Duration::MAX` — a record `HIGH` field is
    /// network-settable — and `Instant + Duration::MAX` panics, where
    /// `tokio::time::sleep` saturates to `far_future()`. The two
    /// backends disagreeing is the defect, so this pins the exec side to
    /// tokio's answer.
    #[test]
    fn an_unrepresentable_delay_never_fires_instead_of_panicking() {
        let pool = CallbackPool::new();
        let timer = DelayedTimer::new(pool.handle());
        let count = Arc::new(AtomicUsize::new(0));
        let waker = Waker::from(Arc::new(CountWaker(Arc::clone(&count))));
        let mut cx = Context::from_waker(&waker);

        let mut s = Box::pin(sleep(&timer.handle(), Duration::MAX));
        assert!(s.as_mut().poll(&mut cx).is_pending());
        drop(s);
        // The ticker anchors its first deadline the same way, and
        // advances it with the same owner.
        let mut every = interval(&timer.handle(), Duration::MAX);
        every.next = crate::runtime::time::deadline_after(every.next, every.period);
        assert_eq!(count.load(Ordering::SeqCst), 0);
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

    /// The E10 regression: a dropped `Sleep` must give its queue entry back,
    /// not leave it to expire. Before the entry was owned, the ~184 B a `Sleep`
    /// allocates (shared cell, boxed wake, and the OS mutex std lazily creates
    /// inside the cell) stayed live for the whole remaining delay — so a
    /// `select!` arm re-arming a long-period tick each iteration accumulated
    /// one of those per iteration until the period elapsed.
    #[test]
    fn dropping_a_sleep_releases_its_timer_entry() {
        let pool = CallbackPool::new();
        let timer = DelayedTimer::new(pool.handle());
        let h = timer.handle();

        let waker = Waker::from(Arc::new(CountWaker(Arc::new(AtomicUsize::new(0)))));
        let mut cx = Context::from_waker(&waker);

        // An hour out, so only the drop can retire it.
        let mut s = Box::pin(sleep(&h, Duration::from_secs(3600)));
        assert!(s.as_mut().poll(&mut cx).is_pending());
        assert_eq!(h.scheduled_count(), 1, "the first poll must arm an entry");

        drop(s);
        assert_eq!(
            h.scheduled_count(),
            0,
            "a dropped sleep left its entry queued; it holds the shared cell for an hour"
        );
    }

    /// A `Sleep` created and never polled arms nothing, so it has nothing to
    /// give back — the lazy-arming half of the same invariant.
    #[test]
    fn dropping_an_unpolled_sleep_queues_nothing() {
        let pool = CallbackPool::new();
        let timer = DelayedTimer::new(pool.handle());
        let h = timer.handle();

        drop(sleep(&h, Duration::from_secs(3600)));
        assert_eq!(h.scheduled_count(), 0);
    }

    /// The interval case the leak was actually measured through: each `tick()`
    /// that loses a `select!` race drops mid-await, and every one of those must
    /// leave the queue as it found it.
    #[test]
    fn abandoned_interval_ticks_leave_no_entries() {
        let pool = CallbackPool::new();
        let timer = DelayedTimer::new(pool.handle());
        let h = timer.handle();

        let waker = Waker::from(Arc::new(CountWaker(Arc::new(AtomicUsize::new(0)))));
        let mut cx = Context::from_waker(&waker);

        let mut iv = interval(&h, Duration::from_secs(180));
        // The first tick is immediate and arms nothing; the rest are 180 s out.
        let mut first = Box::pin(iv.tick());
        assert!(first.as_mut().poll(&mut cx).is_ready());
        drop(first);

        for _ in 0..32 {
            let mut t = Box::pin(iv.tick());
            assert!(t.as_mut().poll(&mut cx).is_pending());
            drop(t); // the `select!` arm lost
        }
        assert_eq!(
            h.scheduled_count(),
            0,
            "abandoned interval ticks accumulate one queue entry each per period"
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

    #[test]
    fn interval_first_tick_immediate_then_periodic() {
        let pool = CallbackPool::new();
        let timer = DelayedTimer::new(pool.handle());
        let period = Duration::from_millis(40);
        let th = timer.handle();
        let start = Instant::now();
        drive(
            async move {
                let mut iv = interval(&th, period);
                iv.tick().await; // first tick: immediate
                let after_first = start.elapsed();
                assert!(
                    after_first < period,
                    "first tick should be immediate, was {after_first:?}"
                );
                iv.tick().await; // ~1 period in
                iv.tick().await; // ~2 periods in
            },
            || false,
        )
        .unwrap();
        assert!(
            start.elapsed() >= 2 * period,
            "two periodic ticks should take at least two periods, took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn interval_bursts_to_catch_up_after_a_stall() {
        // MissedTickBehavior::Burst: after stalling past several deadlines, the
        // overdue ticks fire back-to-back rather than re-spacing from now.
        let pool = CallbackPool::new();
        let timer = DelayedTimer::new(pool.handle());
        let period = Duration::from_millis(30);
        let th = timer.handle();
        drive(
            async move {
                let mut iv = interval(&th, period);
                iv.tick().await; // immediate; deadlines land at 30/60/90/120ms
                // Stall well past four deadlines.
                sleep(&th, Duration::from_millis(140)).await;
                let t = Instant::now();
                iv.tick().await; // deadline 30ms already passed -> immediate
                iv.tick().await; // deadline 60ms passed -> immediate
                iv.tick().await; // deadline 90ms passed -> immediate
                assert!(
                    t.elapsed() < period,
                    "overdue ticks must burst, three took {:?}",
                    t.elapsed()
                );
            },
            || false,
        )
        .unwrap();
    }
}
