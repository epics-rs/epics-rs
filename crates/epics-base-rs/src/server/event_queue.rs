//! The CA server monitor event queue — a port of C `dbEvent.c`'s
//! `event_user` / `event_que` / `evSubscrip` triple.
//!
//! # Why this exists as its own primitive
//!
//! The port used to model a monitor as a bounded `mpsc` channel plus a side
//! "coalesce slot" holding the newest event once the channel filled. That
//! shape cannot express C's queue, and three review rounds (R8-21, R8-22,
//! R8-23) found separate divergences that all trace back to it:
//!
//! * the slot's value means "newer than everything queued", so a consumer that
//!   finds it set has to discard the whole backlog — C instead replaces the
//!   monitor's *last* queued entry in place and keeps the earlier distinct
//!   ones (R8-22);
//! * the queue was per-subscription, so C's `nDuplicates` — a field of the
//!   *queue*, shared by every subscription attached to it — had no home, and
//!   the EVENTS_OFF drain gate was evaluated per subscription (R8-23).
//!
//! So the primitive here is C's, not a patched channel:
//!
//! * [`EventUser`] == C `event_user`: one per CA circuit (C: one per
//!   `db_init_events` client). Owns `flowCtrlMode` (EVENTS_OFF) and the chain
//!   of queues.
//! * [`EvQue`] == C `event_que`: a ring of `EVENTQUESIZE` entries shared by up
//!   to `EVENTQUESIZE/EVENTENTRIES - 1` subscriptions (the `quota` rule,
//!   `dbEvent.c:446-469`), carrying the queue-level `nDuplicates`.
//! * `SubQ` == C `evSubscrip`: `npend` (queued entry count), `nreplace`, and
//!   `pLastLog` — the entry a post *replaces* rather than appending.
//!
//! # Invariants (all enforced by [`EvQue`], the single owner)
//!
//! * MUST: `n_duplicates == Σ over subscriptions of max(0, npend - 1)`. Only
//!   the append branch of [`EventSink::post`] raises it and only
//!   `QueInner::remove_front` lowers it — C `db_queue_event_log`
//!   (`dbEvent.c:836-840`) and `event_remove` (`dbEvent.c:542-558`).
//! * MUST: `total_pending == Σ npend`, and `total_pending < size` — the `quota`
//!   reservation is what guarantees the ring can never fill: past the replace
//!   threshold each attached subscription can add at most one more entry, and
//!   attachment is capped so that always fits (C asserts the free slot at
//!   `dbEvent.c:833`).
//! * MUST NOT: any pending event live outside `SubQ::events`. There is no side
//!   slot; a monitor's newest event is `events.back()` — C's `*pLastLog` —
//!   whether it got there by append or by in-place replace.
//!
//! # The two decisions, each in exactly one place
//!
//! * **Append or replace** — [`EventSink::post`], C `db_queue_event_log`
//!   (`dbEvent.c:806-852`): with an entry already queued for this monitor AND
//!   (`flowCtrlMode` OR the ring within `EVENTSPERQUE` of full), overwrite
//!   `*pLastLog` in place; otherwise append and grow the ring.
//! * **Drain or suspend** — [`EventReader::recv`] / [`EventReader::try_recv`],
//!   C `event_read` (`dbEvent.c:940-952`): suspend only while `flowCtrlMode &&
//!   nDuplicates == 0`; otherwise drain the queue to empty. Once a drain pass
//!   starts it runs to `EVENTQEMPTY` without re-checking the gate (C's `while
//!   (evque[getix] != EVENTQEMPTY)` loop), which is what `draining` tracks.
//!
//! Those two are the ONLY ways in and out of the queue: [`EvQue`] hands out no
//! mutating method of its own, so no caller can enqueue past the replace rule
//! or dequeue past the EVENTS_OFF gate.
//!
//! # Documented deviations from C
//!
//! * C's ring is drained in strict ring order by one event task per
//!   `event_user`; here each subscription has its own reader task (the CA
//!   server frames every subscription separately), so entries are taken in
//!   per-subscription FIFO order and the *interleaving* of two subscriptions on
//!   one circuit is not fixed. Every quantity C's queue behaviour depends on —
//!   ring occupancy, `nDuplicates`, `quota`, per-monitor `npend`/`pLastLog` —
//!   is shared and accounted exactly as in C.
//! * C's `db_queue_event_log` early-drops a post when both the queued and the
//!   incoming field log are by-reference with no copy (`dbEvent.c:794-800`).
//!   Every event here carries an owned `Snapshot` (C `dbfl_has_copy` is always
//!   true), so that branch cannot be reached.
//! * When a post replaces `*pLastLog`, C frees the displaced field log and with
//!   it its `mask`. The port ORs the displaced mask into the survivor so a
//!   class-narrowing consumer still learns which `DBE_*` classes changed since
//!   its last delivery (pre-existing deliberate deviation, 446e0d4a); the
//!   surviving *value* is C's.

// RTEMS-EXEC-MODEL-ALLOW(12): checked - these run and pass in the feature-ON suite.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::runtime::sync::{Arc, Notify};
use crate::server::pv::MonitorEvent;

/// C `EVENTENTRIES` (`dbEvent.c:62`) — ring entries reserved per attached
/// subscription.
pub const EVENT_ENTRIES: usize = 4;

/// C `EVENTSPERQUE` (`dbEvent.c:61`) — the ring-space threshold at or below
/// which a post replaces the monitor's last entry instead of appending, sized
/// by C from the Ethernet MTU. Operators scale the queue with
/// `EPICS_CAS_MAX_EVENTS_PER_CHAN`; the default is C's 36, giving C's 144-entry
/// ring.
pub fn events_per_que() -> usize {
    crate::runtime::env::get("EPICS_CAS_MAX_EVENTS_PER_CHAN")
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n >= EVENT_ENTRIES)
        .unwrap_or(36)
}

