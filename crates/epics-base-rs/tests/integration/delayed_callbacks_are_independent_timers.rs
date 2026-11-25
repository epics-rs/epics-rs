//! A record's [`DelayedCallback`]s are separate C `CALLBACK`s.
//!
//! `callbackRequestDelayed` restarts the timer of the one `CALLBACK` it is
//! given, and `epicsTimerCancel(pcb->timer)` stops that one
//! (`scalerRecord.c:645`, `:660`); a record's other callbacks stay pending.
//! Scaler's update, delay and autocount callbacks (`:269-294`) depend on it.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::record::*;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// A display refresh that re-arms itself, like scaler's update callback.
const REFRESH: DelayedCallback = DelayedCallback::new(0);
/// A one-shot, like scaler's delay callback.
const START: DelayedCallback = DelayedCallback::new(1);

static FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Long, false)];

/// Each `process()` emits the actions the test queued; each fire counts.
struct TwoTimers {
    next: Arc<Mutex<Vec<ProcessAction>>>,
    refreshes: Arc<AtomicU32>,
    starts: Arc<AtomicU32>,
}

impl Record for TwoTimers {
    fn record_type(&self) -> &'static str {
        "twotimers"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome {
            result: RecordProcessResult::Complete,
            actions: std::mem::take(&mut *self.next.lock().unwrap()),
            device_did_compute: false,
            status: Ok(()),
        })
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        (name == "VAL").then_some(EpicsValue::Long(0))
    }
    fn put_field(&mut self, name: &str, _value: EpicsValue) -> CaResult<()> {
        Err(CaError::FieldNotFound(name.to_string()))
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }
    fn delayed_callback_fire(
        &mut self,
        callback: DelayedCallback,
        _pact: bool,
    ) -> DelayedCallbackOutcome {
        if callback == REFRESH {
            self.refreshes.fetch_add(1, Ordering::Relaxed);
            DelayedCallbackOutcome::Rearm(Duration::from_millis(10))
        } else {
            self.starts.fetch_add(1, Ordering::Relaxed);
            DelayedCallbackOutcome::Drop
        }
    }
}

struct Rig {
    db: PvDatabase,
    next: Arc<Mutex<Vec<ProcessAction>>>,
    refreshes: Arc<AtomicU32>,
    starts: Arc<AtomicU32>,
}

impl Rig {
    async fn new() -> Self {
        let db = PvDatabase::new();
        let next = Arc::new(Mutex::new(Vec::new()));
        let refreshes = Arc::new(AtomicU32::new(0));
        let starts = Arc::new(AtomicU32::new(0));
        db.add_record(
            "TT",
            Box::new(TwoTimers {
                next: next.clone(),
                refreshes: refreshes.clone(),
                starts: starts.clone(),
            }),
        )
        .await
        .unwrap();
        Self {
            db,
            next,
            refreshes,
            starts,
        }
    }

    async fn process(&self, actions: Vec<ProcessAction>) {
        *self.next.lock().unwrap() = actions;
        self.db
            .process_record_with_links("TT", &mut ProcStack::new())
            .await
            .unwrap();
    }

    /// Waits for `n` more refreshes. Not a count over a fixed window: macOS
    /// coalesces the timer thread's 10 ms wait up to ~80 ms late (C's
    /// `pthread_cond_timedwait` alike), while a tokio window is not stretched.
    async fn refreshes(&self, n: u32, msg: &str) {
        let target = self.refreshes.load(Ordering::Relaxed) + n;
        until(|| self.refreshes.load(Ordering::Relaxed) >= target, msg).await;
    }
}

/// Polls `done` until it holds, failing with `msg` at a deadline no timer
/// coalescing reaches.
async fn until(done: impl Fn() -> bool, msg: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "{msg}");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
}

fn arm(ms: u64, callback: DelayedCallback) -> ProcessAction {
    ProcessAction::DelayedCallbackAfter(Duration::from_millis(ms), callback)
}

#[epics_macros_rs::epics_test]
async fn arming_one_callback_leaves_another_pending() {
    let rig = Rig::new().await;
    rig.process(vec![arm(10, REFRESH)]).await;
    rig.refreshes(1, "the refresh runs").await;
    let armed = Instant::now();
    rig.process(vec![arm(5000, START)]).await;
    rig.refreshes(5, "the START arm left the refresh running")
        .await;
    // No coalescing stretches five 10 ms refreshes to START's 5 s; a timer
    // shared with START defers them to it.
    assert!(
        armed.elapsed() < Duration::from_secs(5),
        "the refreshes waited for START's deadline"
    );
}

#[epics_macros_rs::epics_test]
async fn cancelling_one_callback_leaves_another_pending() {
    let rig = Rig::new().await;
    rig.process(vec![arm(10, REFRESH), arm(100, START)]).await;
    rig.process(vec![ProcessAction::CancelDelayedCallback(START)])
        .await;
    // Each refresh is due 10 ms after the last fired, so the 15th is due past
    // START's 100 ms on the one deadline-ordered timer queue: a START the
    // cancel missed has fired before it.
    rig.refreshes(15, "cancelling START left the refresh running")
        .await;
    assert_eq!(rig.starts.load(Ordering::Relaxed), 0, "START was cancelled");
}

#[epics_macros_rs::epics_test]
async fn rearming_a_callback_restarts_only_its_own_timer() {
    let rig = Rig::new().await;
    rig.process(vec![arm(500, START)]).await;
    // A first timer that survived the re-arm is due before the probe; the
    // re-arm is due 400 ms after it, which no late wake makes due together.
    rig.process(vec![arm(600, REFRESH), arm(1000, START)]).await;
    rig.refreshes(1, "the probe fires").await;
    assert_eq!(
        rig.starts.load(Ordering::Relaxed),
        0,
        "the re-arm replaced the first timer"
    );
    until(
        || rig.starts.load(Ordering::Relaxed) > 0,
        "the re-armed START fires",
    )
    .await;
    rig.refreshes(2, "the probe keeps running").await;
    assert_eq!(rig.starts.load(Ordering::Relaxed), 1);
}
