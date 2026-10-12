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
//!   `QueInner::release` lowers it — C `db_queue_event_log`
//!   (`dbEvent.c:837-839`) and `event_remove` (`dbEvent.c:542-558`).
//! * MUST: the ring is C's `evque`: `used` entries from `getix`, each a
//!   [`PostLog`] and the slot it was posted to, made once at queue creation;
//!   `used == Σ npend` and a post writes into it, never allocates.
//! * MUST: `used < size` — the `quota` reservation is what guarantees the
//!   ring can never fill: past the replace threshold each attached
//!   subscription can add at most one more entry, and attachment is capped so
//!   that always fits (C asserts the free slot at `dbEvent.c:833`). A
//!   cancelled subscription keeps its slot and its quota until its entries
//!   drain (`dbEvent.c:601-640`, `:999-1002`).
//! * MUST: a subscription is reached through the slab slot its [`AttachId`]
//!   names, never by a search or a hash; a handle whose slot was freed and
//!   reused finds another `generation` there and is refused (C holds the
//!   `evSubscrip*`, `dbEvent.c:776`).
//! * MUST: a subscription's [`LogKind`] is fixed at attach, from the field's
//!   declaration — C `useValque` (`dbEvent.c:493-500`). A [`LogKind::Ref`]
//!   subscription's entries copy no value: the consumer reads the field at
//!   delivery, so a queued one is never stale and a second is never queued
//!   (`dbEvent.c:794-800`). That is what bounds the queue in **bytes**: a
//!   1 MiB waveform monitor costs one fixed-size entry, as in C.
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
//! * When a post replaces `*pLastLog`, or C's early-drop discards it, C frees
//!   the incoming or displaced field log and with it its `mask`. The port ORs
//!   that mask into the survivor so a class-narrowing consumer still learns
//!   which `DBE_*` classes changed since its last delivery (pre-existing
//!   deliberate deviation, 446e0d4a); the surviving *value* is C's.
//! * The early-drop raises no `nreplace`, as in C, but the port counts it in
//!   [`EvQue::ncollapse`] so an operator can see a by-reference monitor
//!   shedding posts.
//! * A per-monitor [`EventReader`] on a shared queue may take its oldest
//!   entry from behind the ring's head; the entries ahead of it move up one
//!   place, so the ring stays contiguous. C has one reader per queue.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::runtime::sync::Arc;
use crate::server::pv::{MonitorEvent, PostLog};
use crate::server::record::{FieldDesc, Special};
use crate::types::EpicsValue;

/// C `EVENTENTRIES` (`dbEvent.c:62`) — ring entries reserved per attached
/// subscription.
pub const EVENT_ENTRIES: usize = 4;

/// C `EVENTSPERQUE` (`dbEvent.c:61`) — the ring-space threshold at or below
/// which a post replaces the monitor's last entry instead of appending, sized
/// by C from the Ethernet MTU. Operators scale the queue with
/// `EPICS_CAS_MAX_EVENTS_PER_CHAN`, set once by [`configure_events_per_que`];
/// until then, and in a process that starts no CA server, it is C's 36,
/// giving C's 144-entry ring. A queue keeps the size it was created with.
pub fn events_per_que() -> usize {
    EVENTS_PER_QUE.load(Ordering::Relaxed)
}

static EVENTS_PER_QUE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(36);

/// Read and clamp `EPICS_CAS_MAX_EVENTS_PER_CHAN` once per process. The CA
/// server calls it on its way up, where `rsrv_init` reads and reports
/// `EPICS_CA_MAX_ARRAY_BYTES` (`caservertask.c:510-530`), so neither the read
/// nor the report runs on the attach path under a record's lock.
pub fn configure_events_per_que() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let raw = crate::runtime::env::get("EPICS_CAS_MAX_EVENTS_PER_CHAN");
        EVENTS_PER_QUE.store(events_per_que_from(raw.as_deref()), Ordering::Relaxed);
    });
}

/// Every value outside `EVENT_ENTRIES..=MAX_EVENTS_PER_QUE` is replaced and
/// said so, in the shape of C's `EPICS_CA_MAX_ARRAY_BYTES` lines; both bounds
/// are constants, so each line is a literal.
fn events_per_que_from(raw: Option<&str>) -> usize {
    const _: () = assert!(EVENT_ENTRIES == 4 && MAX_EVENTS_PER_QUE == 65535);
    let Some(raw) = raw.filter(|v| !v.is_empty()) else {
        return 36;
    };
    let (n, line) = match raw.trim().parse::<usize>() {
        Err(_) => (
            36,
            "CAS: EPICS_CAS_MAX_EVENTS_PER_CHAN was not a positive integer\n",
        ),
        Ok(n) if n < EVENT_ENTRIES => (
            EVENT_ENTRIES,
            "CAS: EPICS_CAS_MAX_EVENTS_PER_CHAN was rounded up to 4\n",
        ),
        Ok(n) if n > MAX_EVENTS_PER_QUE => (
            MAX_EVENTS_PER_QUE,
            "CAS: EPICS_CAS_MAX_EVENTS_PER_CHAN was rounded down to 65535\n",
        ),
        Ok(n) => return n,
    };
    crate::runtime::log::errlog_printf(line);
    n
}

/// A queue holds at most `events_per_que()` monitors, each in a slot an
/// [`AttachId`] names in 16 bits.
pub const MAX_EVENTS_PER_QUE: usize = u16::MAX as usize;

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
/// The queues it made non-empty — C's `firstEventFlag` — are chained through themselves
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
    /// C early-drop branch (`dbEvent.c:794-800`): this monitor's last entry
    /// and the post both read the field at delivery, so the post adds
    /// nothing. Not counted in `nreplace` — C does not count it either.
    Collapsed,
    /// The subscription is gone (reader dropped, or never attached). Nothing was
    /// queued — the `mpsc::Sender::try_send`-on-closed-channel case.
    Closed,
}