/// C `EVENTQUESIZE` (`dbEvent.c:63`) — total ring entries in one queue.
pub fn event_que_size() -> usize {
    EVENT_ENTRIES * events_per_que()
}

/// C `event_user::flowCtrlMode` (`dbEvent.c:101`) — the circuit-wide EVENTS_OFF
/// flag. Held behind its own `Arc` so an [`EvQue`] can read it without a
/// back-pointer to the [`EventUser`] that owns the chain.
#[derive(Default, Debug)]
struct FlowCtrl {
    on: AtomicBool,
}

impl FlowCtrl {
    fn is_on(&self) -> bool {
        self.on.load(Ordering::Acquire)
    }
}

/// What [`EventSink::post`] did, for the caller to account for. The queue
/// applies every change to its own state itself (it is the single owner); this
/// reports the part that lives outside it — the dropped-monitor-event counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostOutcome {
    /// C append branch (`dbEvent.c:832-852`): the event took a new ring entry.
    /// `first_event` mirrors C's `firstEventFlag` — the ring was empty before
    /// this post.
    Appended { first_event: bool },
    /// C replace branch (`dbEvent.c:812-827`): the monitor already had an entry
    /// queued and the queue is in flow control or within `EVENTSPERQUE` of full,
    /// so `*pLastLog` was overwritten. The displaced value is never delivered —
    /// one lost monitor event (C `nreplace`).
    Replaced,
    /// The subscription is gone (reader dropped, or never attached). Nothing was
    /// queued — the `mpsc::Sender::try_send`-on-closed-channel case.
    Closed,
}

/// C `evSubscrip` — one subscription's state inside a queue.
struct SubQ {
    /// C `npend` is `events.len()`; C `*pLastLog` is `events.back_mut()`. Every
    /// pending event of this monitor lives here and nowhere else.
    events: VecDeque<MonitorEvent>,
    /// C `nreplace` — posts that overwrote `*pLastLog`.
    nreplace: u64,
    /// The producer row (`EventSink`) is gone: no further posts can arrive. The
    /// reader still drains what is queued and then sees end-of-stream, exactly
    /// as an `mpsc::Receiver` does when the last `Sender` drops.
    producer_gone: bool,
    /// The reader (`EventReader`) is gone: posts are refused from here on.
    reader_gone: bool,
}

impl SubQ {
    fn new() -> Self {
        Self {
            events: VecDeque::new(),
            nreplace: 0,
            producer_gone: false,
            reader_gone: false,
        }
    }
}

/// The queue's mutable state. Everything here moves under one lock — C's
/// `LOCKEVQUE`.
struct QueInner {
    /// Occupied ring entries, `Σ npend`. C derives this from `putix`/`getix`
    /// (`ringSpace`); the port counts it because entries are taken in
    /// per-subscription order (see the module's documented deviations).
    total_pending: usize,
    /// C `event_que::nDuplicates` (`dbEvent.c:80`) — entries queued beyond the
    /// first for their monitor, summed over every subscription attached HERE.
    /// This is what makes a duplicate on subscription B unblock the drain of
    /// subscription A's entry under EVENTS_OFF.
    n_duplicates: usize,
    /// C `event_read`'s drain loop is in flight: it empties the queue in one
    /// pass and does not re-consult `flowCtrlMode` per entry.
    draining: bool,
    /// Wakers registered by [`EvQue::poll_next`] callers parked on this
    /// queue. The poll-based twin of the `Notify` waiter list: a consumer
    /// that multiplexes MANY subscriptions from ONE task (the QSRV group
    /// drain) parks here instead of holding a `Notified` future per queue.
    /// Flushed — woken and cleared — by [`EvQue::wake_readers`], the single
    /// owner of reader wakeup, on every transition that can make a parked
    /// read Ready.
    poll_wakers: Vec<std::task::Waker>,
    subs: HashMap<u32, SubQ>,
    /// C `event_que::quota` (`dbEvent.c:79`) — ring entries reserved by the
    /// subscriptions attached here, `EVENT_ENTRIES` each. A subscription may
    /// attach while `quota < size - EVENT_ENTRIES` (`dbEvent.c:453`); this is
    /// what caps a queue at `size / EVENT_ENTRIES - 1` monitors and guarantees
    /// the ring cannot fill.
    quota: usize,
    size: usize,
    replace_threshold: usize,
}

impl QueInner {
    /// C `ringSpace` (`dbEvent.c:136-147`) — unused ring entries.
    fn ring_space(&self) -> usize {
        self.size - self.total_pending
    }

    /// C `event_remove` (`dbEvent.c:542-558`): take this monitor's oldest entry
    /// and keep `nDuplicates` / occupancy symmetric. The ONLY removal path —
    /// the reader, subscription teardown, and queue detach all go through it, so
    /// no caller can move one counter without the other.
    fn remove_front(&mut self, sid: u32) -> Option<MonitorEvent> {
        let sub = self.subs.get_mut(&sid)?;
        let event = sub.events.pop_front()?;
        // C: `npend == 1` ⇒ the monitor has no last log any more; otherwise the
        // entry just removed was one of its duplicates.
        if !sub.events.is_empty() {
            debug_assert!(self.n_duplicates >= 1, "nDuplicates underflow");
            self.n_duplicates -= 1;
        }
        self.total_pending -= 1;
        // C's drain loop ends at `EVENTQEMPTY`.
        self.draining = self.total_pending > 0;
        Some(event)
    }

