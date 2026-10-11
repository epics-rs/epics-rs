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
//!   `dbEvent.c:450-469`), carrying the queue-level `nDuplicates`.
//! * `SubQ` == C `evSubscrip`: `npend` (queued entry count), `nreplace`, and
//!   `pLastLog` — the entry a post *replaces* rather than appending.
//!
//! # Invariants (all enforced by [`EvQue`], the single owner)
//!
//! * MUST: `n_duplicates == Σ over subscriptions of max(0, npend - 1)`. Only
//!   the append branch of [`EventSink::post`] raises it and only
//!   `QueInner::remove_front` lowers it — C `db_queue_event_log`
//!   (`dbEvent.c:837-839`) and `event_remove` (`dbEvent.c:542-558`).
//! * MUST: `order` holds one [`AttachId`] per pending entry, in post order,
//!   so `order.len() == Σ npend` and the ids of one monitor appear in `order` in
//!   the order of its `SubQ::events` (C's ring: `evque[getix..putix]`).
//! * MUST: `order.len() < size` — the `quota`
//!   reservation is what guarantees the ring can never fill: past the replace
//!   threshold each attached subscription can add at most one more entry, and
//!   attachment is capped so that always fits (C asserts the free slot at
//!   `dbEvent.c:833`).
//! * MUST NOT: any pending event live outside `SubQ::events`. There is no side
//!   slot; a monitor's newest event is `events.back()` — C's `*pLastLog` —
//!   whether it got there by append or by in-place replace.
//! * MUST: a subscription that has ever carried a value too wide for C's
//!   `union native_value` holds at most ONE pending entry (`latest_only`).
//!   This is what bounds the queue in **bytes**, not just in entries — see
//!   below.
//!
//! # Why an entry bound is not a memory bound
//!
//! C's ring bounds entries: `EVENTQUESIZE` of them, `EVENTENTRIES` reserved
//! per monitor. It gets away with that because an entry is a fixed-size
//! `db_field_log`. For anything wider than `union native_value` — every array
//! field, and every `DBF_STRING` field, whose declared 40 bytes exceed the
//! union's 8 — `db_create_field_log` (`dbEvent.c:726-735`) takes the
//! `dbfl_type_ref` branch and copies **nothing**:
//!
//! ```c
//! pLog->type = dbfl_type_ref;
//! /* don't make a copy yet, just reference the field value */
//! pLog->u.r.field = dbChannelField(chan);
//! /* indicate field value still owned by record */
//! pLog->dtor = NULL;
//! ```
//!
//! and `db_queue_event_log` then refuses to queue a second one
//! (`dbEvent.c:794-800`), because a reference already queued will read the
//! record's current value when it is finally delivered:
//!
//! ```c
//! /* if we have an event on the queue and both the last
//!  * event on the queue and the current event reference
//!  * a record field, simply ignore duplicate events.
//!  */
//! if (pevent->npend > 0u
//!         && !dbfl_has_copy(*pevent->pLastLog)
//!         && !dbfl_has_copy(pLog)) {
//!     db_delete_field_log(pLog);
//!     UNLOCKEVQUE (ev_que);
//!     return;
//! }
//! ```
//!
//! So in C a 1 MiB waveform monitor costs one ~100-byte field log, for ever.
//!
//! The port cannot store a reference: [`MonitorEvent`] owns its `Snapshot`, and
//! `dbfl_has_copy` is therefore always true. Ported literally, the entry bound
//! alone let ONE subscription hold `size - replace_threshold` = 108 whole array
//! copies before the replace branch engaged — 108 MiB for that same waveform,
//! per monitor, per circuit. On an embedded target that is not a slow leak but
//! an allocation failure, and a failed allocation in Rust aborts the process
//! where C's `freeListCalloc` would merely return NULL.
//!
//! Hence `SubQ::latest_only`, C's `useValque == FALSE` reached from the value
//! rather than from the channel: the first post whose value does not satisfy
//! [`EpicsValue::queues_by_value`](crate::types::EpicsValue::queues_by_value) latches the subscription into keep-only-the-
//! latest, and from then on a post with an entry already pending overwrites it
//! instead of appending. Two consequences, both C's:
//!
//! * the subscription's queued bytes are bounded by ONE snapshot, exactly as
//!   C's are bounded by one field log plus the record's own field;
//! * the client still receives the newest value, which is what C's surviving
//!   reference would have read at delivery time.
//!
//! The latch is one-way because C's is: `useValque` is decided once, from the
//! channel's declared element count, and never revisited — a waveform whose
//! `NORD` happens to fall to 1 does not become queueable in C either.
//!
//! # The two decisions, each in exactly one place
//!
//! * **Append or replace** — [`EventSink::post`], C `db_queue_event_log`
//!   (`dbEvent.c:812-852`): with an entry already queued for this monitor AND
//!   (`flowCtrlMode` OR the ring within `EVENTSPERQUE` of full), overwrite
//!   `*pLastLog` in place; otherwise append and grow the ring.
//! * **Drain or suspend** — [`EventReader::recv`] / [`EventReader::try_recv`],
//!   C `event_read` (`dbEvent.c:932-1014`): suspend only while `flowCtrlMode &&
//!   nDuplicates == 0`; otherwise drain the queue to empty. Once a drain pass
//!   starts it runs to `EVENTQEMPTY` without re-checking the gate (C's `while
//!   (evque[getix] != EVENTQEMPTY)` loop), which is what `draining` tracks.
//!
//! Those two are the ONLY ways in and out of the queue: [`EvQue`] hands out no
//! mutating method of its own, so no caller can enqueue past the replace rule
//! or dequeue past the EVENTS_OFF gate.
//!
//! # Two consumers
//!
//! * [`EventUser::poll_next`] is C's `event_task`: one consumer per circuit
//!   taking entries in ring order, `firstque` before `nextque` — the order the
//!   posts were queued in, across every monitor on the circuit. The CA server
//!   delivers through it.
//! * [`EventReader::recv`] takes one monitor's entries only. It is for an
//!   in-process consumer that owns its `EventUser`, where the queue holds one
//!   monitor and the two orders coincide.
//!
//! # Documented deviations from C
//!
//! * C's early-drop (`dbEvent.c:794-800`) keeps the entry already queued and
//!   discards the incoming one, because the queued one is a live reference.
//!   The port's entries are owned copies, so keeping the older one would
//!   deliver a stale value; the latest-only branch keeps the INCOMING event
//!   instead. The queue depth, and therefore the memory, is C's either way,
//!   and so is the value the client ends up seeing.
//! * That branch does not raise `nreplace`, matching C: C's early-drop is not
//!   a replacement and `dbel` reports 0 discards for a by-reference monitor.
//!   The port counts it separately as [`EvQue::ncollapse`].
//! * When a post replaces `*pLastLog`, C frees the displaced field log and with
//!   it its `mask`. The port ORs the displaced mask into the survivor so a
//!   class-narrowing consumer still learns which `DBE_*` classes changed since
//!   its last delivery (pre-existing deliberate deviation, 446e0d4a); the
//!   surviving *value* is C's.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::runtime::sync::Arc;
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
/// flag — and `ppendsem`, the one wakeup of the circuit consumer. Held behind
/// its own `Arc` so an [`EvQue`] can reach both without a back-pointer to the
/// [`EventUser`] that owns the chain.
#[derive(Default, Debug)]
struct FlowCtrl {
    on: AtomicBool,
    /// The parked waker of this `event_user`'s one consumer — the circuit's
    /// [`EventUser::poll_next`], or the sole reader of an in-process
    /// subscription's private user. One slot for the whole chain, so a burst
    /// touching several queues wakes the consumer once, and a wake moves the
    /// waker out rather than freeing a list.
    ppendsem: Mutex<Option<std::task::Waker>>,
}