/// One subscription, unique for the life of the process, and the slab slot
/// its queue holds it in — C's `evSubscrip*`. Never the client's
/// subscription id, which a client may reuse — and a record's subscriber list
/// holds every circuit's — while the earlier slot still drains. `generation`
/// is what refuses a handle whose slot has since been reused. It is drawn
/// process-wide, not per slot: every circuit's first queue has a slot 0, and
/// a record's subscriber list removes by this id across circuits. Two live
/// ids are distinct unless one outlives 2^48 (2.8e14) later attaches, when
/// the 48-bit generation wraps onto it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AttachId(u64);

static NEXT_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl AttachId {
    const SLOT_BITS: u32 = 16;

    fn new(generation: u64, slot: u16) -> Self {
        Self((generation << Self::SLOT_BITS) | u64::from(slot))
    }

    fn slot(self) -> u16 {
        self.0 as u16
    }

    /// The shift drops the bits past 48, so the generation wraps without
    /// reaching the slot.
    fn next_generation() -> u64 {
        NEXT_GENERATION.fetch_add(1, Ordering::Relaxed)
    }
}

/// C `useValque` (`dbEvent.c:493-500`): whether a subscription's entries copy
/// the value, decided once at attach from the field's declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    /// One element, not `SPC_DBADDR`, no wider than `union native_value`:
    /// each entry holds its scalar.
    Val,
    /// Anything else: an entry holds no value, the consumer reads the field.
    Ref,
}

impl LogKind {
    /// C's test on the record field `field` declares. A record type with no
    /// field table declares its fields by the value they store (`stored`).
    pub fn of_field(
        field: Option<&'static FieldDesc>,
        stored: impl FnOnce() -> Option<EpicsValue>,
    ) -> Self {
        match field {
            Some(fd)
                if fd.special != Special::DbAddr
                    && fd.dbf_type.element_size() <= crate::types::NATIVE_VALUE_BYTES =>
            {
                Self::Val
            }
            Some(_) => Self::Ref,
            None => stored().map_or(Self::Ref, |v| Self::of_value(&v)),
        }
    }

    /// A simple PV, whose declaration is its value.
    pub fn of_value(value: &EpicsValue) -> Self {
        if value.queues_by_value() {
            Self::Val
        } else {
            Self::Ref
        }
    }
}

/// C `evSubscrip` — one subscription's state inside a queue.
struct SubQ {
    /// The handle this slot was attached under.
    id: AttachId,
    /// The client's id for this monitor, which its frames carry.
    sid: u32,
    kind: LogKind,
    /// C `npend` — this monitor's entries in the ring.
    npend: usize,
    /// C `pLastLog`: the ring index of this monitor's newest entry, while
    /// `npend > 0`.
    last: usize,
    /// C `nreplace` — posts that overwrote `*pLastLog`.
    nreplace: u64,
    /// Posts absorbed by C's early-drop, which keeps no counter of its own.
    ncollapse: u64,
    /// The producer row (`EventSink`) is gone: no further posts can arrive. The
    /// reader still drains what is queued and then sees end-of-stream, exactly
    /// as an `mpsc::Receiver` does when the last `Sender` drops.
    producer_gone: bool,
    /// The reader (`EventReader`) is gone (C `user_sub == NULL`): posts are
    /// refused, its entries are dropped as the ring drains, and the slot is
    /// freed after the last.
    reader_gone: bool,
    /// [`EventUser::poll_next`] has reported this monitor's end of stream.
    end_reported: bool,
    /// C `user_sub`/`user_arg`, fixed at attach as `db_add_event` fixes them:
    /// handed to the circuit consumer with each entry.
    context: DeliveryContext,
}

impl SubQ {
    fn new(id: AttachId, sid: u32, kind: LogKind, context: DeliveryContext) -> Self {
        Self {
            id,
            sid,
            kind,
            npend: 0,
            last: 0,
            nreplace: 0,
            ncollapse: 0,
            producer_gone: false,
            reader_gone: false,
            end_reported: false,
            context,
        }
    }
}

/// One ring entry: C's `evque[i]` and the `valque[i]` it points at.
struct Entry {
    id: AttachId,
    log: PostLog,
}

/// The queue's mutable state. Everything here moves under one lock — C's
/// `LOCKEVQUE`.
struct QueInner {
    /// C `evque`/`valque`, `size` entries made at queue creation; `used` of
    /// them from `getix` are occupied, oldest first.
    ring: Box<[Option<Entry>]>,
    getix: usize,
    used: usize,
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
    /// C's `evSubscrip`s attached here, each in the slot its handles name.
    /// Sized at queue creation for the quota's monitor cap, so neither attach
    /// nor detach allocates.
    subs: Vec<Option<SubQ>>,
    /// Slots free for the next attach.
    free: Vec<u16>,
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
    /// The live subscription `id` names, or `None` once its slot was freed —
    /// even if another subscription has taken the slot since.
    fn sub(&self, id: AttachId) -> Option<&SubQ> {
        self.subs[usize::from(id.slot())]
            .as_ref()
            .filter(|s| s.id == id)
    }

    fn sub_mut(&mut self, id: AttachId) -> Option<&mut SubQ> {
        self.subs[usize::from(id.slot())]
            .as_mut()
            .filter(|s| s.id == id)
    }

    /// C `ringSpace` (`dbEvent.c:136-147`) — unused ring entries.
    fn ring_space(&self) -> usize {
        self.size - self.used
    }

    /// The ring index `k` entries past `getix`.
    fn ix(&self, k: usize) -> usize {
        (self.getix + k) % self.size
    }

    /// C `event_remove`'s accounting for one entry of `id` leaving the ring
    /// (`dbEvent.c:542-558`), and `db_cancel_event`'s deferred free once a
    /// cancelled subscription's last entry has left (`dbEvent.c:999-1002`).
    /// The ONLY place `npend` falls. `true` when the entry is to be delivered.
    fn release(&mut self, id: AttachId) -> bool {
        let Some(sub) = self.sub_mut(id) else {
            debug_assert!(false, "a slot is held while it has entries");
            return false;
        };
        sub.npend -= 1;
        let (duplicate, gone, drained) = (sub.npend > 0, sub.reader_gone, sub.npend == 0);
        if duplicate {
            debug_assert!(self.n_duplicates >= 1, "nDuplicates underflow");
            self.n_duplicates -= 1;
        }
        if gone && drained {
            self.free_slot(id);
        }
        !gone
    }