    /// Drop every entry a departing subscription still holds, then release its
    /// quota. Symmetric by construction: it drains through `remove_front`
    /// rather than zeroing counters, so `n_duplicates` / `total_pending` cannot
    /// drift. C does the same per entry in `event_remove`, and releases the
    /// quota once the cancelled subscription has drained (`dbEvent.c:999-1002`).
    fn detach(&mut self, sid: u32) {
        while self.remove_front(sid).is_some() {}
        if self.subs.remove(&sid).is_some() {
            self.quota -= EVENT_ENTRIES;
        }
    }

    /// C `db_add_event`'s attach test (`dbEvent.c:451-457`): take this queue if
    /// it still has room for one more monitor's reservation.
    fn try_attach(&mut self, sid: u32) -> bool {
        if self.quota >= self.size - EVENT_ENTRIES {
            return false;
        }
        self.quota += EVENT_ENTRIES;
        self.subs.insert(sid, SubQ::new());
        true
    }

    /// May a reader take an entry right now? C `event_read`'s gate
    /// (`dbEvent.c:947`), plus the in-pass stickiness of its drain loop.
    fn may_drain(&self, flow_ctrl_on: bool) -> bool {
        self.draining || !flow_ctrl_on || self.n_duplicates > 0
    }
}

/// C `event_que` (`dbEvent.c:69-82`) — one ring, shared by every subscription
/// attached to it.
///
/// Exposes no mutating method: events enter through [`EventSink::post`] and
/// leave through [`EventReader::recv`] / [`EventReader::try_recv`], which are
/// the sole owners of C's two decisions. The accessors below are read-only
/// views of C's counters for diagnostics and tests.
pub struct EvQue {
    flow: Arc<FlowCtrl>,
    inner: Mutex<QueInner>,
    /// Broadcast wakeup: a post, a flow-control change, or a producer/reader
    /// teardown re-arms every reader on this queue. C signals the one
    /// `ppendsem` per `event_user` for the same reason.
    wake: Notify,
}