impl FlowCtrl {
    fn is_on(&self) -> bool {
        self.on.load(Ordering::Acquire)
    }

    fn ppendsem(&self) -> std::sync::MutexGuard<'_, Option<std::task::Waker>> {
        self.ppendsem
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Park `cx` as the consumer. Taken under the polled queue's lock, so a
    /// post landing after the consumer's check finds it parked.
    fn park(&self, cx: &std::task::Context<'_>) {
        let mut parked = self.ppendsem();
        if !parked.as_ref().is_some_and(|w| w.will_wake(cx.waker())) {
            *parked = Some(cx.waker().clone());
        }
    }
}

/// One producer's run of posts — C's `db_post_events` calls made under one
/// `dbScanLock` — whose readers are woken once, when it drops, after its last
/// post. [`EventSink::post_in`] is the only way to post into one, so a post is
/// always followed by its wake.
///
/// The queues it touched are chained through themselves
/// ([`QueInner::burst_next`]), so a burst allocates nothing. A queue already
/// in another producer's burst stays there: that burst's drop wakes it, after
/// this post.
#[derive(Default)]
pub struct PostBurst {
    head: Option<Arc<EvQue>>,
}

impl PostBurst {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline(never)]
    fn wake_chain(&mut self) {
        while let Some(que) = self.head.take() {
            self.head = {
                let mut q = que.lock();
                q.in_burst = false;
                q.burst_next.take()
            };
            que.wake_readers();
        }
    }
}

/// Most bursts post nothing — a put to a field no one monitors — and their
/// drop is inlined down to this test.
impl Drop for PostBurst {
    #[inline]
    fn drop(&mut self) {
        if self.head.is_some() {
            self.wake_chain();
        }
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
    /// C early-drop branch (`dbEvent.c:794-800`): this subscription keeps only
    /// its latest entry because it carries values too wide for C's
    /// `union native_value`, and one was already pending. Depth did not grow.
    /// Not counted in `nreplace` — C does not count it either.
    Collapsed,
    /// The subscription is gone (reader dropped, or never attached). Nothing was
    /// queued — the `mpsc::Sender::try_send`-on-closed-channel case.
    Closed,
}

/// One subscription's slot, unique for the life of the process. A slot is
/// keyed by this and never by the client's subscription id, which a client
/// may reuse — and a record's subscriber list holds every circuit's — while
/// the earlier slot still drains (C keys by the `evSubscrip` pointer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AttachId(u64);

impl AttachId {
    fn next() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// C `evSubscrip` — one subscription's state inside a queue.
struct SubQ {
    /// The client's id for this monitor, which its frames carry.
    sid: u32,
    /// C `npend` is `events.len()`; C `*pLastLog` is `events.back_mut()`. Every
    /// pending event of this monitor lives here and nowhere else.
    events: VecDeque<MonitorEvent>,
    /// C `nreplace` — posts that overwrote `*pLastLog`.
    nreplace: u64,
    /// C `useValque == FALSE` (`dbEvent.c:493-500`), latched from the first
    /// value this subscription carried that does not fit `union native_value`.
    /// While set, the subscription holds at most one pending entry — the rule
    /// that bounds the queue in bytes (see the module header).
    latest_only: bool,
    /// Posts absorbed by the latest-only rule. C's early-drop keeps no counter
    /// of its own; this one exists so an operator can see that a wide-value
    /// monitor is shedding updates rather than silently falling behind.
    ncollapse: u64,
    /// The producer row (`EventSink`) is gone: no further posts can arrive. The
    /// reader still drains what is queued and then sees end-of-stream, exactly
    /// as an `mpsc::Receiver` does when the last `Sender` drops.
    producer_gone: bool,
    /// The reader (`EventReader`) is gone: posts are refused from here on.
    reader_gone: bool,
    /// [`EventUser::poll_next`] has reported this monitor's end of stream.
    end_reported: bool,
    /// C `user_sub`/`user_arg`, fixed at attach as `db_add_event` fixes them:
    /// handed to the circuit consumer with each entry.
    context: DeliveryContext,
}

impl SubQ {
    fn new(sid: u32, context: DeliveryContext) -> Self {
        Self {
            sid,
            events: VecDeque::new(),
            nreplace: 0,
            latest_only: false,
            ncollapse: 0,
            producer_gone: false,
            reader_gone: false,
            end_reported: false,
            context,
        }
    }

    /// C `*pevent->pLastLog = pLog` — overwrite this monitor's newest queued
    /// entry in place. Ring occupancy and `nDuplicates` are untouched by
    /// construction, so both callers (the flow-control replace and the
    /// latest-only collapse) stay symmetric without touching queue counters.
    ///
    /// Requires `!self.events.is_empty()`; both callers test `npend > 0`.
    fn overwrite_last(&mut self, event: MonitorEvent) {
        let last = self
            .events
            .back_mut()
            .expect("npend > 0 ⇒ this monitor has a last log");
        let displaced = last.mask;
        *last = event;
        // The displaced VALUE is gone (C frees it); its event class is kept —
        // see the module's documented deviations.
        last.mask |= displaced;
    }
}

/// The queue's mutable state. Everything here moves under one lock — C's
/// `LOCKEVQUE`.
struct QueInner {
    /// C `evque[getix..putix]`: the owning id of every occupied ring entry,
    /// oldest first. Allocated at `size` up front — the quota keeps it short of
    /// that — so a post never allocates here.
    order: VecDeque<AttachId>,
    /// C `event_que::nDuplicates` (`dbEvent.c:80`) — entries queued beyond the
    /// first for their monitor, summed over every subscription attached HERE.
    /// This is what makes a duplicate on subscription B unblock the drain of
    /// subscription A's entry under EVENTS_OFF.
    n_duplicates: usize,
    /// C `event_read`'s drain loop is in flight: it empties the queue in one
    /// pass and does not re-consult `flowCtrlMode` per entry.
    draining: bool,
    /// This queue is on a [`PostBurst`]'s chain, owed one wake.
    in_burst: bool,
    /// The next queue on that chain.
    burst_next: Option<Arc<EvQue>>,
    subs: HashMap<AttachId, SubQ>,
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
        self.size - self.order.len()
    }