    fn free_slot(&mut self, id: AttachId) {
        self.subs[usize::from(id.slot())] = None;
        self.free.push(id.slot());
        self.quota -= EVENT_ENTRIES;
    }

    /// Take the ring's head entry, whoever it belongs to. The log is moved
    /// out; nothing is freed here.
    fn pop(&mut self) -> Option<(AttachId, PostLog, bool)> {
        if self.used == 0 {
            return None;
        }
        let Entry { id, log } = self.ring[self.getix].take().expect("occupied");
        self.getix = (self.getix + 1) % self.size;
        self.used -= 1;
        let live = self.release(id);
        // C's drain loop ends at `EVENTQEMPTY`.
        self.draining = self.used > 0;
        Some((id, log, live))
    }

    /// C `event_read`'s `evque[getix]`: the ring's oldest live entry, if the
    /// EVENTS_OFF gate lets it go; a cancelled subscription's entries are
    /// dropped on the way, as C frees them (`dbEvent.c:969-1002`).
    fn take_oldest(&mut self, flow_ctrl_on: bool) -> Option<CircuitEvent> {
        if self.used == 0 || !self.may_drain(flow_ctrl_on) {
            return None;
        }
        self.draining = true;
        while let Some((id, log, live)) = self.pop() {
            if !live {
                continue;
            }
            let sub = self.sub(id).expect("a delivered entry's slot is attached");
            return Some(CircuitEvent::Event {
                id,
                sid: sub.sid,
                context: sub.context.clone(),
                event: MonitorEvent::of_log(log),
            });
        }
        None
    }

    /// Monitor `id`'s oldest entry, for its own reader. A cancelled
    /// subscription's entries at the head go first, as the circuit consumer
    /// would drop them; an entry further back is moved to the head, the
    /// entries ahead of it one place down.
    fn take_for(&mut self, id: AttachId) -> Option<PostLog> {
        while self.used > 0 {
            let head = self.ring[self.getix].as_ref().expect("occupied").id;
            if self.sub(head).is_some_and(|s| !s.reader_gone) {
                break;
            }
            self.pop();
        }
        let k =
            (0..self.used).find(|&k| self.ring[self.ix(k)].as_ref().is_some_and(|e| e.id == id))?;
        if k > 0 {
            for j in (1..=k).rev() {
                let (a, b) = (self.ix(j), self.ix(j - 1));
                self.ring.swap(a, b);
            }
            let (getix, size) = (self.getix, self.size);
            for sub in self.subs.iter_mut().flatten() {
                if sub.npend > 0 && (sub.last + size - getix) % size < k {
                    sub.last = (sub.last + 1) % size;
                }
            }
        }
        let (taken, log, live) = self.pop()?;
        debug_assert!(taken == id && live);
        Some(log)
    }

    /// A monitor whose producer is gone and whose entries are all delivered,
    /// reported to the circuit consumer once — the end of stream
    /// [`EventReader::recv`] answers with `None`.
    fn take_ended(&mut self) -> Option<CircuitEvent> {
        let sub = self
            .subs
            .iter_mut()
            .flatten()
            .find(|s| s.producer_gone && !s.end_reported && s.npend == 0)?;
        sub.end_reported = true;
        Some(CircuitEvent::Ended(sub.id))
    }