impl EvQue {
    fn new(flow: Arc<FlowCtrl>) -> Self {
        Self {
            flow,
            inner: Mutex::new(QueInner {
                total_pending: 0,
                n_duplicates: 0,
                draining: false,
                poll_wakers: Vec::new(),
                subs: HashMap::new(),
                quota: 0,
                size: event_que_size(),
                replace_threshold: events_per_que(),
            }),
            wake: Notify::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Wake every reader parked on this queue — the `Notify` waiters that
    /// [`Self::next`] suspends on AND the [`Self::poll_next`] wakers held in
    /// [`QueInner::poll_wakers`].
    ///
    /// SINGLE OWNER of reader wakeup: every transition that can make a
    /// previously-parked read Ready (a post, a producer/reader teardown, an
    /// EVENTS_ON release) MUST come through here and MUST NOT call
    /// `wake.notify_waiters()` directly, so the two wake mechanisms cannot
    /// diverge — a site that woke only the `Notify` would strand a
    /// `poll_next` consumer forever (and on the RTEMS exec backend a task
    /// whose waker is held nowhere live is dropped outright).
    fn wake_readers(&self) {
        let wakers = std::mem::take(&mut self.lock().poll_wakers);
        self.wake.notify_waiters();
        for waker in wakers {
            waker.wake();
        }
    }

    /// C `db_queue_event_log` (`dbEvent.c:776-868`).
    fn post(&self, sid: u32, event: MonitorEvent) -> PostOutcome {
        let outcome = {
            let mut q = self.lock();
            let flow_on = self.flow.is_on();
            let ring_space = q.ring_space();
            let size = q.size;
            let threshold = q.replace_threshold;
            let Some(sub) = q.subs.get_mut(&sid) else {
                return PostOutcome::Closed;
            };
            if sub.reader_gone {
                return PostOutcome::Closed;
            }
            let npend = sub.events.len();
            if npend > 0 && (flow_on || ring_space <= threshold) {
                // C: `db_delete_field_log(*pLastLog); *pLastLog = pLog;` — the
                // ring does not grow and the earlier distinct entries stay put.
                let last = sub
                    .events
                    .back_mut()
                    .expect("npend > 0 ⇒ this monitor has a last log");
                let displaced = last.mask;
                *last = event;
                // The displaced VALUE is gone (C frees it); its event class is
                // kept — see the module's documented deviations.
                last.mask |= displaced;
                sub.nreplace += 1;
                PostOutcome::Replaced
            } else {
                sub.events.push_back(event);
                // C: an entry appended while the monitor already had one queued
                // is a duplicate — the queue-level count the EVENTS_OFF gate
                // reads (`dbEvent.c:837-839`).
                if npend > 0 {
                    q.n_duplicates += 1;
                }
                q.total_pending += 1;
                debug_assert!(
                    q.total_pending < size,
                    "the quota reservation must keep the ring from filling"
                );
                PostOutcome::Appended {
                    first_event: ring_space == size,
                }
            }
        };
        if outcome != PostOutcome::Closed {
            self.wake_readers();
        }
        outcome
    }

    /// C `event_read` (`dbEvent.c:932-1014`), reader half.
    async fn next(&self, sid: u32) -> Option<MonitorEvent> {
        loop {
            // Arm the wakeup BEFORE inspecting the queue: `notify_waiters()`
            // stores no permit, so a post or an EVENTS_ON landing between the
            // check and the await must find this waiter already registered.
            let wake = self.wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            {
                let mut q = self.lock();
                let flow_on = self.flow.is_on();
                let sub = q.subs.get(&sid)?;
                let has_entry = !sub.events.is_empty();
                let producer_gone = sub.producer_gone;
                if has_entry {
                    if q.may_drain(flow_on) {
                        q.draining = true;
                        return q.remove_front(sid);
                    }
                    // Suspended: C's event_read returns without delivering.
                } else if producer_gone {
                    return None;
                }
            }
            wake.await;
        }
    }

    /// Poll-based [`Self::next`]: the same gate and the same delivery, but
    /// instead of suspending on the queue's `Notify` it registers `cx`'s
    /// waker in [`QueInner::poll_wakers`] and returns [`Poll::Pending`].
    ///
    /// This is what lets ONE task await MANY subscriptions — the QSRV group
    /// drain polls each of its member readers in turn and parks once, its
    /// waker held by every queue it polled. Registration happens under the
    /// queue lock and every Ready-making mutation wakes through
    /// [`Self::wake_readers`] under the same lock, so a post landing between
    /// the check and the `Pending` return cannot be lost.
    ///
    /// [`Poll::Ready`]`(None)` matches [`Self::next`]'s `None`: the
    /// subscription is detached, or its producer is gone and the queue
    /// drained. An entry withheld by EVENTS_OFF parks exactly where
    /// [`Self::next`] suspends (`flowCtrlMode && nDuplicates == 0`, no drain
    /// pass in flight) and is released by the same [`Self::wake_readers`]
    /// call `flow_ctrl_off` makes.
    fn poll_next(
        &self,
        sid: u32,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<MonitorEvent>> {
        use std::task::Poll;
        let mut q = self.lock();
        let flow_on = self.flow.is_on();
        let Some(sub) = q.subs.get(&sid) else {
            return Poll::Ready(None);
        };
        let has_entry = !sub.events.is_empty();
        let producer_gone = sub.producer_gone;
        if has_entry {
            if q.may_drain(flow_on) {
                q.draining = true;
                return Poll::Ready(q.remove_front(sid));
            }
            // Suspended by EVENTS_OFF: park, C's event_read returns without
            // delivering.
        } else if producer_gone {
            return Poll::Ready(None);
        }
        if !q.poll_wakers.iter().any(|w| w.will_wake(cx.waker())) {
            q.poll_wakers.push(cx.waker().clone());
        }
        Poll::Pending
    }

    /// Non-blocking [`Self::next`]: same gate, no suspension.
    fn try_next(&self, sid: u32) -> Result<MonitorEvent, TryRecvError> {
        let mut q = self.lock();
        let flow_on = self.flow.is_on();
        let Some(sub) = q.subs.get(&sid) else {
            return Err(TryRecvError::Disconnected);
        };
        if sub.events.is_empty() {
            return Err(if sub.producer_gone {
                TryRecvError::Disconnected
            } else {
                TryRecvError::Empty
            });
        }
        if !q.may_drain(flow_on) {
            // Suspended by EVENTS_OFF — nothing is deliverable to this monitor.
            return Err(TryRecvError::Empty);
        }
        q.draining = true;
        q.remove_front(sid).ok_or(TryRecvError::Empty)
    }

    /// Is this subscription's reader gone (C: the monitor was cancelled)?
    /// Producer rows are reaped on this, replacing `mpsc::Sender::is_closed`.
    fn reader_gone(&self, sid: u32) -> bool {
        self.lock().subs.get(&sid).is_none_or(|s| s.reader_gone)
    }

    /// The producer row for `sid` is gone: no more posts, but what is already
    /// queued is still delivered.
    fn close_producer(&self, sid: u32) {
        {
            let mut q = self.lock();
            let Some(sub) = q.subs.get_mut(&sid) else {
                return;
            };
            sub.producer_gone = true;
            if sub.reader_gone {
                q.detach(sid);
            }
        }
        self.wake_readers();
    }

    /// The reader for `sid` is gone: its queued entries leave the ring through
    /// the same accounting every other removal uses.
    fn close_reader(&self, sid: u32) {
        {
            let mut q = self.lock();
            let Some(sub) = q.subs.get_mut(&sid) else {
                return;
            };
            sub.reader_gone = true;
            q.detach(sid);
        }
        self.wake_readers();
    }

    /// C `nDuplicates` — entries queued beyond the first for their monitor,
    /// across every subscription on this queue.
    pub fn n_duplicates(&self) -> usize {
        self.lock().n_duplicates
    }

    /// C `nreplace` for one monitor — posts that overwrote its last entry.
    pub fn nreplace(&self, sid: u32) -> u64 {
        self.lock().subs.get(&sid).map_or(0, |s| s.nreplace)
    }

    /// C `npend` for one monitor — entries queued and not yet delivered.
    pub fn npend(&self, sid: u32) -> usize {
        self.lock().subs.get(&sid).map_or(0, |s| s.events.len())
    }

    /// C `quota` — ring entries reserved by the monitors attached here.
    pub fn quota(&self) -> usize {
        self.lock().quota
    }
}

/// C `event_user` (`dbEvent.c:84-105`) — one per CA circuit. Owns the EVENTS_OFF
/// flag and the chain of queues subscriptions attach to.
pub struct EventUser {
    flow: Arc<FlowCtrl>,
    /// C `firstque` + `nextque`: a subscription attaches to the first queue with
    /// spare quota, and a new queue is chained when none has
    /// (`dbEvent.c:446-469`). This — not the client — is the sharing granularity
    /// of `nDuplicates`.
    ques: Mutex<Vec<Arc<EvQue>>>,
}

impl Default for EventUser {
    fn default() -> Self {
        Self::new()
    }
}

impl EventUser {
    /// C `db_init_events`.
    pub fn new() -> Self {
        let flow = Arc::new(FlowCtrl::default());
        Self {
            ques: Mutex::new(vec![Arc::new(EvQue::new(flow.clone()))]),
            flow,
        }
    }

    /// C `db_add_event`'s queue-selection loop (`dbEvent.c:446-469`): walk the
    /// chain for the first queue whose `quota` still admits a monitor, and chain
    /// a fresh one only when none does.
    ///
    /// The sharing this produces is not an implementation detail — it is where
    /// `nDuplicates` lives (R8-23). A duplicate queued for any monitor on the
    /// queue releases the EVENTS_OFF drain of every monitor on it, which
    /// per-subscription rings cannot express.
    fn attach_que(&self, sid: u32) -> Arc<EvQue> {
        let mut ques = self
            .ques
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for que in ques.iter() {
            if que.lock().try_attach(sid) {
                return que.clone();
            }
        }
        let que = Arc::new(EvQue::new(self.flow.clone()));
        let attached = que.lock().try_attach(sid);
        debug_assert!(attached, "a fresh queue must admit its first monitor");
        ques.push(que.clone());
        que
    }

    /// C `db_event_flow_ctrl_mode_on` — EVENTS_OFF. Posts now replace each
    /// monitor's last queued entry in place, and a reader suspends once its
    /// queue holds no duplicates.
    pub fn flow_ctrl_on(&self) {
        self.flow.on.store(true, Ordering::Release);
    }

    /// C `db_event_flow_ctrl_mode_off` — EVENTS_ON. Releases every suspended
    /// reader on this circuit; C posts `ppendsem` for the same reason.
    pub fn flow_ctrl_off(&self) {
        self.flow.on.store(false, Ordering::Release);
        let ques = self
            .ques
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for que in ques.iter() {
            que.wake_readers();
        }
    }

    pub fn is_flow_ctrl_on(&self) -> bool {
        self.flow.is_on()
    }
}

/// Why a non-blocking take found nothing. Mirrors `mpsc::error::TryRecvError` so
/// consumers of the channel this replaced read unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryRecvError {
    /// Nothing deliverable to this monitor right now — either its queue is empty
    /// or EVENTS_OFF is suspending the drain.
    Empty,
    /// The producer row is gone and everything queued has been delivered.
    Disconnected,
}

impl std::fmt::Display for TryRecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "no monitor event available"),
            Self::Disconnected => write!(f, "monitor producer gone"),
        }
    }
}