    /// C `event_remove` (`dbEvent.c:542-558`): take this monitor's oldest entry
    /// and keep `nDuplicates` / occupancy symmetric. The ONLY removal path —
    /// the reader, subscription teardown, and queue detach all go through it, so
    /// no caller can move one counter without the other.
    fn remove_front(&mut self, id: AttachId) -> Option<MonitorEvent> {
        let sub = self.subs.get_mut(&id)?;
        let event = sub.events.pop_front()?;
        // C: `npend == 1` ⇒ the monitor has no last log any more; otherwise the
        // entry just removed was one of its duplicates.
        if !sub.events.is_empty() {
            debug_assert!(self.n_duplicates >= 1, "nDuplicates underflow");
            self.n_duplicates -= 1;
        }
        // The oldest of this monitor's ring entries. It is the ring's head
        // whenever the circuit drains in ring order; a per-monitor reader on a
        // shared queue may take it from further back.
        let at = self
            .order
            .iter()
            .position(|&s| s == id)
            .expect("every pending entry has its ring slot");
        self.order.remove(at);
        // C's drain loop ends at `EVENTQEMPTY`.
        self.draining = !self.order.is_empty();
        Some(event)
    }

    /// C `event_read`'s `evque[getix]`: the ring's oldest entry, if the
    /// EVENTS_OFF gate lets it go.
    fn take_oldest(&mut self, flow_ctrl_on: bool) -> Option<CircuitEvent> {
        let id = *self.order.front()?;
        if !self.may_drain(flow_ctrl_on) {
            return None;
        }
        let context = self.subs.get(&id)?.context.clone();
        self.draining = true;
        let event = self.remove_front(id)?;
        Some(CircuitEvent::Event {
            id,
            sid: self.subs[&id].sid,
            context,
            event,
        })
    }

    /// A monitor whose producer is gone and whose entries are all delivered,
    /// reported to the circuit consumer once — the end of stream
    /// [`EventReader::recv`] answers with `None`.
    fn take_ended(&mut self) -> Option<CircuitEvent> {
        let (&id, sub) = self
            .subs
            .iter_mut()
            .find(|(_, s)| s.producer_gone && !s.end_reported && s.events.is_empty())?;
        sub.end_reported = true;
        Some(CircuitEvent::Ended(id))
    }

    /// Drop every entry a departing subscription still holds, then release its
    /// quota. Symmetric by construction: it drains through `remove_front`
    /// rather than zeroing counters, so `n_duplicates` / `order` cannot
    /// drift. C does the same per entry in `event_remove`, and releases the
    /// quota once the cancelled subscription has drained (`dbEvent.c:999-1002`).
    fn detach(&mut self, id: AttachId) {
        while self.remove_front(id).is_some() {}
        if self.subs.remove(&id).is_some() {
            self.quota -= EVENT_ENTRIES;
        }
    }