    /// C `db_add_event`'s attach test (`dbEvent.c:451-457`): take this queue if
    /// it still has room for one more monitor's reservation.
    /// The handle names the slot it was given.
    fn try_attach(
        &mut self,
        generation: u64,
        sid: u32,
        kind: LogKind,
        context: &DeliveryContext,
    ) -> Option<AttachId> {
        if self.quota >= self.size - EVENT_ENTRIES {
            return None;
        }
        self.quota += EVENT_ENTRIES;
        let slot = self
            .free
            .pop()
            .expect("the quota caps monitors at the slab's size");
        let id = AttachId::new(generation, slot);
        self.subs[usize::from(slot)] = Some(SubQ::new(id, sid, kind, context.clone()));
        Some(id)
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
        // The quota admits at most `size / EVENT_ENTRIES - 1` monitors.
        let slots = u16::try_from(size / EVENT_ENTRIES)
            .expect("events_per_que() is capped at MAX_EVENTS_PER_QUE");
        Self {
            flow,
            inner: Mutex::new(QueInner {
                ring: (0..size).map(|_| None).collect(),
                getix: 0,
                used: 0,
                n_duplicates: 0,
                draining: false,
                in_burst: false,
                burst_next: None,
                subs: (0..slots).map(|_| None).collect(),
                free: (0..slots).rev().collect(),
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
    /// `epicsEventSignal(ppendsem)`: the wake is `burst`'s, at its drop, and
    /// owed only for C's `firstEventFlag` (`:842-867`). A reader parked on a
    /// ring that already held an entry is parked by EVENTS_OFF, whose release
    /// wakes it; any other reader has that entry still to take.
    fn post(
        self: &Arc<Self>,
        burst: &mut PostBurst,
        id: AttachId,
        event: MonitorEvent,
    ) -> PostOutcome {
        use crate::server::pv::LogData;
        let mut guard = self.lock();
        let flow_on = self.flow.is_on();
        let q = &mut *guard;
        let ring_space = q.ring_space();
        let Some(sub) = q.subs[usize::from(id.slot())]
            .as_mut()
            .filter(|s| s.id == id && !s.reader_gone)
        else {
            return PostOutcome::Closed;
        };
        let mut log = event.into_log();
        // A by-reference subscription copies no value (`dbEvent.c:727-730`);
        // a copy a filter made is the filter's (M7).
        if sub.kind == LogKind::Ref && matches!(log.value, LogData::Val(_)) {
            log.value = LogData::Ref;
        }
        let npend = sub.npend;
        let last = q.ring[sub.last].as_mut().filter(|_| npend > 0);
        match last {
            // C's early-drop (`dbEvent.c:794-800`): the queued reference
            // reads the field at delivery, so the post adds nothing.
            Some(last) if log.value.is_ref() && last.log.value.is_ref() => {
                last.log.set_mask_or(log.mask());
                sub.ncollapse += 1;
                PostOutcome::Collapsed
            }
            // C: `db_delete_field_log(*pLastLog); *pLastLog = pLog;` — the
            // ring does not grow and the earlier distinct entries stay put.
            Some(last) if flow_on || ring_space <= q.replace_threshold => {
                log.set_mask_or(last.log.mask());
                last.log = log;
                sub.nreplace += 1;
                PostOutcome::Replaced
            }
            _ => {
                let at = (q.getix + q.used) % q.size;
                debug_assert!(q.ring[at].is_none(), "the ring is contiguous");
                q.ring[at] = Some(Entry { id, log });
                sub.npend += 1;
                sub.last = at;
                // C: an entry appended while the monitor already had one
                // queued is a duplicate — the queue-level count the
                // EVENTS_OFF gate reads (`dbEvent.c:837-839`).
                if npend > 0 {
                    q.n_duplicates += 1;
                }
                q.used += 1;
                debug_assert!(
                    q.used < q.size,
                    "the quota reservation must keep the ring from filling"
                );
                let first_event = ring_space == q.size;
                if first_event && !q.in_burst {
                    q.in_burst = true;
                    q.burst_next = burst.head.replace(self.clone());
                }
                PostOutcome::Appended { first_event }
            }
        }
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
        let Some(sub) = q.sub(id) else {
            return Poll::Ready(None);
        };
        let has_entry = sub.npend > 0;
        let producer_gone = sub.producer_gone;
        if has_entry {
            if q.may_drain(flow_on) {
                q.draining = true;
                return Poll::Ready(q.take_for(id).map(MonitorEvent::of_log));
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
        let Some(sub) = q.sub(id) else {
            return Err(TryRecvError::Disconnected);
        };
        if sub.npend == 0 {
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
        q.take_for(id)
            .map(MonitorEvent::of_log)
            .ok_or(TryRecvError::Empty)
    }

    /// Is this subscription's reader gone (C: the monitor was cancelled)?
    /// Producer rows are reaped on this, replacing `mpsc::Sender::is_closed`.
    fn reader_gone(&self, id: AttachId) -> bool {
        self.lock().sub(id).is_none_or(|s| s.reader_gone)
    }

    /// The producer row for `id` is gone: no more posts, but what is already
    /// queued is still delivered.
    fn close_producer(&self, id: AttachId) {
        {
            let mut q = self.lock();
            let Some(sub) = q.sub_mut(id) else {
                return;
            };
            sub.producer_gone = true;
        }
        self.wake_readers();
    }

    /// C `db_cancel_event` (`dbEvent.c:601-640`): the reader for `id` is gone.
    /// Its queued entries stay in the ring until a drain drops them, and the
    /// slot keeps its quota until the last has gone.
    fn close_reader(&self, id: AttachId) {
        {
            let mut q = self.lock();
            let Some(sub) = q.sub_mut(id) else {
                return;
            };
            sub.reader_gone = true;
            if sub.npend == 0 {
                q.free_slot(id);
            }
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
        self.lock().sub(id).map_or(0, |s| s.nreplace)
    }

    /// C `npend` for one monitor — entries queued and not yet delivered.
    pub fn npend(&self, id: AttachId) -> usize {
        self.lock().sub(id).map_or(0, |s| s.npend)
    }

    /// Posts this monitor absorbed by C's uncounted early-drop
    /// (`dbEvent.c:794-800`).
    pub fn ncollapse(&self, id: AttachId) -> u64 {
        self.lock().sub(id).map_or(0, |s| s.ncollapse)
    }

    /// C `useValque == FALSE` for one monitor: a [`LogKind::Ref`] attach. C
    /// reports it as "queueing disabled" in `dbel` (`dbEvent.c:224-226`).
    pub fn latest_only(&self, id: AttachId) -> bool {
        self.lock().sub(id).is_some_and(|s| s.kind == LogKind::Ref)
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
        let sub = q.sub(id);
        QueReport {
            npend: sub.map_or(0, |s| s.npend),
            ring_space: q.ring_space(),
            ring_size: q.size,
            nreplace: sub.map_or(0, |s| s.nreplace),
            latest_only: sub.is_some_and(|s| s.kind == LogKind::Ref),
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
    fn attach_que(
        &self,
        sid: u32,
        kind: LogKind,
        context: &DeliveryContext,
    ) -> (Arc<EvQue>, AttachId) {
        let mut chain = self
            .ques
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ques = &mut chain.ques;
        let generation = AttachId::next_generation();
        for que in ques.iter() {
            if let Some(id) = que.lock().try_attach(generation, sid, kind, context) {
                return (que.clone(), id);
            }
        }
        let que = Arc::new(EvQue::new(self.flow.clone()));
        let id = que
            .lock()
            .try_attach(generation, sid, kind, context)
            .expect("a fresh queue must admit its first monitor");
        ques.push(que.clone());
        // A circuit consumer parked before this queue existed is registered on
        // `firstque` only; re-poll it so it registers here too.
        ques[0].wake_readers();
        (que, id)
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
    /// The slot's [`LogKind`], fixed at attach.
    kind: LogKind,
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

    /// The kind every log posted here holds, fixed at attach.
    pub fn kind(&self) -> LogKind {
        self.kind
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
pub fn attach(user: &EventUser, sid: u32, kind: LogKind) -> (EventSink, EventReader) {
    attach_with(user, sid, kind, no_context())
}

/// The context of a monitor nobody delivers through [`EventUser::poll_next`].
pub fn no_context() -> &'static DeliveryContext {
    static NONE: std::sync::LazyLock<DeliveryContext> = std::sync::LazyLock::new(|| Arc::new(()));
    &NONE
}

/// C `db_add_event`: attach client monitor `sid` to `user`'s queue chain in a
/// fresh slot of `kind` that carries `context` to the circuit consumer, and
/// hand back the producer and consumer halves. The slot is deliverable at
/// once, as a C `evSubscrip` is to `event_read` before `db_event_enable`.
pub fn attach_with(
    user: &EventUser,
    sid: u32,
    kind: LogKind,
    context: &DeliveryContext,
) -> (EventSink, EventReader) {
    let (que, id) = user.attach_que(sid, kind, context);
    (
        EventSink {
            que: que.clone(),
            id,
            kind,
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
        MonitorEvent::read_log(
            &mut Snapshot::new(EpicsValue::Long(v), 0, 0, std::time::SystemTime::UNIX_EPOCH),
            EventMask::VALUE,
        )
    }

    fn val(e: &MonitorEvent) -> i32 {
        match e.value().as_value().cloned().unwrap() {
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
        let (sink, reader) = attach(&user, 1, LogKind::Val);
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
        let (sink, mut reader) = attach(&user, 1, LogKind::Val);
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
        let (sink, mut reader) = attach(&user, 1, LogKind::Val);
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
        let (sink, mut reader) = attach(&user, 1, LogKind::Val);
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

    /// A reader dropped with entries queued (C `db_cancel_event` with
    /// `npend > 0`, `dbEvent.c:601-640`): the next post is refused, the
    /// entries stay until the drain drops them through the accounting every
    /// removal uses (`event_remove`, `dbEvent.c:542-558`), and the slot and
    /// its quota come back only then (`dbEvent.c:623`, `:999-1002`).
    #[epics_macros_rs::epics_test]
    async fn detach_with_queued_entries_keeps_the_duplicate_count_symmetric() {
        let user = EventUser::new();
        let (sink, reader) = attach(&user, 1, LogKind::Val);
        let que = reader.queue();
        let id = reader.id();
        for v in 1..=3 {
            sink.post(ev(v)); // npend 3 ⇒ 2 duplicates
        }
        assert_eq!(que.n_duplicates(), 2);
        assert_eq!(que.npend(id), 3);
        drop(reader);
        assert_eq!(sink.post(ev(4)), PostOutcome::Closed);
        assert!(sink.is_closed());
        assert_eq!(que.npend(id), 3, "cancel leaves the entries queued");
        assert_eq!(que.quota(), EVENT_ENTRIES, "and the quota reserved");
        assert!(
            circuit_now(&user).is_pending(),
            "a dead entry is never delivered"
        );
        assert_eq!(que.n_duplicates(), 0, "the drain removed its duplicates");
        assert_eq!(que.npend(id), 0, "its entries left the ring");
        assert_eq!(que.quota(), 0, "the quota comes back after the last");
    }

    /// Producer gone with entries still queued: the reader drains them and only
    /// then sees end-of-stream — the `mpsc` contract every consumer was written
    /// against.
    #[epics_macros_rs::epics_test]
    async fn producer_drop_drains_then_ends_the_stream() {
        let user = EventUser::new();
        let (sink, mut reader) = attach(&user, 1, LogKind::Val);
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
        let (sink, mut reader) = attach(&user, 1, LogKind::Val);
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
        let (sink_a, mut reader_a) = attach(&user, 1, LogKind::Val);
        let (sink_b, _reader_b) = attach(&user, 2, LogKind::Val);
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
        let (sink_a, mut reader_a) = attach(&user, 1, LogKind::Val);
        let (sink_b, mut reader_b) = attach(&user, 2, LogKind::Val);
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
            held.push(attach(&user, sid, LogKind::Val));
        }
        let first = held[0].1.queue();
        for (_, reader) in &held {
            assert!(Arc::ptr_eq(&reader.queue(), &first), "all within quota");
        }
        assert_eq!(first.quota(), event_que_size() - EVENT_ENTRIES);

        let (_sink, overflow) = attach(&user, cap as u32, LogKind::Val);
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
        let mut held: Vec<_> = (0..cap as u32)
            .map(|sid| attach(&user, sid, LogKind::Val))
            .collect();
        let first = held[0].1.queue();
        assert_eq!(first.quota(), event_que_size() - EVENT_ENTRIES);
        held.pop(); // cancel one monitor: sink and reader both go
        assert_eq!(first.quota(), event_que_size() - 2 * EVENT_ENTRIES);
        let (_sink, reader) = attach(&user, 900, LogKind::Val);
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
        let (sink_a, reader_a) = attach(&user, 1, LogKind::Val);
        let (sink_b, mut reader_b) = attach(&user, 2, LogKind::Val);
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
        assert_eq!(que.n_duplicates(), 3, "A's entries wait for the drain");
        assert_eq!(val(&reader_b.try_recv().unwrap()), 10);
        assert_eq!(
            que.n_duplicates(),
            0,
            "A's duplicates left on the way to B's first entry, then B's own"
        );
        assert_eq!(que.npend(reader_b.id()), 1, "B's second entry is untouched");
        drop(reader_b);
        drop(sink_b);
        assert_eq!(
            que.quota(),
            EVENT_ENTRIES,
            "B's entry holds its reservation"
        );
        assert!(circuit_now(&user).is_pending());
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
        let (sink, mut reader) = attach(&user, 7, LogKind::Val);
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
        let (sink, mut reader) = attach(&user, 7, LogKind::Val);
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
        let (sink, mut reader) = attach(&user, 7, LogKind::Val);
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

    // -- the post path wakes on C's `firstEventFlag` only (`dbEvent.c:842-867`)

    /// An armed consumer slot sees every `ppendsem` take a post makes. Only
    /// the post that makes the ring non-empty takes it.
    #[test]
    fn only_the_post_that_fills_an_empty_ring_wakes() {
        let user = EventUser::new();
        let (sink, _reader) = attach(&user, 1, LogKind::Val);
        let (counter, waker) = count_waker();
        let cx = std::task::Context::from_waker(&waker);
        let flow = sink.que.flow.clone();
        flow.park(&cx);
        sink.post(ev(1));
        assert_eq!(
            counter.0.load(Ordering::SeqCst),
            1,
            "empty ring: first post wakes"
        );
        for v in 2..10 {
            flow.park(&cx);
            sink.post(ev(v));
        }
        assert_eq!(
            counter.0.load(Ordering::SeqCst),
            1,
            "a non-empty ring wakes no one"
        );
    }

    /// EVENTS_OFF parks the reader on a non-empty ring: a replace-last post
    /// leaves it parked, and EVENTS_ON is its one wake.
    #[test]
    fn a_replace_under_events_off_wakes_no_one() {
        let user = EventUser::new();
        let (sink, mut reader) = attach(&user, 1, LogKind::Val);
        let (counter, waker) = count_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        user.flow_ctrl_on();
        sink.post(ev(1));
        assert!(reader.poll_recv(&mut cx).is_pending());
        assert_eq!(sink.post(ev(2)), PostOutcome::Replaced);
        assert_eq!(counter.0.load(Ordering::SeqCst), 0);
        user.flow_ctrl_off();
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    }

    /// The latest-only drop (`dbEvent.c:794-800`) wakes no one either, nor
    /// does a sibling's first entry appended to the same non-empty ring.
    #[test]
    fn a_collapse_or_a_siblings_append_to_a_held_ring_wakes_no_one() {
        let user = EventUser::new();
        let (sink, mut reader) = attach(&user, 1, LogKind::Ref);
        let (sibling, _rs) = attach(&user, 2, LogKind::Val);
        let (counter, waker) = count_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        user.flow_ctrl_on();
        sink.post(wide_ref());
        assert!(reader.poll_recv(&mut cx).is_pending());
        assert_eq!(sink.post(wide_ref()), PostOutcome::Collapsed);
        assert_eq!(
            sibling.post(ev(3)),
            PostOutcome::Appended { first_event: false }
        );
        assert_eq!(counter.0.load(Ordering::SeqCst), 0);
    }

    /// A value too wide for C's `union native_value`: a 32-element waveform
    /// copied into the log as a filter's copy (`arr`), tagged with `v` in
    /// element 0 so delivery order is checkable.
    fn wide(v: i32) -> MonitorEvent {
        let mut arr = vec![0.0f64; 32];
        arr[0] = v as f64;
        MonitorEvent::read_log(
            &mut Snapshot::new(
                EpicsValue::DoubleArray(arr.into()),
                0,
                0,
                std::time::SystemTime::UNIX_EPOCH,
            ),
            EventMask::VALUE,
        )
    }

    /// The log a producer posts for that waveform: no value, read at
    /// delivery (`dbfl_type_ref`).
    fn wide_ref() -> MonitorEvent {
        MonitorEvent::post_log(
            &Snapshot::new(
                EpicsValue::DoubleArray(vec![0.0; 32].into()),
                0,
                0,
                std::time::SystemTime::UNIX_EPOCH,
            ),
            0,
            EventMask::VALUE,
        )
    }

    fn wide_val(e: &MonitorEvent) -> i32 {
        match e.value().as_value().cloned().unwrap() {
            EpicsValue::DoubleArray(ref arr) => arr[0] as i32,
            ref other => panic!("expected DoubleArray, got {other:?}"),
        }
    }

    /// The memory bound, at the boundary that used to break it: ring space is
    /// ABOVE the replace threshold, which is precisely where a by-value monitor
    /// appends (see `ring_space_above_threshold_appends_distinct_entries`).
    /// A by-reference monitor must NOT append there — C's early-drop
    /// (`dbEvent.c:794-800`) caps it at one entry regardless of ring space.
    #[epics_macros_rs::epics_test]
    async fn a_ref_monitor_holds_one_entry_however_much_ring_space_is_free() {
        let user = EventUser::new();
        let (sink, mut reader) = attach(&user, 1, LogKind::Ref);
        assert_eq!(
            sink.post(wide_ref()),
            PostOutcome::Appended { first_event: true },
            "npend == 0 appends: C has no queued log to drop against"
        );
        let posts = event_que_size() * 4;
        for v in 1..posts {
            assert_eq!(
                sink.post(wide_ref()),
                PostOutcome::Collapsed,
                "post {v} landed on a non-empty by-reference monitor"
            );
        }
        assert_eq!(reader.npend(), 1);
        assert_eq!(reader.queue().n_duplicates(), 0);
        assert_eq!(
            reader.queue().nreplace(reader.id()),
            0,
            "C's early-drop is not a replacement and raises no nreplace"
        );
        assert_eq!(reader.queue().ncollapse(reader.id()), posts as u64 - 1);
        assert_eq!(
            reader.recv().await.unwrap().value(),
            crate::server::pv::LogValue::Ref,
            "the entry holds no value: the consumer reads the field"
        );
        assert!(matches!(reader.try_recv(), Err(TryRecvError::Empty)));
    }

    /// The kind is fixed at attach, as C's `useValque` is (`dbEvent.c:493-500`):
    /// a by-reference monitor stores no value even when a post carries a
    /// scalar, so its second post is dropped; a by-value monitor keeps
    /// queueing.
    #[epics_macros_rs::epics_test]
    async fn the_kind_is_fixed_at_attach() {
        let user = EventUser::new();
        let (sink, reader) = attach(&user, 1, LogKind::Ref);
        assert!(reader.queue().latest_only(reader.id()));
        assert!(matches!(sink.post(ev(7)), PostOutcome::Appended { .. }));
        assert_eq!(sink.post(ev(8)), PostOutcome::Collapsed);
        assert_eq!(reader.npend(), 1);
        let mut reader = reader;
        assert_eq!(
            reader.try_recv().unwrap().value(),
            crate::server::pv::LogValue::Ref
        );
        let (sink, reader) = attach(&user, 2, LogKind::Val);
        assert!(!reader.queue().latest_only(reader.id()));
        sink.post(ev(7));
        sink.post(ev(8));
        assert_eq!(reader.npend(), 2);
    }

    /// A filter's copy (`arr`, C `dbfl_type_ref` with a `dtor`) is not a
    /// reference, so it is never dropped against one: it queues and replaces
    /// as a value does, and the consumer gets the copy.
    #[epics_macros_rs::epics_test]
    async fn a_filter_copy_on_a_ref_monitor_queues_as_a_value() {
        let user = EventUser::new();
        let (sink, mut reader) = attach(&user, 1, LogKind::Ref);
        assert!(matches!(sink.post(wide(1)), PostOutcome::Appended { .. }));
        assert!(matches!(sink.post(wide(2)), PostOutcome::Appended { .. }));
        user.flow_ctrl_on();
        assert_eq!(sink.post(wide(3)), PostOutcome::Replaced);
        user.flow_ctrl_off();
        assert_eq!(wide_val(&reader.try_recv().unwrap()), 1);
        assert_eq!(wide_val(&reader.try_recv().unwrap()), 3);
    }

    /// Churn without a drain: every cancelled monitor keeps its slot and
    /// quota while its entries are queued (`dbEvent.c:623`, `:999-1002`), so
    /// attaches move on to chained queues instead of reusing a slot whose
    /// entries are still in the ring, and no ring ever fills. Returning the
    /// quota at cancel overfills the first ring.
    #[test]
    fn a_cancelled_monitor_holds_its_quota_until_its_entries_drain() {
        let user = EventUser::new();
        let first = attach(&user, 0, LogKind::Val).1.queue();
        for sid in 1..4 * event_que_size() as u32 {
            let (sink, reader) = attach(&user, sid, LogKind::Val);
            sink.post(ev(sid as i32));
            sink.post(ev(sid as i32));
            drop(reader);
            assert_eq!(sink.post(ev(0)), PostOutcome::Closed);
        }
        assert!(
            first.quota() > EVENT_ENTRIES,
            "dead monitors still hold theirs"
        );
        assert!(
            circuit_now(&user).is_pending(),
            "no dead entry is delivered"
        );
        assert_eq!(
            first.quota(),
            0,
            "every reservation came back with its entries"
        );
        let (sink, reader) = attach(&user, 7, LogKind::Val);
        assert!(
            Arc::ptr_eq(&reader.queue(), &first),
            "a freed slot is reused"
        );
        sink.post(ev(70));
        assert_eq!(circuit_event(&user), (7, 70), "and sees only its own entry");
        assert!(circuit_now(&user).is_pending());
    }

    /// C `db_cancel_event` waits out a callback in progress
    /// (`dbEvent.c:601-640`). The port does not: an entry the consumer took
    /// before cancel returns is the consumer's and can still be delivered
    /// after; an entry still queued at cancel never is.
    #[test]
    fn an_entry_taken_before_cancel_is_still_the_consumers() {
        let user = EventUser::new();
        let (sink, reader) = attach(&user, 1, LogKind::Val);
        sink.post(ev(1));
        sink.post(ev(2));
        let taken = circuit_now(&user);
        drop(reader);
        match taken {
            std::task::Poll::Ready(CircuitEvent::Event { sid: 1, event, .. }) => {
                assert_eq!(val(&event), 1)
            }
            _ => panic!("the first entry was taken"),
        }
        assert!(circuit_now(&user).is_pending(), "the queued one is dropped");
    }

    /// A monitor that never carries a wide value is untouched by the rule —
    /// `latest_only` stays clear and the C append/replace ring is unchanged.
    #[epics_macros_rs::epics_test]
    async fn narrow_only_monitor_keeps_the_ring_discipline() {
        let user = EventUser::new();
        let (sink, reader) = attach(&user, 1, LogKind::Val);
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
        let (a, _ra) = attach(&user, 1, LogKind::Val);
        let (b, _rb) = attach(&user, 2, LogKind::Val);
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
        let (a, _ra) = attach(&user, 1, LogKind::Val);
        let (b, _rb) = attach(&user, 2, LogKind::Val);
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
        let (a, _ra) = attach(&user, 1, LogKind::Val);
        let (b, mut rb) = attach(&user, 2, LogKind::Val);
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
        let (a, ra) = attach(&user, 1, LogKind::Val);
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
        let (a, _ra) = attach(&user, 1, LogKind::Val);
        a.post(ev(10));
        let (b, _rb) = attach(&user, 2, LogKind::Val);
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
        let (old, old_r) = attach_with(
            &user,
            9,
            LogKind::Val,
            &(Arc::new("old") as DeliveryContext),
        );
        old.post(ev(1));
        drop(old);
        let que = old_r.queue();
        let (new, new_r) = attach_with(
            &user,
            9,
            LogKind::Val,
            &(Arc::new("new") as DeliveryContext),
        );
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
        let (old, old_r) = attach_with(
            &user,
            5,
            LogKind::Val,
            &(Arc::new("old") as DeliveryContext),
        );
        old.post(ev(1));
        drop(old_r);
        let (new, _new_r) = attach_with(
            &user,
            5,
            LogKind::Val,
            &(Arc::new("new") as DeliveryContext),
        );
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
        let held: Vec<_> = (0..cap as u32)
            .map(|sid| attach(&user, sid, LogKind::Val))
            .collect();
        let woken = count.0.load(Ordering::SeqCst);
        let (late, _late_r) = attach(&user, cap as u32, LogKind::Val);
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
        let held: Vec<_> = (0..=cap)
            .map(|sid| attach(user, sid, LogKind::Val))
            .collect();
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

    /// The posting path has nothing to hash: the queue holds no map. The
    /// handle stays `Hash` for the CA server's per-circuit registries, which
    /// only EVENT_ADD/EVENT_CANCEL consult.
    #[test]
    fn a_post_reaches_its_slot_without_a_hash() {
        let src = source_guard::production(
            include_str!("event_queue.rs"),
            source_guard::Comments::Strip,
        );
        for token in ["HashMap", "HashSet", "BTreeMap", "BuildHasher"] {
            assert!(!src.contains(token), "event_queue.rs ships `{token}`");
        }
    }

    /// A slot freed by its reader is reused by the next attach, and the old
    /// producer half finds the new subscription there and is refused.
    #[test]
    fn a_reused_slot_refuses_the_old_sink() {
        let user = EventUser::new();
        let (old_sink, old_reader) = attach(&user, 1, LogKind::Val);
        let slot = old_sink.id().slot();
        drop(old_reader);
        let (new_sink, mut new_reader) = attach(&user, 2, LogKind::Val);
        assert_eq!(new_sink.id().slot(), slot, "the freed slot is reused");
        assert_ne!(new_sink.id(), old_sink.id());
        assert!(old_sink.is_closed());
        assert_eq!(old_sink.post(ev(1)), PostOutcome::Closed);
        assert!(matches!(new_reader.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(old_sink.report().npend, 0);
        drop(old_sink);
        assert!(!new_sink.is_closed(), "the old sink's drop spares the slot");
        new_sink.post(ev(2));
        assert_eq!(new_reader.try_recv().map(|e| val(&e)), Ok(2));
    }

    /// Every circuit's first monitor takes slot 0 of its own queue; the ids
    /// still differ, because a record's subscriber list holds both.
    #[test]
    fn the_same_slot_on_two_circuits_is_two_ids() {
        let (one, two) = (EventUser::new(), EventUser::new());
        let (a, _ra) = attach(&one, 1, LogKind::Val);
        let (b, _rb) = attach(&two, 1, LogKind::Val);
        assert_eq!(a.id().slot(), b.id().slot());
        assert_ne!(a.id(), b.id());
    }

    /// Two live monitors on slot 0 of two circuits, attached 2^32 generations
    /// apart — the distance at which a 32-bit generation gave both one id.
    /// The generations are handed to `try_attach` as `attach_que` hands it
    /// the counter's, so the process-wide `NEXT_GENERATION` other tests draw
    /// from is never written.
    #[test]
    fn two_live_ids_never_collide_across_a_32_bit_wrap() {
        let que = || EvQue::new(Arc::new(FlowCtrl::default()));
        let attach_at = |q: &EvQue, generation: u64| {
            q.lock()
                .try_attach(generation, 1, LogKind::Val, no_context())
                .expect("a fresh queue admits a monitor")
        };
        let a = attach_at(&que(), (1 << 48) - 1);
        let b = attach_at(&que(), (1 << 48) - 1 + (1 << 32));
        assert_eq!((a.slot(), b.slot()), (0, 0));
        assert_ne!(a, b);
        // The 48-bit wrap lands on generation 0, still slot 0.
        let c = attach_at(&que(), 1 << 48);
        assert_eq!(c, AttachId::new(0, 0));
        assert_ne!(c, a);
    }

    /// A queue's slots must fit the 16-bit slot of an [`AttachId`], and a
    /// queue needs `EVENT_ENTRIES` of them; each bound is kept, not defaulted.
    #[test]
    fn events_per_que_is_clamped_to_both_bounds() {
        assert_eq!(events_per_que_from(Some("65535")), 65535);
        assert_eq!(events_per_que_from(Some("65536")), MAX_EVENTS_PER_QUE);
        assert_eq!(events_per_que_from(Some("4")), EVENT_ENTRIES);
        assert_eq!(events_per_que_from(Some("3")), EVENT_ENTRIES);
        assert_eq!(events_per_que_from(Some("x")), 36);
        assert_eq!(events_per_que_from(Some("")), 36);
        assert_eq!(events_per_que_from(None), 36);
    }

    #[test]
    fn an_attach_id_is_eight_bytes() {
        assert_eq!(std::mem::size_of::<AttachId>(), 8);
    }

    fn free_slots(sink: &EventSink) -> usize {
        sink.que.lock().free.len()
    }

    /// Producer first: the reader drains, sees the end, and only its drop
    /// frees the slot.
    #[test]
    fn a_slot_outlives_its_producer_until_the_reader_drops() {
        let user = EventUser::new();
        let (keep, _keep_reader) = attach(&user, 9, LogKind::Val);
        let (sink, mut reader) = attach(&user, 1, LogKind::Val);
        let free = free_slots(&keep);
        sink.post(ev(1));
        drop(sink);
        assert_eq!(free_slots(&keep), free);
        assert_eq!(reader.try_recv().map(|e| val(&e)), Ok(1));
        assert!(matches!(reader.try_recv(), Err(TryRecvError::Disconnected)));
        drop(reader);
        assert_eq!(free_slots(&keep), free + 1);
    }

    /// Reader first, nothing queued: the slot is freed at once and the
    /// producer is refused.
    #[test]
    fn a_slot_is_freed_when_its_reader_drops_first() {
        let user = EventUser::new();
        let (sink, reader) = attach(&user, 1, LogKind::Val);
        let free = free_slots(&sink);
        drop(reader);
        assert_eq!(free_slots(&sink), free + 1);
        assert_eq!(sink.post(ev(2)), PostOutcome::Closed);
        assert_eq!(sink.que.n_duplicates(), 0);
        drop(sink);
        assert_eq!(free_slots_of(&user), free + 1);
    }

    fn free_slots_of(user: &EventUser) -> usize {
        user.ques.lock().unwrap().ques[0].lock().free.len()
    }

    /// `dbel` reads each monitor's own slot on a shared queue.
    #[test]
    fn report_reads_each_monitors_own_slot() {
        let user = EventUser::new();
        let (a, _ra) = attach(&user, 1, LogKind::Val);
        let (b, _rb) = attach(&user, 2, LogKind::Val);
        user.flow_ctrl_on();
        a.post(ev(1));
        a.post(ev(2));
        b.post(ev(3));
        let (ra, rb) = (a.report(), b.report());
        assert_eq!((ra.npend, ra.nreplace), (1, 1));
        assert_eq!((rb.npend, rb.nreplace), (1, 0));
        assert_eq!(ra.ring_space, rb.ring_space);
    }
}