impl std::error::Error for TryRecvError {}

/// The consumer half of one subscription — C's per-monitor view of `event_read`,
/// and the single owner of the EVENTS_OFF drain-or-suspend decision. Both CA
/// server monitor loops and every in-process consumer reach the gate through
/// here, so they cannot disagree about what a pause does.
///
/// Dropping it cancels the subscription's queue slot (C `db_cancel_event`),
/// releasing its ring entries and its quota.
pub struct EventReader {
    que: Arc<EvQue>,
    sid: u32,
}

impl EventReader {
    /// Await this subscription's next event, suspending exactly where C's
    /// `event_read` does (`flowCtrlMode && nDuplicates == 0`, no drain pass in
    /// flight). `None` = producer gone and queue drained.
    pub async fn recv(&mut self) -> Option<MonitorEvent> {
        self.que.next(self.sid).await
    }

    /// Non-blocking [`Self::recv`].
    pub fn try_recv(&mut self) -> Result<MonitorEvent, TryRecvError> {
        self.que.try_next(self.sid)
    }

    /// Poll-based [`Self::recv`]: `Poll::Ready(None)` where `recv()` returns
    /// `None`, `Poll::Pending` with `cx`'s waker registered on the queue
    /// where `recv()` suspends. For consumers that multiplex many
    /// subscriptions from one task (the QSRV group drain) — the waker stays
    /// registered until [`EvQue::wake_readers`] flushes it, so a caller that
    /// polled several readers and parked is woken by whichever queue changes
    /// first.
    pub fn poll_recv(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<MonitorEvent>> {
        self.que.poll_next(self.sid, cx)
    }

    /// The queue this subscription is attached to — a read-only handle for
    /// diagnostics and tests (`n_duplicates`, `npend`, `nreplace`).
    pub fn queue(&self) -> Arc<EvQue> {
        self.que.clone()
    }

    /// C `npend` for this subscription.
    pub fn npend(&self) -> usize {
        self.que.npend(self.sid)
    }
}

impl Drop for EventReader {
    fn drop(&mut self) {
        self.que.close_reader(self.sid);
    }
}

/// The producer half of one subscription, held by the record / PV that posts to
/// it. Dropping it is the end-of-stream signal a monitor gets when its channel
/// goes away.
pub struct EventSink {
    que: Arc<EvQue>,
    sid: u32,
}

impl EventSink {
    /// C `db_queue_event_log` for this subscription: append, or replace this
    /// monitor's last queued entry in place. The single owner of that decision.
    pub fn post(&self, event: MonitorEvent) -> PostOutcome {
        self.que.post(self.sid, event)
    }