    /// C `db_add_event`'s attach test (`dbEvent.c:451-457`): take this queue if
    /// it still has room for one more monitor's reservation.
    fn try_attach(&mut self, id: AttachId, sid: u32, context: &DeliveryContext) -> bool {
        if self.quota >= self.size - EVENT_ENTRIES {
            return false;
        }
        self.quota += EVENT_ENTRIES;
        self.subs.insert(id, SubQ::new(sid, context.clone()));
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
/// leave through [`EventUser::poll_next`] or [`EventReader::recv`] /
/// [`EventReader::try_recv`], which are the sole owners of C's two decisions. The accessors below are read-only
/// views of C's counters for diagnostics and tests.
pub struct EvQue {
    flow: Arc<FlowCtrl>,
    inner: Mutex<QueInner>,
}

impl EvQue {
    fn new(flow: Arc<FlowCtrl>) -> Self {
        let replace_threshold = events_per_que();
        let size = EVENT_ENTRIES * replace_threshold;
        Self {
            flow,
            inner: Mutex::new(QueInner {
                order: VecDeque::with_capacity(size),
                n_duplicates: 0,
                draining: false,
                in_burst: false,
                burst_next: None,
                subs: HashMap::new(),
                quota: 0,
                size,
                replace_threshold,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// C `epicsEventSignal(ppendsem)`: wake the consumer parked on this
    /// queue's `event_user`.
    ///
    /// SINGLE OWNER of reader wakeup: every transition that can make a parked
    /// read Ready (a burst's posts, a producer/reader teardown, an EVENTS_ON
    /// release) comes through here. It frees nothing and allocates nothing,
    /// because a [`PostBurst`] drops under the record hold.
    fn wake_readers(&self) {
        if let Some(waker) = self.flow.ppendsem().take() {
            waker.wake();
        }
    }

    /// C `db_queue_event_log` (`dbEvent.c:776-868`), minus its
    /// `epicsEventSignal(ppendsem)`: the wake is `burst`'s, at its drop.
    fn post(
        self: &Arc<Self>,
        burst: &mut PostBurst,
        id: AttachId,
        event: MonitorEvent,
    ) -> PostOutcome {
        let mut q = self.lock();
        let flow_on = self.flow.is_on();
        let ring_space = q.ring_space();
        let size = q.size;
        let threshold = q.replace_threshold;
        let Some(sub) = q.subs.get_mut(&id) else {
            return PostOutcome::Closed;
        };
        if sub.reader_gone {
            return PostOutcome::Closed;
        }
        let npend = sub.events.len();
        // C `db_add_event`'s `useValque` decision (`dbEvent.c:493-500`),
        // taken from the value because the port has no channel here. It is
        // a latch, not a per-post test: C decides once per subscription and
        // never revisits it.
        if !event.snapshot.value.queues_by_value() {
            sub.latest_only = true;
        }
        let outcome = if sub.latest_only && npend > 0 {
            // C `db_queue_event_log`'s early-drop (`dbEvent.c:794-800`).
            // C keeps the queued reference and frees the incoming log; the
            // port keeps the incoming snapshot, because its queued one is a
            // copy that would deliver a stale value where C's reference
            // reads the record. Depth is C's — one entry — either way.
            sub.overwrite_last(event);
            sub.ncollapse += 1;
            PostOutcome::Collapsed
        } else if npend > 0 && (flow_on || ring_space <= threshold) {
            // C: `db_delete_field_log(*pLastLog); *pLastLog = pLog;` — the
            // ring does not grow and the earlier distinct entries stay put.
            sub.overwrite_last(event);
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
            q.order.push_back(id);
            debug_assert!(
                q.order.len() < size,
                "the quota reservation must keep the ring from filling"
            );
            PostOutcome::Appended {
                first_event: ring_space == size,
            }
        };
        if !q.in_burst {
            q.in_burst = true;
            q.burst_next = burst.head.replace(self.clone());
        }
        outcome
    }

    /// C `event_read` (`dbEvent.c:932-1014`), reader half.
    async fn next(&self, id: AttachId) -> Option<MonitorEvent> {
        std::future::poll_fn(|cx| self.poll_next(id, cx)).await
    }

    /// One poll of [`Self::next`]: deliver, or park `cx` as the `event_user`'s
    /// consumer and return [`Poll::Pending`](std::task::Poll::Pending).
    ///
    /// This is what lets ONE task await MANY subscriptions — the QSRV group
    /// drain polls each of its member readers in turn and parks once, its
    /// waker held by each one's private `event_user`. Parking happens under
    /// the queue lock, so a post landing between the check and the `Pending`
    /// return finds it parked.
    ///
    /// [`Poll::Ready`](std::task::Poll::Ready)`(None)` matches [`Self::next`]'s `None`: the
    /// subscription is detached, or its producer is gone and the queue
    /// drained. An entry withheld by EVENTS_OFF parks exactly where
    /// [`Self::next`] suspends (`flowCtrlMode && nDuplicates == 0`, no drain
    /// pass in flight) and is released by the same [`Self::wake_readers`]
    /// call `flow_ctrl_off` makes.
    fn poll_next(
        &self,
        id: AttachId,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<MonitorEvent>> {
        use std::task::Poll;
        let mut q = self.lock();
        let flow_on = self.flow.is_on();
        let Some(sub) = q.subs.get(&id) else {
            return Poll::Ready(None);
        };
        let has_entry = !sub.events.is_empty();
        let producer_gone = sub.producer_gone;
        if has_entry {
            if q.may_drain(flow_on) {
                q.draining = true;
                return Poll::Ready(q.remove_front(id));
            }
            // Suspended by EVENTS_OFF: park, C's event_read returns without
            // delivering.
        } else if producer_gone {
            return Poll::Ready(None);
        }
        self.flow.park(cx);
        Poll::Pending
    }

    /// The circuit consumer's read of this queue: the ring's oldest entry
    /// whichever monitor it belongs to, else a monitor's end of stream, else
    /// park `cx` in the circuit's `ppendsem` slot.
    fn poll_oldest(&self, cx: &mut std::task::Context<'_>) -> std::task::Poll<CircuitEvent> {
        use std::task::Poll;
        let mut q = self.lock();
        let flow_on = self.flow.is_on();
        if let Some(step) = q.take_oldest(flow_on).or_else(|| q.take_ended()) {
            return Poll::Ready(step);
        }
        self.flow.park(cx);
        Poll::Pending
    }

    /// Non-blocking [`Self::next`]: same gate, no suspension.
    fn try_next(&self, id: AttachId) -> Result<MonitorEvent, TryRecvError> {
        let mut q = self.lock();
        let flow_on = self.flow.is_on();
        let Some(sub) = q.subs.get(&id) else {
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
        q.remove_front(id).ok_or(TryRecvError::Empty)
    }

    /// Is this subscription's reader gone (C: the monitor was cancelled)?
    /// Producer rows are reaped on this, replacing `mpsc::Sender::is_closed`.
    fn reader_gone(&self, id: AttachId) -> bool {
        self.lock().subs.get(&id).is_none_or(|s| s.reader_gone)
    }

    /// The producer row for `id` is gone: no more posts, but what is already
    /// queued is still delivered.
    fn close_producer(&self, id: AttachId) {
        {
            let mut q = self.lock();
            let Some(sub) = q.subs.get_mut(&id) else {
                return;
            };
            sub.producer_gone = true;
            if sub.reader_gone {
                q.detach(id);
            }
        }
        self.wake_readers();
    }

    /// The reader for `id` is gone: its queued entries leave the ring through
    /// the same accounting every other removal uses.
    fn close_reader(&self, id: AttachId) {
        {
            let mut q = self.lock();
            let Some(sub) = q.subs.get_mut(&id) else {
                return;
            };
            sub.reader_gone = true;
            q.detach(id);
        }
        self.wake_readers();
    }

    /// C `nDuplicates` — entries queued beyond the first for their monitor,
    /// across every subscription on this queue.
    pub fn n_duplicates(&self) -> usize {
        self.lock().n_duplicates
    }

    /// C `nreplace` for one monitor — posts that overwrote its last entry.
    pub fn nreplace(&self, id: AttachId) -> u64 {
        self.lock().subs.get(&id).map_or(0, |s| s.nreplace)
    }

    /// C `npend` for one monitor — entries queued and not yet delivered.
    pub fn npend(&self, id: AttachId) -> usize {
        self.lock().subs.get(&id).map_or(0, |s| s.events.len())
    }

    /// Posts this monitor absorbed under the latest-only rule — the port's
    /// counter for C's uncounted early-drop (`dbEvent.c:794-800`).
    pub fn ncollapse(&self, id: AttachId) -> u64 {
        self.lock().subs.get(&id).map_or(0, |s| s.ncollapse)
    }

    /// C `useValque == FALSE` for one monitor: it has carried a value too wide
    /// for `union native_value`, so it keeps only its latest entry. C reports
    /// the same state as "queueing disabled" in `dbel` (`dbEvent.c:224-226`).
    pub fn latest_only(&self, id: AttachId) -> bool {
        self.lock().subs.get(&id).is_some_and(|s| s.latest_only)
    }

    /// C `quota` — ring entries reserved by the monitors attached here.
    pub fn quota(&self) -> usize {
        self.lock().quota
    }

    /// Everything `dbel` prints about one monitor's queue, read under a
    /// single `LOCKEVQUE`.
    ///
    /// C takes that lock twice — once in its `level > 1` block for
    /// `ringSpace`, again in its `level > 2` block for `nDuplicates`
    /// (`dbEvent.c:198-233`) — so its two halves can disagree about a queue
    /// a producer is filling between them. One snapshot cannot.
    pub fn report(&self, id: AttachId) -> QueReport {
        let q = self.lock();
        let sub = q.subs.get(&id);
        QueReport {
            npend: sub.map_or(0, |s| s.events.len()),
            ring_space: q.ring_space(),
            ring_size: q.size,
            nreplace: sub.map_or(0, |s| s.nreplace),
            latest_only: sub.is_some_and(|s| s.latest_only),
            n_duplicates: q.n_duplicates,
        }
    }
}

/// One consistent read of a monitor's queue state — the fields C's `dbel`
/// reaches for through `pevent` and `pevent->ev_que` (`dbEvent.c:181-246`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueReport {
    /// C `pevent->npend` — entries queued for this monitor, undelivered.
    pub npend: usize,
    /// C `ringSpace(pevent->ev_que)` — unused entries in the shared ring.
    pub ring_space: usize,
    /// C `EVENTQUESIZE`, the constant `ringSpace` is compared against to
    /// decide "queue empty".
    pub ring_size: usize,
    /// C `pevent->nreplace` — posts that overwrote this monitor's last entry.
    pub nreplace: u64,
    /// C `! pevent->useValque`, which `dbel` reports as "queueing disabled".
    pub latest_only: bool,
    /// C `pevent->ev_que->nDuplicates`, shared by every monitor on the queue.
    pub n_duplicates: usize,
}

/// What the circuit consumer delivers a monitor's entries with — C
/// `user_sub`/`user_arg` — given to [`attach_with`].
pub type DeliveryContext = Arc<dyn std::any::Any + Send + Sync>;

/// What [`EventUser::poll_next`] took off the circuit's queues.
pub enum CircuitEvent {
    /// The oldest queued entry, the monitor (`id`) it was posted to, and
    /// that monitor's [`DeliveryContext`].
    Event {
        id: AttachId,
        /// The client's id for that monitor.
        sid: u32,
        context: DeliveryContext,
        event: MonitorEvent,
    },
    /// Monitor `id`'s producer is gone and its entries are delivered.
    Ended(AttachId),
}

/// C `event_user` (`dbEvent.c:84-105`) — one per CA circuit. Owns the EVENTS_OFF
/// flag and the chain of queues subscriptions attach to.
pub struct EventUser {
    flow: Arc<FlowCtrl>,
    /// C `firstque` + `nextque`: a subscription attaches to the first queue with
    /// spare quota, and a new queue is chained when none has
    /// (`dbEvent.c:450-469`). This — not the client — is the sharing granularity
    /// of `nDuplicates`.
    ques: Mutex<QueChain>,
}

/// The queue chain and where the circuit consumer's drain pass stands on it.
struct QueChain {
    ques: Vec<Arc<EvQue>>,
    /// The queue `event_task`'s loop is on: `event_read` empties it before
    /// the loop moves to `nextque`, and a new pass starts at `firstque`.
    reading: usize,
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
            ques: Mutex::new(QueChain {
                ques: vec![Arc::new(EvQue::new(flow.clone()))],
                reading: 0,
            }),
            flow,
        }
    }

    /// C `db_add_event`'s queue-selection loop (`dbEvent.c:450-469`): walk the
    /// chain for the first queue whose `quota` still admits a monitor, and chain
    /// a fresh one only when none does.
    ///
    /// The sharing this produces is not an implementation detail — it is where
    /// `nDuplicates` lives (R8-23). A duplicate queued for any monitor on the
    /// queue releases the EVENTS_OFF drain of every monitor on it, which
    /// per-subscription rings cannot express.
    fn attach_que(&self, id: AttachId, sid: u32, context: &DeliveryContext) -> Arc<EvQue> {
        let mut chain = self
            .ques
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ques = &mut chain.ques;
        for que in ques.iter() {
            if que.lock().try_attach(id, sid, context) {
                return que.clone();
            }
        }
        let que = Arc::new(EvQue::new(self.flow.clone()));
        let attached = que.lock().try_attach(id, sid, context);
        debug_assert!(attached, "a fresh queue must admit its first monitor");
        ques.push(que.clone());
        // A circuit consumer parked before this queue existed is registered on
        // `firstque` only; re-poll it so it registers here too.
        ques[0].wake_readers();
        que
    }

    /// C `event_task` (`dbEvent.c:1056-1061`): the next entry in ring order,
    /// one pass over the chain per wake. `event_read` empties a queue, posts
    /// landing on it meanwhile included (`dbEvent.c:952`), before the loop
    /// reaches `nextque`; a post to an earlier queue waits for the next pass,
    /// so a busy `firstque` cannot starve the queues behind it. The single consumer of a circuit whose monitors are
    /// delivered in the order they were posted; per-monitor
    /// [`EventReader::recv`] calls on the same queues would take entries out
    /// of that order.
    pub fn poll_next(&self, cx: &mut std::task::Context<'_>) -> std::task::Poll<CircuitEvent> {
        let mut chain = self
            .ques
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let n = chain.ques.len();
        for step in 0..n {
            let at = (chain.reading + step) % n;
            if let std::task::Poll::Ready(event) = chain.ques[at].poll_oldest(cx) {
                chain.reading = at;
                return std::task::Poll::Ready(event);
            }
        }
        // Every queue is parked on `cx`; the next wake is a new pass.
        chain.reading = 0;
        std::task::Poll::Pending
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
        let chain = self
            .ques
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for que in chain.ques.iter() {
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
    id: AttachId,
}

impl EventReader {
    /// Await this subscription's next event, suspending exactly where C's
    /// `event_read` does (`flowCtrlMode && nDuplicates == 0`, no drain pass in
    /// flight). `None` = producer gone and queue drained.
    pub async fn recv(&mut self) -> Option<MonitorEvent> {
        self.que.next(self.id).await
    }

    /// Non-blocking [`Self::recv`].
    pub fn try_recv(&mut self) -> Result<MonitorEvent, TryRecvError> {
        self.que.try_next(self.id)
    }

    /// Poll-based [`Self::recv`]: `Poll::Ready(None)` where `recv()` returns
    /// `None`, `Poll::Pending` with `cx`'s waker registered on the queue
    /// where `recv()` suspends. For consumers that multiplex many
    /// subscriptions from one task (the QSRV group drain) — the waker stays
    /// registered until `EvQue::wake_readers` flushes it, so a caller that
    /// polled several readers and parked is woken by whichever queue changes
    /// first.
    pub fn poll_recv(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<MonitorEvent>> {
        self.que.poll_next(self.id, cx)
    }

    /// The queue this subscription is attached to — a read-only handle for
    /// diagnostics and tests (`n_duplicates`, `npend`, `nreplace`).
    pub fn queue(&self) -> Arc<EvQue> {
        self.que.clone()
    }

    /// This subscription's slot.
    pub fn id(&self) -> AttachId {
        self.id
    }

    /// C `npend` for this subscription.
    pub fn npend(&self) -> usize {
        self.que.npend(self.id)
    }
}

impl Drop for EventReader {
    fn drop(&mut self) {
        self.que.close_reader(self.id);
    }
}

/// The producer half of one subscription, held by the record / PV that posts to
/// it. Dropping it is the end-of-stream signal a monitor gets when its channel
/// goes away.
pub struct EventSink {
    que: Arc<EvQue>,
    id: AttachId,
}

impl EventSink {
    /// C `db_queue_event_log` for this subscription: append, or replace this
    /// monitor's last queued entry in place. The single owner of that decision.
    pub fn post(&self, event: MonitorEvent) -> PostOutcome {
        self.post_in(&mut PostBurst::new(), event)
    }

    /// [`Self::post`] as one of `burst`'s posts, woken when `burst` drops.
    pub fn post_in(&self, burst: &mut PostBurst, event: MonitorEvent) -> PostOutcome {
        self.que.post(burst, self.id, event)
    }

    /// This subscription's slot — what its producer row is removed by.
    pub fn id(&self) -> AttachId {
        self.id
    }

    /// The reader is gone — the producer row can be reaped. Replaces
    /// `mpsc::Sender::is_closed`.
    pub fn is_closed(&self) -> bool {
        self.que.reader_gone(self.id)
    }

    /// This monitor's queue state, for `dbel`.
    pub fn report(&self) -> QueReport {
        self.que.report(self.id)
    }
}

impl Drop for EventSink {
    fn drop(&mut self) {
        self.que.close_producer(self.id);
    }
}

/// [`attach_with`] for a monitor read through its own [`EventReader`], which
/// needs no delivery context.
pub fn attach(user: &EventUser, sid: u32) -> (EventSink, EventReader) {
    attach_with(user, sid, no_context())
}

/// The context of a monitor nobody delivers through [`EventUser::poll_next`].
pub fn no_context() -> &'static DeliveryContext {
    static NONE: std::sync::LazyLock<DeliveryContext> = std::sync::LazyLock::new(|| Arc::new(()));
    &NONE
}

/// C `db_add_event`: attach client monitor `sid` to `user`'s queue chain in a
/// fresh slot that carries `context` to the circuit consumer, and hand back
/// the producer and consumer halves. The slot is deliverable at once, as a C
/// `evSubscrip` is to `event_read` before `db_event_enable`.
pub fn attach_with(
    user: &EventUser,
    sid: u32,
    context: &DeliveryContext,
) -> (EventSink, EventReader) {
    let id = AttachId::next();
    let que = user.attach_que(id, sid, context);
    (
        EventSink {
            que: que.clone(),
            id,
        },
        EventReader { que, id },
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
            snapshot: std::sync::Arc::new(Snapshot::new(
                EpicsValue::Long(v),
                0,
                0,
                std::time::SystemTime::UNIX_EPOCH,
            )),
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
    #[epics_macros_rs::epics_test]
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
    /// place (`dbEvent.c:812-827`). The queue does not grow and no duplicate is
    /// created — which is exactly why the reader stays suspended.
    #[epics_macros_rs::epics_test]
    async fn npend_positive_under_flow_control_replaces_in_place() {
        let user = EventUser::new();
        user.flow_ctrl_on();
        let (sink, mut reader) = attach(&user, 1);
        sink.post(ev(1));
        assert_eq!(sink.post(ev(2)), PostOutcome::Replaced);
        assert_eq!(sink.post(ev(3)), PostOutcome::Replaced);
        assert_eq!(reader.npend(), 1, "the queue never grew past one entry");
        assert_eq!(reader.queue().nreplace(reader.id()), 2);
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
    #[epics_macros_rs::epics_test]
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
    /// threshold: C replaces ONLY the monitor's last entry (`dbEvent.c:812-827`)
    /// and keeps every earlier distinct entry, so burst delivery is
    /// {earlier distinct backlog…, coalesced tail}. The old primitive parked the
    /// newest event in a side slot and the consumer then discarded the whole
    /// backlog, delivering only the newest.
    #[epics_macros_rs::epics_test]
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
        assert_eq!(reader.queue().nreplace(reader.id()), 10);
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
    #[epics_macros_rs::epics_test]
    async fn detach_with_queued_entries_keeps_the_duplicate_count_symmetric() {
        let user = EventUser::new();
        let (sink, reader) = attach(&user, 1);
        let que = reader.queue();
        let id = reader.id();
        for v in 1..=3 {
            sink.post(ev(v)); // npend 3 ⇒ 2 duplicates
        }
        assert_eq!(que.n_duplicates(), 2);
        assert_eq!(que.npend(id), 3);
        drop(reader);
        assert_eq!(que.n_duplicates(), 0, "teardown removed its duplicates");
        assert_eq!(que.npend(id), 0, "its entries left the ring");
        // The row is gone, so the producer reaps itself on the next post.
        assert_eq!(sink.post(ev(4)), PostOutcome::Closed);
        assert!(sink.is_closed());
    }

    /// Producer gone with entries still queued: the reader drains them and only
    /// then sees end-of-stream — the `mpsc` contract every consumer was written
    /// against.
    #[epics_macros_rs::epics_test]
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
    #[epics_macros_rs::epics_test]
    async fn flow_ctrl_off_wakes_a_suspended_reader() {
        let user = Arc::new(EventUser::new());
        user.flow_ctrl_on();
        let (sink, mut reader) = attach(&user, 1);
        sink.post(ev(1));
        sink.post(ev(2)); // replaces in place: still one entry, no duplicate
        let u2 = user.clone();
        let waker = crate::runtime::task::Reactor::current()
            .expect("the test driver enters an executor")
            .spawn(async move {
                crate::runtime::task::yield_now().await;
                u2.flow_ctrl_off();
            });
        let got = crate::runtime::task::timeout(std::time::Duration::from_secs(2), reader.recv())
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
    #[epics_macros_rs::epics_test]
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
        let got = crate::runtime::task::timeout(std::time::Duration::from_secs(2), reader_a.recv())
            .await
            .expect("a duplicate anywhere on the queue must release the drain")
            .expect("A's entry is delivered");
        assert_eq!(val(&got), 1);
    }

    /// The complement of the boundary above — EVENTS_OFF with the queue holding
    /// NO duplicate on any subscription: every reader on it suspends, and
    /// EVENTS_ON releases them all.
    #[epics_macros_rs::epics_test]
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
    /// overbooking the ring (`dbEvent.c:450-469`), so a circuit's monitors past
    /// the cap share a *different* `nDuplicates`.
    #[epics_macros_rs::epics_test]
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
    #[epics_macros_rs::epics_test]
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
    #[epics_macros_rs::epics_test]
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
        assert_eq!(que.npend(reader_b.id()), 2, "B's entries are untouched");
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
    /// nDuplicates == 0`), and EVENTS_ON releases it through
    /// `wake_readers`.
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

    /// A value too wide for C's `union native_value`: a 32-element waveform
    /// post, tagged with `v` in element 0 so delivery order is checkable.
    fn wide(v: i32) -> MonitorEvent {
        let mut arr = vec![0.0f64; 32];
        arr[0] = v as f64;
        MonitorEvent {
            snapshot: std::sync::Arc::new(Snapshot::new(
                EpicsValue::DoubleArray(arr.into()),
                0,
                0,
                std::time::SystemTime::UNIX_EPOCH,
            )),
            origin: 0,
            mask: EventMask::VALUE,
        }
    }

    fn wide_val(e: &MonitorEvent) -> i32 {
        match e.snapshot.value {
            EpicsValue::DoubleArray(ref arr) => arr[0] as i32,
            ref other => panic!("expected DoubleArray, got {other:?}"),
        }
    }

    /// The memory bound, at the boundary that used to break it: ring space is
    /// ABOVE the replace threshold, which is precisely where a narrow monitor
    /// appends (see `ring_space_above_threshold_appends_distinct_entries`).
    /// A wide-value monitor must NOT append there — C's early-drop
    /// (`dbEvent.c:794-800`) caps it at one entry regardless of ring space,
    /// and that cap is what keeps 108 whole array copies from accumulating.
    #[epics_macros_rs::epics_test]
    async fn wide_value_holds_one_entry_however_much_ring_space_is_free() {
        let user = EventUser::new();
        let (sink, mut reader) = attach(&user, 1);
        assert_eq!(
            sink.post(wide(0)),
            PostOutcome::Appended { first_event: true },
            "npend == 0 appends: C has no queued log to drop against"
        );
        let posts = event_que_size() * 4;
        for v in 1..posts as i32 {
            assert_eq!(
                sink.post(wide(v)),
                PostOutcome::Collapsed,
                "post {v} landed on a non-empty wide-value monitor"
            );
        }
        assert_eq!(reader.npend(), 1, "one snapshot's worth of memory, no more");
        assert_eq!(reader.queue().n_duplicates(), 0);
        assert_eq!(
            reader.queue().nreplace(reader.id()),
            0,
            "C's early-drop is not a replacement and raises no nreplace"
        );
        assert_eq!(reader.queue().ncollapse(reader.id()), posts as u64 - 1);
        assert_eq!(
            wide_val(&reader.recv().await.unwrap()),
            posts as i32 - 1,
            "the client sees the newest value, as C's surviving reference would"
        );
        assert!(matches!(reader.try_recv(), Err(TryRecvError::Empty)));
    }

    /// The latch is one-way, as C's `useValque` is: once a monitor has carried
    /// a wide value it keeps only the latest even for a narrow post. Without
    /// this a waveform whose element count dips to a scalar would start
    /// appending again — C never does, because it decided from the channel's
    /// declared element count.
    #[epics_macros_rs::epics_test]
    async fn wide_value_latch_is_one_way() {
        let user = EventUser::new();
        let (sink, reader) = attach(&user, 1);
        sink.post(wide(0));
        assert!(reader.queue().latest_only(reader.id()));
        assert_eq!(sink.post(ev(7)), PostOutcome::Collapsed);
        assert_eq!(sink.post(ev(8)), PostOutcome::Collapsed);
        assert_eq!(reader.npend(), 1);
        assert!(reader.queue().latest_only(reader.id()));
    }

    /// The other side of the boundary: a narrow backlog already queued when the
    /// first wide value arrives. The wide post overwrites the tail rather than
    /// appending, so the queue holds at most one wide snapshot and the earlier
    /// distinct narrow entries still go out — the invariant is on wide entries,
    /// not on depth.
    #[epics_macros_rs::epics_test]
    async fn wide_post_onto_a_narrow_backlog_takes_the_tail() {
        let user = EventUser::new();
        let (sink, mut reader) = attach(&user, 1);
        for v in 1..=3 {
            assert!(matches!(sink.post(ev(v)), PostOutcome::Appended { .. }));
        }
        assert_eq!(sink.post(wide(9)), PostOutcome::Collapsed);
        assert_eq!(reader.npend(), 3, "depth unchanged: the tail was replaced");
        assert_eq!(val(&reader.try_recv().unwrap()), 1);
        assert_eq!(val(&reader.try_recv().unwrap()), 2);
        assert_eq!(
            wide_val(&reader.try_recv().unwrap()),
            9,
            "the third entry is the wide value that displaced ev(3)"
        );
        // A second wide post can only land on an empty or already-wide tail, so
        // two wide snapshots are never pending at once.
        sink.post(wide(10));
        sink.post(wide(11));
        assert_eq!(reader.npend(), 1);
    }

    /// A monitor that never carries a wide value is untouched by the rule —
    /// `latest_only` stays clear and the C append/replace ring is unchanged.
    #[epics_macros_rs::epics_test]
    async fn narrow_only_monitor_keeps_the_ring_discipline() {
        let user = EventUser::new();
        let (sink, reader) = attach(&user, 1);
        for v in 1..=3 {
            assert!(matches!(sink.post(ev(v)), PostOutcome::Appended { .. }));
        }
        assert!(!reader.queue().latest_only(reader.id()));
        assert_eq!(reader.queue().ncollapse(reader.id()), 0);
        assert_eq!(reader.npend(), 3);
    }

    /// One poll of the circuit consumer with a waker nobody watches.
    fn circuit_now(user: &EventUser) -> std::task::Poll<CircuitEvent> {
        user.poll_next(&mut std::task::Context::from_waker(std::task::Waker::noop()))
    }

    fn circuit_event(user: &EventUser) -> (u32, i32) {
        match circuit_now(user) {
            std::task::Poll::Ready(CircuitEvent::Event { sid, event, .. }) => (sid, val(&event)),
            std::task::Poll::Ready(CircuitEvent::Ended(id)) => panic!("{id:?} ended"),
            std::task::Poll::Pending => panic!("nothing to take"),
        }
    }

    /// C `event_read` walks `evque[getix]` (`dbEvent.c:957-1006`): a circuit's
    /// monitors are delivered in the order their posts were queued, not one
    /// monitor's backlog after another's.
    #[epics_macros_rs::epics_test]
    async fn the_circuit_takes_monitors_in_post_order() {
        let user = EventUser::new();
        let (a, _ra) = attach(&user, 1);
        let (b, _rb) = attach(&user, 2);
        a.post(ev(10));
        b.post(ev(20));
        a.post(ev(11));
        b.post(ev(21));
        let got: Vec<_> = (0..4).map(|_| circuit_event(&user)).collect();
        assert_eq!(got, [(1, 10), (2, 20), (1, 11), (2, 21)]);
        assert!(circuit_now(&user).is_pending());
    }

    /// C replaces `*pLastLog` where it sits (`dbEvent.c:812-827`): the
    /// replacing value goes out from the replaced entry's ring slot, ahead of
    /// another monitor's entry posted in between.
    #[epics_macros_rs::epics_test]
    async fn a_replaced_entry_keeps_its_ring_slot() {
        let user = EventUser::new();
        let (a, _ra) = attach(&user, 1);
        let (b, _rb) = attach(&user, 2);
        user.flow_ctrl_on();
        a.post(ev(10));
        b.post(ev(20));
        assert_eq!(a.post(ev(11)), PostOutcome::Replaced);
        user.flow_ctrl_off();
        assert_eq!(circuit_event(&user), (1, 11));
        assert_eq!(circuit_event(&user), (2, 20));
    }

    /// A per-monitor take on a shared queue removes that monitor's ring slot,
    /// so the circuit consumer still sees the rest in post order.
    #[epics_macros_rs::epics_test]
    async fn a_per_monitor_take_leaves_the_ring_order_of_the_rest() {
        let user = EventUser::new();
        let (a, _ra) = attach(&user, 1);
        let (b, mut rb) = attach(&user, 2);
        a.post(ev(10));
        b.post(ev(20));
        a.post(ev(11));
        assert_eq!(val(&rb.try_recv().unwrap()), 20);
        assert_eq!(circuit_event(&user), (1, 10));
        assert_eq!(circuit_event(&user), (1, 11));
        assert!(circuit_now(&user).is_pending());
    }

    /// A monitor whose producer went away is reported ended once, after the
    /// entries it had queued.
    #[epics_macros_rs::epics_test]
    async fn a_monitor_ends_once_after_its_backlog() {
        let user = EventUser::new();
        let (a, ra) = attach(&user, 1);
        a.post(ev(10));
        drop(a);
        assert_eq!(circuit_event(&user), (1, 10));
        assert!(matches!(
            circuit_now(&user),
            std::task::Poll::Ready(CircuitEvent::Ended(id)) if id == ra.id()
        ));
        assert!(circuit_now(&user).is_pending());
    }

    /// Boundary: the entry a monitor's attach queued first (C
    /// `db_post_single_event` before `db_event_enable`) is deliverable at
    /// once, in post order with every other monitor's.
    #[epics_macros_rs::epics_test]
    async fn a_just_attached_monitor_is_deliverable_at_once() {
        let user = EventUser::new();
        let (a, _ra) = attach(&user, 1);
        a.post(ev(10));
        let (b, _rb) = attach(&user, 2);
        b.post(ev(20));
        a.post(ev(11));
        assert_eq!(circuit_event(&user), (1, 10));
        assert_eq!(circuit_event(&user), (2, 20));
        assert_eq!(circuit_event(&user), (1, 11));
    }

    /// Boundary: a client sub_id re-added while its cancelled reader has not
    /// dropped yet (the blocking driver hands the cancel to its event thread).
    /// Each attach owns its slot, so the new monitor posts, the circuit takes
    /// both in post order, and the late drop releases only the old reservation.
    #[epics_macros_rs::epics_test]
    async fn a_sub_id_re_added_before_the_old_reader_drops_keeps_both_slots() {
        let user = EventUser::new();
        let (old, old_r) = attach_with(&user, 9, &(Arc::new("old") as DeliveryContext));
        old.post(ev(1));
        drop(old);
        let que = old_r.queue();
        let (new, new_r) = attach_with(&user, 9, &(Arc::new("new") as DeliveryContext));
        assert_ne!(new.post(ev(2)), PostOutcome::Closed);
        assert_eq!(que.quota(), 2 * EVENT_ENTRIES);
        assert_eq!(circuit_event(&user), (9, 1));
        assert_eq!(circuit_event(&user), (9, 2));
        assert!(matches!(
            circuit_now(&user),
            std::task::Poll::Ready(CircuitEvent::Ended(id)) if id == old_r.id()
        ));
        drop(old_r);
        assert_eq!(que.quota(), EVENT_ENTRIES);
        assert_ne!(new.post(ev(3)), PostOutcome::Closed);
        assert_eq!(circuit_event(&user), (9, 3));
        drop(new_r);
    }

    /// A monitor id reused after its reader dropped carries the new
    /// monitor's context, never the cancelled one's, and none of the
    /// cancelled monitor's entries.
    #[epics_macros_rs::epics_test]
    async fn a_reused_monitor_id_delivers_with_its_own_context() {
        let user = EventUser::new();
        let (old, old_r) = attach_with(&user, 5, &(Arc::new("old") as DeliveryContext));
        old.post(ev(1));
        drop(old_r);
        let (new, _new_r) = attach_with(&user, 5, &(Arc::new("new") as DeliveryContext));
        new.post(ev(2));
        match circuit_now(&user) {
            std::task::Poll::Ready(CircuitEvent::Event {
                sid: 5,
                context,
                event,
                ..
            }) => {
                assert_eq!(context.downcast_ref::<&str>(), Some(&"new"));
                assert_eq!(val(&event), 2);
            }
            _ => panic!("expected monitor 5's entry"),
        }
        assert!(circuit_now(&user).is_pending());
    }

    /// A consumer parked before the quota chained a second queue is woken by
    /// the chaining, and then drains `firstque` before `nextque`
    /// (`dbEvent.c:1056-1061`).
    #[epics_macros_rs::epics_test]
    async fn a_chained_queue_reaches_a_parked_consumer() {
        let user = EventUser::new();
        let (count, waker) = count_waker();
        assert!(
            user.poll_next(&mut std::task::Context::from_waker(&waker))
                .is_pending()
        );
        let cap = event_que_size() / EVENT_ENTRIES - 1;
        let held: Vec<_> = (0..cap as u32).map(|sid| attach(&user, sid)).collect();
        let woken = count.0.load(Ordering::SeqCst);
        let (late, _late_r) = attach(&user, cap as u32);
        assert!(
            count.0.load(Ordering::SeqCst) > woken,
            "chaining a queue wakes the parked consumer"
        );
        late.post(ev(99));
        held[0].0.post(ev(1));
        assert_eq!(circuit_event(&user), (0, 1));
        assert_eq!(circuit_event(&user), (cap as u32, 99));
    }

    /// Monitors `0..=cap` attached: `0..cap` fill `firstque`,
    /// `cap` is chained onto `nextque`.
    fn two_queues(user: &EventUser) -> (u32, Vec<(EventSink, EventReader)>) {
        let cap = (event_que_size() / EVENT_ENTRIES - 1) as u32;
        let held: Vec<_> = (0..=cap).map(|sid| attach(user, sid)).collect();
        assert!(!Arc::ptr_eq(
            &held[0].1.queue(),
            &held[cap as usize].1.queue()
        ));
        (cap, held)
    }

    /// A post to `firstque` after the pass moved on to `nextque` waits for
    /// the next pass (`dbEvent.c:1056-1061`).
    #[epics_macros_rs::epics_test]
    async fn a_pass_drains_nextque_before_returning_to_firstque() {
        let user = EventUser::new();
        let (cap, held) = two_queues(&user);
        let (first, late) = (&held[0].0, &held[cap as usize].0);
        first.post(ev(1));
        late.post(ev(10));
        late.post(ev(11));
        assert_eq!(circuit_event(&user), (0, 1));
        assert_eq!(circuit_event(&user), (cap, 10));
        first.post(ev(2));
        assert_eq!(circuit_event(&user), (cap, 11));
        assert_eq!(circuit_event(&user), (0, 2));
        assert!(circuit_now(&user).is_pending());
    }

    /// A post to the queue being read joins the same `event_read`
    /// (`dbEvent.c:952`): the pass stays on `firstque` until it is empty.
    #[epics_macros_rs::epics_test]
    async fn a_post_to_the_queue_being_read_joins_its_drain() {
        let user = EventUser::new();
        let (cap, held) = two_queues(&user);
        let (first, late) = (&held[0].0, &held[cap as usize].0);
        first.post(ev(1));
        late.post(ev(10));
        assert_eq!(circuit_event(&user), (0, 1));
        first.post(ev(2));
        assert_eq!(circuit_event(&user), (0, 2));
        assert_eq!(circuit_event(&user), (cap, 10));
        assert!(circuit_now(&user).is_pending());
    }

    /// A burst spanning both queues reaches a parked circuit consumer as ONE
    /// wake, after its last post, and the consumer then takes it in C's
    /// per-queue pass: all of `firstque`, then `nextque`
    /// (`dbEvent.c:1056-1061`).
    #[epics_macros_rs::epics_test]
    async fn a_burst_across_two_queues_wakes_the_consumer_once() {
        let user = EventUser::new();
        let (cap, held) = two_queues(&user);
        let (first, late) = (&held[0].0, &held[cap as usize].0);
        let (count, waker) = count_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(user.poll_next(&mut cx).is_pending());
        {
            let mut burst = PostBurst::new();
            first.post_in(&mut burst, ev(1));
            late.post_in(&mut burst, ev(10));
            first.post_in(&mut burst, ev(2));
            late.post_in(&mut burst, ev(11));
            assert_eq!(count.0.load(Ordering::SeqCst), 0, "no wake mid-burst");
        }
        assert_eq!(count.0.load(Ordering::SeqCst), 1, "one wake per burst");
        assert_eq!(circuit_event(&user), (0, 1));
        assert_eq!(circuit_event(&user), (0, 2));
        assert_eq!(circuit_event(&user), (cap, 10));
        assert_eq!(circuit_event(&user), (cap, 11));
        assert!(circuit_now(&user).is_pending());
    }
}