    /// The reader is gone — the producer row can be reaped. Replaces
    /// `mpsc::Sender::is_closed`.
    pub fn is_closed(&self) -> bool {
        self.que.reader_gone(self.sid)
    }
}

impl Drop for EventSink {
    fn drop(&mut self) {
        self.que.close_producer(self.sid);
    }
}

/// C `db_add_event`: attach `sid` to `user`'s queue chain and hand back the
/// producer and consumer halves.
pub fn attach(user: &EventUser, sid: u32) -> (EventSink, EventReader) {
    let que = user.attach_que(sid);
    (
        EventSink {
            que: que.clone(),
            sid,
        },
        EventReader { que, sid },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::recgbl::EventMask;
    use crate::server::snapshot::Snapshot;
    use crate::types::EpicsValue;

    fn ev(v: i32) -> MonitorEvent {
        MonitorEvent {
            snapshot: Snapshot::new(EpicsValue::Long(v), 0, 0, std::time::SystemTime::UNIX_EPOCH),
            origin: 0,
            mask: EventMask::VALUE,
        }
    }

    fn val(e: &MonitorEvent) -> i32 {
        match e.snapshot.value {
            EpicsValue::Long(v) => v,
            ref other => panic!("expected Long, got {other:?}"),
        }
    }

    /// Boundary `npend == 0`: C appends (`dbEvent.c:832-852`) — there is no last
    /// log to replace — even under flow control, and flags the ring's
    /// empty→non-empty transition (C `firstEventFlag`).
    #[tokio::test]
    async fn npend_zero_appends_even_under_flow_control() {
        let user = EventUser::new();
        user.flow_ctrl_on();
        let (sink, reader) = attach(&user, 1);
        assert_eq!(
            sink.post(ev(1)),
            PostOutcome::Appended { first_event: true }
        );
        assert_eq!(reader.npend(), 1);
        assert_eq!(
            reader.queue().n_duplicates(),
            0,
            "a monitor's first entry is not a duplicate"
        );
    }

    /// Boundary `npend > 0` with flow control ON: C replaces `*pLastLog` in
    /// place (`dbEvent.c:812-820`). The queue does not grow and no duplicate is
    /// created — which is exactly why the reader stays suspended.
    #[tokio::test]
    async fn npend_positive_under_flow_control_replaces_in_place() {
        let user = EventUser::new();
        user.flow_ctrl_on();
        let (sink, mut reader) = attach(&user, 1);
        sink.post(ev(1));
        assert_eq!(sink.post(ev(2)), PostOutcome::Replaced);
        assert_eq!(sink.post(ev(3)), PostOutcome::Replaced);
        assert_eq!(reader.npend(), 1, "the queue never grew past one entry");
        assert_eq!(reader.queue().nreplace(1), 2);
        assert_eq!(reader.queue().n_duplicates(), 0);
        user.flow_ctrl_off();
        assert_eq!(
            val(&reader.recv().await.unwrap()),
            3,
            "the latest value survives in the held entry"
        );
        assert!(matches!(reader.try_recv(), Err(TryRecvError::Empty)));
    }

    /// Boundary `npend > 0`, flow control OFF, ring space ABOVE the threshold:
    /// C appends (`dbEvent.c:832-852`), so distinct updates each get their own
    /// entry and each is delivered.
    #[tokio::test]
    async fn ring_space_above_threshold_appends_distinct_entries() {
        let user = EventUser::new();
        let (sink, mut reader) = attach(&user, 1);
        for v in 1..=3 {
            assert!(matches!(sink.post(ev(v)), PostOutcome::Appended { .. }));
        }
        assert_eq!(reader.npend(), 3);
        assert_eq!(reader.queue().n_duplicates(), 2, "npend 3 ⇒ 2 duplicates");
        let got: Vec<i32> = (0..3).map(|_| val(&reader.try_recv().unwrap())).collect();
        assert_eq!(got, vec![1, 2, 3]);
        assert_eq!(reader.queue().n_duplicates(), 0, "symmetric on drain");
    }

    /// R8-22 boundary — `npend > 0`, flow control OFF, ring space AT/BELOW the
    /// threshold: C replaces ONLY the monitor's last entry (`dbEvent.c:812-820`)
    /// and keeps every earlier distinct entry, so burst delivery is
    /// {earlier distinct backlog…, coalesced tail}. The old primitive parked the
    /// newest event in a side slot and the consumer then discarded the whole
    /// backlog, delivering only the newest.
    #[tokio::test]
    async fn ring_space_at_threshold_replaces_only_the_last_entry() {
        let user = EventUser::new();
        let (sink, mut reader) = attach(&user, 1);
        let appended = event_que_size() - events_per_que();
        for v in 0..appended as i32 {
            assert!(
                matches!(sink.post(ev(v)), PostOutcome::Appended { .. }),
                "post {v} must append while ring space is above the threshold"
            );
        }
        assert_eq!(reader.npend(), appended);
        // Ring space is now AT the threshold: every further post replaces the
        // tail in place.
        for v in 100..110 {
            assert_eq!(sink.post(ev(v)), PostOutcome::Replaced);
        }
        assert_eq!(reader.npend(), appended, "the backlog did not grow");
        assert_eq!(reader.queue().nreplace(1), 10);
        let got: Vec<i32> = (0..appended)
            .map(|_| val(&reader.try_recv().unwrap()))
            .collect();
        let mut want: Vec<i32> = (0..appended as i32 - 1).collect();
        want.push(109);
        assert_eq!(
            got, want,
            "earlier distinct entries survive; only the tail coalesced"
        );
        assert!(matches!(reader.try_recv(), Err(TryRecvError::Empty)));
    }

    /// The `nDuplicates` invariant survives the removal path that bypasses the
    /// reader: a subscription torn down with entries still queued (C
    /// `event_remove` per entry, `dbEvent.c:542-558`). Teardown drains through
    /// the same accounting the reader uses, so the counters cannot drift.
    #[tokio::test]
    async fn detach_with_queued_entries_keeps_the_duplicate_count_symmetric() {
        let user = EventUser::new();
        let (sink, reader) = attach(&user, 1);
        let que = reader.queue();
        for v in 1..=3 {
            sink.post(ev(v)); // npend 3 ⇒ 2 duplicates
        }
        assert_eq!(que.n_duplicates(), 2);
        assert_eq!(que.npend(1), 3);
        drop(reader);
        assert_eq!(que.n_duplicates(), 0, "teardown removed its duplicates");
        assert_eq!(que.npend(1), 0, "its entries left the ring");
        // The row is gone, so the producer reaps itself on the next post.
        assert_eq!(sink.post(ev(4)), PostOutcome::Closed);
        assert!(sink.is_closed());
    }

    /// Producer gone with entries still queued: the reader drains them and only
    /// then sees end-of-stream — the `mpsc` contract every consumer was written
    /// against.
    #[tokio::test]
    async fn producer_drop_drains_then_ends_the_stream() {
        let user = EventUser::new();
        let (sink, mut reader) = attach(&user, 1);
        sink.post(ev(1));
        sink.post(ev(2));
        drop(sink);
        assert_eq!(val(&reader.recv().await.unwrap()), 1);
        assert_eq!(val(&reader.recv().await.unwrap()), 2);
        assert!(reader.recv().await.is_none(), "drained ⇒ end of stream");
        assert!(matches!(reader.try_recv(), Err(TryRecvError::Disconnected)));
    }

    /// A reader suspended by EVENTS_OFF wakes on EVENTS_ON without needing a
    /// further post (C signals `ppendsem` from `db_event_flow_ctrl_mode_off`).
    /// A lost wake here would strand the monitor for good.
    #[tokio::test]
    async fn flow_ctrl_off_wakes_a_suspended_reader() {
        let user = Arc::new(EventUser::new());
        user.flow_ctrl_on();
        let (sink, mut reader) = attach(&user, 1);
        sink.post(ev(1));
        sink.post(ev(2)); // replaces in place: still one entry, no duplicate
        let u2 = user.clone();
        let waker = tokio::spawn(async move {
            tokio::task::yield_now().await;
            u2.flow_ctrl_off();
        });
        let got = tokio::time::timeout(std::time::Duration::from_secs(2), reader.recv())
            .await
            .expect("EVENTS_ON must wake the suspended reader")
            .expect("the held entry is delivered");
        waker.await.unwrap();
        assert_eq!(val(&got), 2, "the held entry carries the latest value");
    }

    /// R8-23 boundary — a duplicate on a SIBLING subscription. C's `nDuplicates`
    /// is a field of the queue (`dbEvent.c:80`), so `event_read`'s gate
    /// (`flowCtrlMode && nDuplicates == 0`, `dbEvent.c:947`) is answered by the
    /// queue as a whole: a duplicate queued for monitor B releases the drain of
    /// monitor A's entry even though A has no duplicate of its own.
    ///
    /// The per-subscription queues this replaced evaluated the gate per monitor,
    /// so A stayed suspended until EVENTS_ON.
    #[tokio::test]
    async fn duplicate_on_a_sibling_subscription_releases_the_events_off_drain() {
        let user = EventUser::new();
        let (sink_a, mut reader_a) = attach(&user, 1);
        let (sink_b, _reader_b) = attach(&user, 2);
        assert!(
            Arc::ptr_eq(&reader_a.queue(), &_reader_b.queue()),
            "the quota admits both monitors to one queue — C's sharing granularity"
        );

        sink_a.post(ev(1)); // A: npend 1, no duplicate of its own
        sink_b.post(ev(10));
        sink_b.post(ev(11)); // B: npend 2 ⇒ the queue holds one duplicate
        assert_eq!(reader_a.queue().n_duplicates(), 1);

        user.flow_ctrl_on();
        let got = tokio::time::timeout(std::time::Duration::from_secs(2), reader_a.recv())
            .await
            .expect("a duplicate anywhere on the queue must release the drain")
            .expect("A's entry is delivered");
        assert_eq!(val(&got), 1);
    }

    /// The complement of the boundary above — EVENTS_OFF with the queue holding
    /// NO duplicate on any subscription: every reader on it suspends, and
    /// EVENTS_ON releases them all.
    #[tokio::test]
    async fn flow_control_without_duplicates_suspends_every_reader_on_the_queue() {
        let user = EventUser::new();
        user.flow_ctrl_on();
        let (sink_a, mut reader_a) = attach(&user, 1);
        let (sink_b, mut reader_b) = attach(&user, 2);
        sink_a.post(ev(1));
        sink_b.post(ev(2));
        assert_eq!(reader_a.queue().n_duplicates(), 0);
        assert!(matches!(reader_a.try_recv(), Err(TryRecvError::Empty)));
        assert!(matches!(reader_b.try_recv(), Err(TryRecvError::Empty)));
        user.flow_ctrl_off();
        assert_eq!(val(&reader_a.try_recv().unwrap()), 1);
        assert_eq!(val(&reader_b.try_recv().unwrap()), 2);
    }

    /// Boundary `quota == size - EVENT_ENTRIES`: C chains a new queue rather than
    /// overbooking the ring (`dbEvent.c:446-469`), so a circuit's monitors past
    /// the cap share a *different* `nDuplicates`.
    #[tokio::test]
    async fn quota_exhaustion_chains_a_second_queue() {
        let user = EventUser::new();
        let cap = event_que_size() / EVENT_ENTRIES - 1;
        let mut held = Vec::new();
        for sid in 0..cap as u32 {
            held.push(attach(&user, sid));
        }
        let first = held[0].1.queue();
        for (_, reader) in &held {
            assert!(Arc::ptr_eq(&reader.queue(), &first), "all within quota");
        }
        assert_eq!(first.quota(), event_que_size() - EVENT_ENTRIES);

        let (_sink, overflow) = attach(&user, cap as u32);
        assert!(
            !Arc::ptr_eq(&overflow.queue(), &first),
            "the {cap}th monitor exhausts the quota; the next one chains a queue"
        );
        assert_eq!(overflow.queue().quota(), EVENT_ENTRIES);
    }

    /// C releases the cancelled monitor's quota (`dbEvent.c:999-1002`), so the
    /// freed slot is reusable — the next attach lands back on the first queue
    /// instead of chaining forever.
    #[tokio::test]
    async fn detaching_a_monitor_releases_its_quota() {
        let user = EventUser::new();
        let cap = event_que_size() / EVENT_ENTRIES - 1;
        let mut held: Vec<_> = (0..cap as u32).map(|sid| attach(&user, sid)).collect();
        let first = held[0].1.queue();
        assert_eq!(first.quota(), event_que_size() - EVENT_ENTRIES);
        held.pop(); // cancel one monitor: sink and reader both go
        assert_eq!(first.quota(), event_que_size() - 2 * EVENT_ENTRIES);
        let (_sink, reader) = attach(&user, 900);
        assert!(
            Arc::ptr_eq(&reader.queue(), &first),
            "the released quota must be reusable"
        );
    }

    /// Teardown symmetry on a SHARED queue: cancelling one monitor removes only
    /// its own duplicates from the queue-level count; its sibling's stay.
    #[tokio::test]
    async fn detaching_one_monitor_leaves_a_siblings_duplicates_counted() {
        let user = EventUser::new();
        let (sink_a, reader_a) = attach(&user, 1);
        let (sink_b, reader_b) = attach(&user, 2);
        let que = reader_a.queue();
        for v in 1..=3 {
            sink_a.post(ev(v)); // A: npend 3 ⇒ 2 duplicates
        }
        for v in 10..=11 {
            sink_b.post(ev(v)); // B: npend 2 ⇒ 1 duplicate
        }
        assert_eq!(que.n_duplicates(), 3);
        drop(reader_a);
        drop(sink_a);
        assert_eq!(que.n_duplicates(), 1, "only A's duplicates left the ring");
        assert_eq!(que.npend(2), 2, "B's entries are untouched");
        drop(reader_b);
        drop(sink_b);
        assert_eq!(que.n_duplicates(), 0);
        assert_eq!(que.quota(), 0, "both monitors released their reservation");
    }

    // -- poll_recv: the poll-based reader the QSRV group drain multiplexes on

    /// A waker that counts its wakes, for driving `poll_recv` without a
    /// runtime. `std::task::Wake` gives the `RawWaker` plumbing for free.
    struct CountWaker(std::sync::atomic::AtomicUsize);

    impl std::task::Wake for CountWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn count_waker() -> (Arc<CountWaker>, std::task::Waker) {
        let counter = Arc::new(CountWaker(std::sync::atomic::AtomicUsize::new(0)));
        let waker = std::task::Waker::from(counter.clone());
        (counter, waker)
    }

    /// Boundary empty→posted: a parked `poll_recv` registers its waker, a
    /// post wakes it exactly through `wake_readers`, and the re-poll drains
    /// the entry then parks again. Also proves the waker is NOT re-woken by
    /// its own drain (no self-wake loop for the group drain to spin on).
    #[test]
    fn poll_recv_parks_then_delivers_on_post() {
        let user = EventUser::new();
        let (sink, mut reader) = attach(&user, 7);
        let (counter, waker) = count_waker();
        let mut cx = std::task::Context::from_waker(&waker);

        assert!(reader.poll_recv(&mut cx).is_pending(), "empty queue parks");
        assert_eq!(counter.0.load(Ordering::SeqCst), 0);

        sink.post(ev(41));
        assert_eq!(
            counter.0.load(Ordering::SeqCst),
            1,
            "the post must flush the registered waker"
        );
        match reader.poll_recv(&mut cx) {
            std::task::Poll::Ready(Some(event)) => assert_eq!(val(&event), 41),
            other => panic!("expected the posted event, got {other:?}"),
        }
        assert!(
            reader.poll_recv(&mut cx).is_pending(),
            "drained queue parks"
        );
        // Delivering must not have woken the waker again.
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    }

    /// Boundary producer-gone: queued entries still drain through
    /// `poll_recv`, then the stream ends with `Ready(None)` — same contract
    /// as `recv()` — and the teardown itself wakes a parked poller.
    #[test]
    fn poll_recv_drains_backlog_then_reports_disconnect() {
        let user = EventUser::new();
        let (sink, mut reader) = attach(&user, 7);
        let (counter, waker) = count_waker();
        let mut cx = std::task::Context::from_waker(&waker);

        sink.post(ev(1));
        sink.post(ev(2));
        match reader.poll_recv(&mut cx) {
            std::task::Poll::Ready(Some(event)) => assert_eq!(val(&event), 1),
            other => panic!("expected first entry, got {other:?}"),
        }
        match reader.poll_recv(&mut cx) {
            std::task::Poll::Ready(Some(event)) => assert_eq!(val(&event), 2),
            other => panic!("expected second entry, got {other:?}"),
        }
        assert!(reader.poll_recv(&mut cx).is_pending());
        let woken_before = counter.0.load(Ordering::SeqCst);
        drop(sink);
        assert!(
            counter.0.load(Ordering::SeqCst) > woken_before,
            "producer teardown must wake the parked poller"
        );
        assert!(
            matches!(reader.poll_recv(&mut cx), std::task::Poll::Ready(None)),
            "producer gone + queue drained ⇒ end of stream"
        );
    }

    /// Boundary EVENTS_OFF: an entry withheld by flow control parks
    /// `poll_recv` exactly where `recv()` suspends (`flowCtrlMode &&
    /// nDuplicates == 0`), and EVENTS_ON releases it through the same
    /// `wake_readers` the `Notify` waiters get.
    #[test]
    fn poll_recv_respects_events_off_and_wakes_on_events_on() {
        let user = EventUser::new();
        let (sink, mut reader) = attach(&user, 7);
        let (counter, waker) = count_waker();
        let mut cx = std::task::Context::from_waker(&waker);

        user.flow_ctrl_on();
        sink.post(ev(5)); // npend 0 → appends even under flow control
        assert!(
            reader.poll_recv(&mut cx).is_pending(),
            "EVENTS_OFF with no duplicates suspends the poll-based reader too"
        );
        user.flow_ctrl_off();
        assert_eq!(
            counter.0.load(Ordering::SeqCst),
            1,
            "the post preceded registration (woke nobody); EVENTS_ON must wake \
             the parked poller"
        );
        match reader.poll_recv(&mut cx) {
            std::task::Poll::Ready(Some(event)) => assert_eq!(val(&event), 5),
            other => panic!("expected the withheld entry after EVENTS_ON, got {other:?}"),
        }
    }
}
