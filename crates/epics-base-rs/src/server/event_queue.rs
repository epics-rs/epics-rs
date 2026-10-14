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

use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::runtime::sync::Arc;
use crate::server::pv::{LogData, LogParts, MonitorEvent, PostLog};
use crate::server::recgbl::EventMask;
use crate::server::record::{FieldDesc, Special};
use crate::sync::{RetireGuard, RetiringSwap};
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
        let raw = crate::runtime::env::get_lossy("EPICS_CAS_MAX_EVENTS_PER_CHAN");
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

    fn ppendsem(&self) -> parking_lot::MutexGuard<'_, Option<std::task::Waker>> {
        self.ppendsem.lock()
    }
}

/// One producer's run of posts — C's `db_post_events` calls made under one
/// `dbScanLock` — whose readers are woken once, when it drops, after its last
/// post. [`EventSink::post_in`] is the only way to post into one, so a post is
/// always followed by its wake.
///
/// The queues it made non-empty — C's `firstEventFlag` — are chained through themselves
/// (`QueInner::burst_next`), so a burst allocates nothing. A queue already
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
    /// [`SubQ::PRODUCER_GONE`] and [`SubQ::READER_GONE`]; a post is accepted
    /// only while it is 0, one test on the cycle path.
    gone: u8,
    /// [`EventUser::poll_next`] has reported this monitor's end of stream.
    end_reported: bool,
    /// C `user_sub`/`user_arg`, fixed at attach as `db_add_event` fixes them:
    /// handed to the circuit consumer with each entry.
    context: DeliveryContext,
}

impl SubQ {
    /// The producer row (`EventSink`) is closed: no further posts land. The
    /// reader still drains what is queued and then sees end-of-stream, exactly
    /// as an `mpsc::Receiver` does when the last `Sender` drops.
    const PRODUCER_GONE: u8 = 1;
    /// The reader (`EventReader`) is gone (C `user_sub == NULL`): posts are
    /// refused, its entries are dropped as the ring drains, and the slot is
    /// freed after the last.
    const READER_GONE: u8 = 2;

    fn producer_gone(&self) -> bool {
        self.gone & Self::PRODUCER_GONE != 0
    }

    fn reader_gone(&self) -> bool {
        self.gone & Self::READER_GONE != 0
    }

    fn new(id: AttachId, sid: u32, kind: LogKind, context: DeliveryContext) -> Self {
        Self {
            id,
            sid,
            kind,
            npend: 0,
            last: 0,
            nreplace: 0,
            ncollapse: 0,
            gone: 0,
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

/// Ring index `i` of a `size`-entry ring, for any `i < 2 * size`: no divide
/// on the post path.
fn wrap(i: usize, size: usize) -> usize {
    debug_assert!(i < 2 * size);
    if i >= size { i - size } else { i }
}

/// What a post writes into its ring slot: a [`LogParts`] the slot's log is
/// built from in place, or a log a pre chain already made.
pub(crate) trait LogSource {
    fn value(&self) -> &LogData;
    fn mask(&self) -> EventMask;
    /// Make a copied scalar a reference.
    fn drop_val(&mut self);
    /// The log, its mask widened by `also`.
    fn into_log_or(self, also: EventMask) -> PostLog;
}

impl LogSource for PostLog {
    fn value(&self) -> &LogData {
        PostLog::value(self)
    }
    fn mask(&self) -> EventMask {
        PostLog::mask(self)
    }
    fn drop_val(&mut self) {
        if matches!(self.value, LogData::Val(_)) {
            self.value = LogData::Ref;
        }
    }
    fn into_log_or(mut self, also: EventMask) -> PostLog {
        self.set_mask_or(also);
        self
    }
}

impl LogSource for LogParts<'_> {
    fn value(&self) -> &LogData {
        &self.value
    }
    fn mask(&self) -> EventMask {
        self.mask
    }
    fn drop_val(&mut self) {
        if matches!(self.value, LogData::Val(_)) {
            self.value = LogData::Ref;
        }
    }
    #[inline]
    fn into_log_or(mut self, also: EventMask) -> PostLog {
        self.mask |= also;
        self.into_log()
    }
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
    /// Subscriptions whose producer is gone and whose end is not reported
    /// yet: [`Self::take_ended`] scans the slab only while this is nonzero,
    /// not on every empty poll.
    unreported_ends: usize,
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

    /// The ring index `k <= size` entries past `getix`.
    fn ix(&self, k: usize) -> usize {
        wrap(self.getix + k, self.size)
    }

    /// C `event_remove`'s accounting for one entry of `id` leaving the ring
    /// (`dbEvent.c:542-558`), and `db_cancel_event`'s deferred free once a
    /// cancelled subscription's last entry has left (`dbEvent.c:999-1002`).
    /// The ONLY place `npend` falls. `true` when the entry is to be delivered.
    fn release(&mut self, id: AttachId, retired: &mut Retired) -> bool {
        let Some(sub) = self.sub_mut(id) else {
            debug_assert!(false, "a slot is held while it has entries");
            return false;
        };
        sub.npend -= 1;
        let (duplicate, gone, drained) = (sub.npend > 0, sub.reader_gone(), sub.npend == 0);
        if duplicate {
            debug_assert!(self.n_duplicates >= 1, "nDuplicates underflow");
            self.n_duplicates -= 1;
        }
        if gone && drained {
            self.free_slot(id, retired);
        }
        !gone
    }

    /// Detach `id`'s subscription into `retired`. Its context can own the
    /// last reference to a PV whose subscribers close their sinks on this
    /// queue, so it is dropped after the lock, never here (C frees the
    /// `evSubscrip` under `LOCKEVQUE`, but it runs no destructor).
    fn free_slot(&mut self, id: AttachId, retired: &mut Retired) {
        let sub = self.subs[usize::from(id.slot())].take();
        if sub
            .as_ref()
            .is_some_and(|s| s.producer_gone() && !s.end_reported)
        {
            self.unreported_ends -= 1;
        }
        retired.retire_sub(sub);
        self.free.push(id.slot());
        self.quota -= EVENT_ENTRIES;
    }

    /// Retire the ring's head entry, whoever it belongs to: the slot it
    /// sits in, its monitor, and whether it is to be delivered. The entry
    /// is left in that slot for the caller to take before it lets go of
    /// `self`, so its log moves once, straight into what the caller builds.
    fn advance(&mut self, retired: &mut Retired) -> Option<(usize, AttachId, bool)> {
        if self.used == 0 {
            return None;
        }
        let at = self.getix;
        let id = self.ring[at].as_ref().expect("occupied").id;
        self.getix = self.ix(1);
        self.used -= 1;
        let live = self.release(id, retired);
        // C's drain loop ends at `EVENTQEMPTY`.
        self.draining = self.used > 0;
        Some((at, id, live))
    }

    /// The log of the entry [`Self::advance`] retired from slot `at`.
    fn take_log(&mut self, at: usize) -> PostLog {
        self.ring[at].take().expect("occupied").log
    }

    /// C `event_read`'s `evque[getix]`: the ring's oldest live entry, if the
    /// EVENTS_OFF gate lets it go; a cancelled subscription's entries are
    /// dropped on the way, as C frees them (`dbEvent.c:969-1002`).
    fn take_oldest(&mut self, flow_ctrl_on: bool, retired: &mut Retired) -> Option<CircuitEvent> {
        if self.used == 0 || !self.may_drain(flow_ctrl_on) {
            return None;
        }
        self.draining = true;
        while let Some((at, id, live)) = self.advance(retired) {
            if !live {
                self.ring[at] = None;
                continue;
            }
            let sub = self.sub(id).expect("a delivered entry's slot is attached");
            let (sid, context) = (sub.sid, sub.context.clone());
            return Some(CircuitEvent::Event {
                id,
                sid,
                context,
                event: MonitorEvent::of_log(self.take_log(at)),
                // C `eventsRemaining`: the next ring slot, a cancelled
                // monitor's entry included (`dbEvent.c:965`).
                remaining: self.used > 0,
            });
        }
        None
    }

    /// Monitor `id`'s oldest entry, for its own reader. A cancelled
    /// subscription's entries at the head go first, as the circuit consumer
    /// would drop them; an entry further back is moved to the head, the
    /// entries ahead of it one place down.
    fn take_for(&mut self, id: AttachId, retired: &mut Retired) -> Option<PostLog> {
        while self.used > 0 {
            let head = self.ring[self.getix].as_ref().expect("occupied").id;
            if self.sub(head).is_some_and(|s| !s.reader_gone()) {
                break;
            }
            let (at, _, _) = self.advance(retired).expect("used > 0");
            self.ring[at] = None;
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
                if sub.npend > 0 && wrap(sub.last + size - getix, size) < k {
                    sub.last = wrap(sub.last + 1, size);
                }
            }
        }
        let (at, taken, live) = self.advance(retired)?;
        debug_assert!(taken == id && live);
        Some(self.take_log(at))
    }

    /// A monitor whose producer is gone and whose entries are all delivered,
    /// reported to the circuit consumer once — the end of stream
    /// [`EventReader::recv`] answers with `None`.
    fn take_ended(&mut self) -> Option<CircuitEvent> {
        if self.unreported_ends == 0 {
            return None;
        }
        let sub = self
            .subs
            .iter_mut()
            .flatten()
            .find(|s| s.producer_gone() && !s.end_reported && s.npend == 0)?;
        sub.end_reported = true;
        let id = sub.id;
        self.unreported_ends -= 1;
        Some(CircuitEvent::Ended(id))
    }

    /// Subscription `id`'s producer row is gone; `false` if it is detached.
    fn close_producer(&mut self, id: AttachId) -> bool {
        let Some(sub) = self.sub_mut(id) else {
            return false;
        };
        if !sub.producer_gone() {
            sub.gone |= SubQ::PRODUCER_GONE;
            self.unreported_ends += 1;
        }
        true
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

/// The only way to a [`Retired`]: its fields and its guard's are private to
/// this module, so nothing but [`EvQue::lock_retiring`] can build one, and
/// every one drops after the lock it was filled under.
mod retiring {
    use super::{EvQue, FlowCtrl, QueInner, SubQ};

    /// What a holder of a queue's lock displaced: subscriptions freed from
    /// their slots and a parked waker replaced. Either can own user state
    /// whose drop takes this lock again, so it is dropped only by
    /// [`QueGuard`], after the lock.
    ///
    /// Nothing is displaced on almost every lock, so that case must be one
    /// inline null test: an inline `Vec<SubQ>`, or a plain `Option<Box<_>>`,
    /// left the drop an out-of-line glue call with a full prologue. The
    /// `ManuallyDrop` has no glue; [`Drop`] below empties it.
    pub(super) struct Retired(std::mem::ManuallyDrop<Option<Box<Displaced>>>);

    impl Drop for Retired {
        #[inline]
        fn drop(&mut self) {
            if let Some(displaced) = self.0.take() {
                release(displaced);
            }
        }
    }

    #[cold]
    #[inline(never)]
    fn release(displaced: Box<Displaced>) {
        drop(displaced);
    }

    #[derive(Default)]
    struct Displaced {
        subs: Vec<SubQ>,
        waker: Option<std::task::Waker>,
    }

    impl Retired {
        fn displaced(&mut self) -> &mut Displaced {
            self.0.get_or_insert_default()
        }

        pub(super) fn retire_sub(&mut self, sub: Option<SubQ>) {
            if let Some(sub) = sub {
                self.displaced().subs.push(sub);
            }
        }
    }

    /// [`EvQue`]'s lock, carrying the [`Retired`] its holder displaced.
    pub(super) struct QueGuard<'a> {
        // Fields drop in declaration order: the lock is released before
        // `retired` drops.
        q: parking_lot::MutexGuard<'a, QueInner>,
        retired: Retired,
        flow: &'a FlowCtrl,
    }

    impl QueGuard<'_> {
        pub(super) fn parts(&mut self) -> (&mut QueInner, &mut Retired) {
            (&mut self.q, &mut self.retired)
        }

        /// Park `cx` as the consumer. Taken under the polled queue's lock, so
        /// a post landing after the consumer's check finds it parked. The
        /// waker it displaces goes to `retired`: the last waker of a task
        /// drops its future. Consuming the guard makes this the one
        /// displacement it carries, so none is overwritten under the lock.
        pub(super) fn park(mut self, cx: &std::task::Context<'_>) {
            let mut parked = self.flow.ppendsem();
            if !parked.as_ref().is_some_and(|w| w.will_wake(cx.waker())) {
                if let Some(old) = parked.replace(cx.waker().clone()) {
                    self.retired.displaced().waker = Some(old);
                }
            }
        }
    }

    impl std::ops::Deref for QueGuard<'_> {
        type Target = QueInner;
        fn deref(&self) -> &QueInner {
            &self.q
        }
    }

    impl std::ops::DerefMut for QueGuard<'_> {
        fn deref_mut(&mut self) -> &mut QueInner {
            &mut self.q
        }
    }

    impl EvQue {
        /// [`Self::lock`] for a holder that may free a slot or displace a
        /// parked waker; the post path takes the plain lock and pays nothing
        /// for it.
        pub(super) fn lock_retiring(&self) -> QueGuard<'_> {
            QueGuard {
                q: self.lock(),
                retired: Retired(std::mem::ManuallyDrop::new(None)),
                flow: &self.flow,
            }
        }
    }
}
use retiring::Retired;

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
                unreported_ends: 0,
            }),
        }
    }

    fn lock(&self) -> parking_lot::MutexGuard<'_, QueInner> {
        self.inner.lock()
    }

    /// C `epicsEventSignal(ppendsem)`: wake the consumer parked on this
    /// queue's `event_user`.
    ///
    /// SINGLE OWNER of reader wakeup: every transition that can make a parked
    /// read Ready (a burst's posts, a producer/reader teardown, an EVENTS_ON
    /// release) comes through here. It frees nothing and allocates nothing,
    /// because a [`PostBurst`] drops under the record hold.
    fn wake_readers(&self) {
        // Woken after the guard drops: a parker contending for `ppendsem`
        // would otherwise be woken into the still-held lock.
        let waker = self.flow.ppendsem().take();
        if let Some(waker) = waker {
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
        mut log: impl LogSource,
    ) -> PostOutcome {
        let mut guard = self.lock();
        let flow_on = self.flow.is_on();
        let q = &mut *guard;
        let ring_space = q.ring_space();
        let Some(sub) = q.subs[usize::from(id.slot())]
            .as_mut()
            .filter(|s| s.id == id && s.gone == 0)
        else {
            return PostOutcome::Closed;
        };
        // A by-reference subscription copies no value (`dbEvent.c:727-730`);
        // a copy a filter made is the filter's (M7).
        if sub.kind == LogKind::Ref {
            log.drop_val();
        }
        let npend = sub.npend;
        let last = q.ring[sub.last].as_mut().filter(|_| npend > 0);
        match last {
            // C's early-drop (`dbEvent.c:794-800`): the queued reference
            // reads the field at delivery, so the post adds nothing.
            Some(last) if log.value().is_ref() && last.log.value.is_ref() => {
                last.log.set_mask_or(log.mask());
                sub.ncollapse += 1;
                PostOutcome::Collapsed
            }
            // C: `db_delete_field_log(*pLastLog); *pLastLog = pLog;` — the
            // ring does not grow and the earlier distinct entries stay put.
            Some(last) if flow_on || ring_space <= q.replace_threshold => {
                last.log = log.into_log_or(last.log.mask());
                sub.nreplace += 1;
                PostOutcome::Replaced
            }
            _ => {
                let at = wrap(q.getix + q.used, q.size);
                debug_assert!(q.ring[at].is_none(), "the ring is contiguous");
                q.ring[at] = Some(Entry {
                    id,
                    log: log.into_log_or(EventMask::NONE),
                });
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
        let mut guard = self.lock_retiring();
        let flow_on = self.flow.is_on();
        let (q, retired) = guard.parts();
        let Some(sub) = q.sub(id) else {
            return Poll::Ready(None);
        };
        let has_entry = sub.npend > 0;
        let producer_gone = sub.producer_gone();
        if has_entry {
            if q.may_drain(flow_on) {
                q.draining = true;
                return Poll::Ready(q.take_for(id, retired).map(MonitorEvent::of_log));
            }
            // Suspended by EVENTS_OFF: park, C's event_read returns without
            // delivering.
        } else if producer_gone {
            return Poll::Ready(None);
        }
        guard.park(cx);
        Poll::Pending
    }

    /// The circuit consumer's read of this queue: the ring's oldest entry
    /// whichever monitor it belongs to, else a monitor's end of stream, else
    /// park `cx` in the circuit's `ppendsem` slot.
    fn poll_oldest(&self, cx: &mut std::task::Context<'_>) -> std::task::Poll<CircuitEvent> {
        use std::task::Poll;
        let mut guard = self.lock_retiring();
        let flow_on = self.flow.is_on();
        let (q, retired) = guard.parts();
        if let Some(event) = q.take_oldest(flow_on, retired) {
            return Poll::Ready(event);
        }
        if let Some(ended) = q.take_ended() {
            return Poll::Ready(ended);
        }
        guard.park(cx);
        Poll::Pending
    }

    /// Non-blocking [`Self::next`]: same gate, no suspension.
    fn try_next(&self, id: AttachId) -> Result<MonitorEvent, TryRecvError> {
        let mut guard = self.lock_retiring();
        let flow_on = self.flow.is_on();
        let (q, retired) = guard.parts();
        let Some(sub) = q.sub(id) else {
            return Err(TryRecvError::Disconnected);
        };
        if sub.npend == 0 {
            return Err(if sub.producer_gone() {
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
        q.take_for(id, retired)
            .map(MonitorEvent::of_log)
            .ok_or(TryRecvError::Empty)
    }

    /// Does this subscription refuse posts — its reader gone (C: the monitor
    /// was cancelled) or its producer closed? Producer rows are reaped on
    /// this, replacing `mpsc::Sender::is_closed`.
    fn refuses_posts(&self, id: AttachId) -> bool {
        self.lock().sub(id).is_none_or(|s| s.gone != 0)
    }

    /// The producer row for `id` is gone: no more posts, but what is already
    /// queued is still delivered.
    fn close_producer(&self, id: AttachId) {
        if self.lock().close_producer(id) {
            self.wake_readers();
        }
    }

    /// C `db_cancel_event` (`dbEvent.c:601-640`): the reader for `id` is gone.
    /// Its queued entries stay in the ring until a drain drops them, and the
    /// slot keeps its quota until the last has gone.
    fn close_reader(&self, id: AttachId) {
        {
            let mut guard = self.lock_retiring();
            let (q, retired) = guard.parts();
            let Some(sub) = q.sub_mut(id) else {
                return;
            };
            sub.gone |= SubQ::READER_GONE;
            if sub.npend == 0 {
                q.free_slot(id, retired);
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
        /// C `eventsRemaining`: this queue holds another entry. A consumer
        /// flushes its send buffer on `false`, as `read_reply` does
        /// (`camessage.c:525`, `:536`, `:593`).
        remaining: bool,
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
        let mut chain = self.ques.lock();
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
        // No wake: `ppendsem` is one slot for the whole chain, so a consumer
        // parked before this queue existed is woken by its first post, and C's
        // `db_add_event` signals nothing.
        ques.push(que.clone());
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
        let mut chain = self.ques.lock();
        let n = chain.ques.len();
        for step in 0..n {
            // `reading < n`: no divide per poll.
            let at = wrap(chain.reading + step, n);
            if let std::task::Poll::Ready(event) = chain.ques[at].poll_oldest(cx) {
                chain.reading = at;
                return std::task::Poll::Ready(event);
            }
        }
        // Every queue is parked on `cx`; the next wake is a new pass.
        chain.reading = 0;
        std::task::Poll::Pending
    }

    /// The context monitor `id` was attached with, for the consumer of its
    /// [`CircuitEvent::Ended`]: the row outlives that report until the
    /// monitor's [`EventReader`] drops.
    pub fn context(&self, id: AttachId) -> Option<DeliveryContext> {
        let chain = self.ques.lock();
        chain.ques.iter().find_map(|que| {
            // Queues chained before and after `configure_events_per_que`
            // can differ in slot count.
            let q = que.lock();
            (usize::from(id.slot()) < q.subs.len())
                .then(|| q.sub(id).map(|sub| sub.context.clone()))
                .flatten()
        })
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
        let chain = self.ques.lock();
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
    /// The monitor list holding this subscription's row, which the drop
    /// unlinks it from (C `db_cancel_event`'s `ellDelete`), so the list a
    /// post walks never holds a row whose reader is gone.
    owner: Option<std::sync::Weak<dyn RowOwner>>,
}

/// A monitor list an [`EventReader`]'s drop unlinks its row from.
pub(crate) trait RowOwner: Send + Sync {
    fn remove_row(&self, id: AttachId);
}

impl EventReader {
    /// Unlink this subscription's row from `owner` when the reader drops.
    pub(crate) fn owned_by(mut self, owner: std::sync::Weak<dyn RowOwner>) -> Self {
        self.owner = Some(owner);
        self
    }

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
        if let Some(owner) = self.owner.take().and_then(|o| o.upgrade()) {
            owner.remove_row(self.id);
        }
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
        self.que.post(burst, self.id, event.into_log())
    }

    /// [`Self::post_in`] of a log the queue builds in its ring slot.
    pub(crate) fn post_parts(&self, burst: &mut PostBurst, parts: LogParts<'_>) -> PostOutcome {
        self.que.post(burst, self.id, parts)
    }

    /// This subscription's slot — what its producer row is removed by.
    pub fn id(&self) -> AttachId {
        self.id
    }

    /// The kind every log posted here holds, fixed at attach.
    pub fn kind(&self) -> LogKind {
        self.kind
    }

    /// The queue refuses this sink's posts — the producer row can be reaped.
    /// Replaces `mpsc::Sender::is_closed`.
    pub fn is_closed(&self) -> bool {
        self.que.refuses_posts(self.id)
    }

    /// End this monitor's stream now: later posts are refused and the reader
    /// gets end-of-stream once what is queued is delivered. The owner that
    /// removed the monitor calls it — a [`SubscriberList`] map retired by
    /// that removal can still hold this sink until a later store.
    pub(crate) fn close(&self) {
        self.que.close_producer(self.id);
    }

    /// This monitor's queue state, for `dbel`.
    pub fn report(&self) -> QueReport {
        self.que.report(self.id)
    }
}

impl Drop for EventSink {
    fn drop(&mut self) {
        self.close();
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
        EventReader {
            que,
            id,
            owner: None,
        },
    )
}

/// A record's monitors keyed by field index, C's `mlis` — read lock-free by
/// the cycle, replaced whole by EVENT_ADD/CANCEL without the record lock.
///
/// Entries are sorted by field and in attach order within a field, so a post
/// walks only its field's run. Readers get a [`RetireGuard`]: the map they
/// see is freed by a later writer, never by their guard's drop.
pub struct SubscriberList<T> {
    map: RetiringSwap<SubscriberMap<T>>,
    /// Never below the published map's length — [`Self::publish`] raises it
    /// before the store and lowers it after — so a reader that sees 0 sees an
    /// empty map. A nonzero count does not say the map holds anyone.
    count: AtomicUsize,
    /// Serialises the copy-modify-store and holds the closed state, so only
    /// [`Self::close`], inside this section, can close the list and every
    /// add reads it in the section it publishes in. A cycle thread never
    /// takes it.
    writer: parking_lot::Mutex<bool>,
}

/// A [`SubscriberList`] add refused because [`SubscriberList::close`] ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;

/// One version of a [`SubscriberList`]'s map.
///
/// The map owns each monitor through an `Arc` a reader never sees: it gets
/// `&T`, so no cycle thread can clone the owning handle out of a guard and
/// keep the monitor alive past its removal.
///
/// ```
/// use std::sync::Arc;
/// use epics_base_rs::server::event_queue::SubscriberList;
/// let list = SubscriberList::default();
/// list.add(0, Arc::new(1u32), || ());
/// let _seen: &u32 = list.load().of(0).next().unwrap();
/// ```
///
/// ```compile_fail
/// use std::sync::Arc;
/// use epics_base_rs::server::event_queue::SubscriberList;
/// let list = SubscriberList::default();
/// list.add(0, Arc::new(1u32), || ());
/// let _owned: Arc<u32> = list.load().of(0).next().unwrap().clone();
/// ```
///
/// ```compile_fail
/// use std::sync::Arc;
/// use epics_base_rs::server::event_queue::SubscriberList;
/// let list = SubscriberList::default();
/// list.add(0, Arc::new(1u32), || ());
/// let _owned: Arc<u32> = list.load().iter().next().unwrap().1.clone();
/// ```
pub struct SubscriberMap<T> {
    entries: Vec<(u16, Arc<T>)>,
}

impl<T> SubscriberMap<T> {
    /// The run of `field`'s monitors, in attach order.
    pub fn of(&self, field: u16) -> impl Iterator<Item = &T> {
        let start = self.entries.partition_point(|(f, _)| *f < field);
        self.entries[start..]
            .iter()
            .take_while(move |(f, _)| *f == field)
            .map(|(_, s)| &**s)
    }

    /// Every monitor with its field index, by field then attach order.
    pub fn iter(&self) -> impl Iterator<Item = (u16, &T)> {
        self.entries.iter().map(|(f, s)| (*f, &**s))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl<T> Default for SubscriberList<T> {
    fn default() -> Self {
        Self {
            map: RetiringSwap::new(Arc::new(SubscriberMap {
                entries: Vec::new(),
            })),
            count: AtomicUsize::new(0),
            writer: parking_lot::Mutex::new(false),
        }
    }
}

impl<T> SubscriberList<T> {
    /// The current map, for one phase on this thread.
    ///
    /// A post loads it inside the record hold that wrote the value; loaded
    /// after the release, a late post can queue an older value behind a newer
    /// one. Every cycle thread runs [`crate::sync::prime_current_thread`]
    /// first, or its first load allocates.
    pub fn load(&self) -> RetireGuard<'_, SubscriberMap<T>> {
        self.map.load()
    }

    /// No map load: the cycle's "anyone watching" test. `true` means the map
    /// is empty; `false` does not mean it holds anyone.
    pub fn is_empty(&self) -> bool {
        self.count.load(Ordering::Acquire) == 0
    }

    /// C `db_event_enable`'s `ellAdd` onto `mlis` (`dbEvent.c:515`), behind
    /// `field`'s earlier monitors, then `seed` — EVENT_ADD's first value, read
    /// and queued inside one scan-lock hold (C `db_post_single_event`'s
    /// `dbScanLock`).
    ///
    /// The seed runs once `sub` is published and the writer mutex released.
    /// The publish alone orders nothing: the seed must read and queue under
    /// the hold a change takes, or a change between its read and its queue
    /// lands first and the seed's older value arrives last. C seeds before the
    /// enable (`camessage.c:1907`, `:1913`) and can lose a change.
    ///
    /// For an owner whose list is live for every add; one that can close it
    /// adds with [`Self::try_add`], which refuses as [`Closed`].
    pub fn add<R>(&self, field: u16, sub: Arc<T>, seed: impl FnOnce() -> R) -> R {
        self.try_add(field, sub, |_| Ok::<(), Closed>(()), seed)
            .expect("SubscriberList::add on a closed list")
    }

    /// [`Self::add`] behind `check`, which runs under the writer mutex on the
    /// current map after the closed test: the owner's own refusal (the
    /// monitor cap) is decided in the same writer section that would
    /// publish. A refusal publishes nothing, runs no seed, and drops `sub`
    /// after the release.
    pub fn try_add<R, E: From<Closed>>(
        &self,
        field: u16,
        sub: Arc<T>,
        check: impl FnOnce(&SubscriberMap<T>) -> Result<(), E>,
        seed: impl FnOnce() -> R,
    ) -> Result<R, E> {
        let w = self.writer.lock();
        if *w {
            return Err(Closed.into());
        }
        let cur = self.map.load();
        check(&cur)?;
        let at = cur.entries.partition_point(|(f, _)| *f <= field);
        let mut entries = Vec::with_capacity(cur.entries.len() + 1);
        entries.extend_from_slice(&cur.entries[..at]);
        entries.push((field, sub));
        entries.extend_from_slice(&cur.entries[at..]);
        drop(cur);
        let freed = self.publish(entries, 1, 0);
        drop(w);
        drop(freed);
        Ok(seed())
    }

    /// Close the list: remove every monitor and refuse every later add, in
    /// one writer section. The closing call gets the removed monitors, to
    /// drop once every lock is released; a later call gets `None`.
    pub fn close(&self) -> Option<Vec<Arc<T>>> {
        let mut w = self.writer.lock();
        if *w {
            return None;
        }
        *w = true;
        let cur = self.map.load();
        let gone: Vec<Arc<T>> = cur.entries.iter().map(|(_, s)| s.clone()).collect();
        drop(cur);
        let freed = (!gone.is_empty()).then(|| self.publish(Vec::new(), 0, gone.len()));
        drop(w);
        drop(freed);
        Some(gone)
    }

    /// Whether [`Self::close`] has run.
    pub fn is_closed(&self) -> bool {
        *self.writer.lock()
    }

    /// Remove every monitor `keep` refuses, returning them so their drops run
    /// after the writer mutex is released, as do the drops of the maps the
    /// store frees. No store when nothing goes.
    pub fn retain(&self, mut keep: impl FnMut(u16, &T) -> bool) -> Vec<Arc<T>> {
        let mut gone = Vec::new();
        let _freed = {
            let _w = self.writer.lock();
            let cur = self.map.load();
            let kept: Vec<bool> = cur.entries.iter().map(|(f, s)| keep(*f, s)).collect();
            if kept.iter().all(|k| *k) {
                return gone;
            }
            let mut entries = Vec::with_capacity(cur.entries.len());
            for (entry, kept) in cur.entries.iter().zip(kept) {
                if kept {
                    entries.push(entry.clone());
                } else {
                    gone.push(entry.1.clone());
                }
            }
            drop(cur);
            self.publish(entries, 0, gone.len())
        };
        gone
    }

    /// Store `entries` with the count raised by `added` before and lowered by
    /// `removed` after, which keeps it at or above the map's length.
    fn publish(
        &self,
        entries: Vec<(u16, Arc<T>)>,
        added: usize,
        removed: usize,
    ) -> crate::sync::Freed<SubscriberMap<T>> {
        self.count.fetch_add(added, Ordering::Release);
        #[cfg(test)]
        self.midpoint(true);
        let freed = self.map.store_deferred(Arc::new(SubscriberMap { entries }));
        #[cfg(test)]
        self.midpoint(false);
        self.count.fetch_sub(removed, Ordering::Release);
        freed
    }

    /// The count and the published map's length between `publish`'s steps,
    /// and a test's hook just before the store.
    #[cfg(test)]
    fn midpoint(&self, before_store: bool) {
        let seen = (self.count.load(Ordering::Acquire), self.map.load().len());
        subscriber_list_tests::MIDPOINTS.with(|m| m.borrow_mut().push(seen));
        if before_store && let Some(hook) = subscriber_list_tests::BEFORE_STORE.take() {
            hook();
        }
    }
}

#[cfg(test)]
mod subscriber_list_tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::thread::{self, ThreadId};

    type Drops = Arc<StdMutex<Vec<(u32, ThreadId)>>>;

    thread_local! {
        pub(super) static MIDPOINTS: std::cell::RefCell<Vec<(usize, usize)>> =
            const { std::cell::RefCell::new(Vec::new()) };
        pub(super) static BEFORE_STORE: std::cell::Cell<Option<Box<dyn FnOnce()>>> =
            const { std::cell::Cell::new(None) };
    }

    /// A record whose posts load the list inside the hold that wrote the
    /// value, and whose monitors each queue what they are sent.
    #[derive(Default)]
    struct Rec {
        hold: StdMutex<i32>,
        list: SubscriberList<StdMutex<Vec<i32>>>,
    }

    impl Rec {
        fn post(&self, v: i32) {
            let mut val = self.hold.lock().unwrap();
            *val = v;
            for q in self.list.load().of(0) {
                q.lock().unwrap().push(v);
            }
        }

        /// EVENT_ADD: attach a monitor and seed it inside one hold.
        fn event_add(&self) -> Arc<StdMutex<Vec<i32>>> {
            let q = Arc::<StdMutex<Vec<i32>>>::default();
            self.list.add(0, q.clone(), || {
                let val = self.hold.lock().unwrap();
                q.lock().unwrap().push(*val);
            });
            q
        }
    }

    /// The C-loss case: a post after the seed's read but before the monitor
    /// is published. Seeding after the publish reads it instead.
    #[test]
    fn a_post_racing_the_add_is_not_lost() {
        let rec = Arc::new(Rec::default());
        rec.post(1);
        let poster = rec.clone();
        BEFORE_STORE.set(Some(Box::new(move || poster.post(2))));
        let q = rec.event_add();
        assert_eq!(*q.lock().unwrap(), [2], "the monitor's last value is stale");
    }

    /// A post between the publish and the seed: delivered, and the seed
    /// repeats it — the at-most-twice bound. The writer mutex stays a leaf.
    #[test]
    fn a_post_between_the_publish_and_the_seed_is_delivered_at_most_twice() {
        let rec = Rec::default();
        rec.post(1);
        let q = Arc::<StdMutex<Vec<i32>>>::default();
        rec.list.add(0, q.clone(), || {
            assert!(
                !rec.list.writer.is_locked(),
                "the seed ran under the writer"
            );
            rec.post(2);
            let val = rec.hold.lock().unwrap();
            q.lock().unwrap().push(*val);
        });
        assert_eq!(*q.lock().unwrap(), [2, 2]);
    }

    /// A post after the seed: delivered once.
    #[test]
    fn a_post_after_the_seed_is_delivered_once() {
        let rec = Rec::default();
        rec.post(1);
        let q = rec.event_add();
        rec.post(2);
        assert_eq!(*q.lock().unwrap(), [1, 2]);
    }

    /// Between the count update and the store of every add and removal, the
    /// count is at or above the published map's length.
    #[test]
    fn the_count_never_falls_below_the_published_map() {
        let drops = Drops::default();
        let list = SubscriberList::default();
        for id in 0..3 {
            list.add(id as u16, probe(id, &drops), || ());
        }
        drop(list.retain(|f, _| f != 1));
        drop(list.retain(|_, _| false));
        let seen = MIDPOINTS.with(|m| m.take());
        assert_eq!(seen.len(), 10);
        assert!(seen.iter().all(|(count, len)| count >= len), "{seen:?}");
        assert!(list.is_empty() && list.load().is_empty());
    }

    /// Records its drop and the thread it ran on: the last holder of a
    /// removed monitor's `Arc`.
    struct Probe {
        id: u32,
        drops: Drops,
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            self.drops
                .lock()
                .unwrap()
                .push((self.id, thread::current().id()));
        }
    }

    fn probe(id: u32, drops: &Drops) -> Arc<Probe> {
        Arc::new(Probe {
            id,
            drops: drops.clone(),
        })
    }

    fn ids(map: &SubscriberMap<Probe>, field: u16) -> Vec<u32> {
        map.of(field).map(|p| p.id).collect()
    }

    #[test]
    fn empty_to_one_and_attach_order_within_a_field() {
        let drops = Drops::default();
        let list = SubscriberList::default();
        assert!(list.is_empty() && list.load().is_empty());
        list.add(3, probe(1, &drops), || ());
        assert!(!list.is_empty());
        assert_eq!(ids(&list.load(), 3), [1]);
        assert!(ids(&list.load(), 2).is_empty());
        list.add(3, probe(2, &drops), || ());
        list.add(1, probe(3, &drops), || ());
        list.add(5, probe(4, &drops), || ());
        let map = list.load();
        assert_eq!(ids(&map, 3), [1, 2]);
        let order: Vec<_> = map.iter().map(|(f, p)| (f, p.id)).collect();
        assert_eq!(order, [(1, 3), (3, 1), (3, 2), (5, 4)]);
    }

    #[test]
    fn last_removed_empties_and_a_no_op_retain_stores_nothing() {
        let drops = Drops::default();
        let list = SubscriberList::default();
        list.add(0, probe(1, &drops), || ());
        MIDPOINTS.with(|m| m.take());
        assert!(list.retain(|_, _| true).is_empty());
        assert!(
            MIDPOINTS.with(|m| m.take()).is_empty(),
            "the no-op retain stored"
        );
        list.add(0, probe(2, &drops), || ());
        let gone = list.retain(|_, _| false);
        assert_eq!(gone.iter().map(|p| p.id).collect::<Vec<_>>(), [1, 2]);
        assert!(list.is_empty() && list.load().is_empty());
    }

    /// A reader's guard keeps the removed monitor visible; its drop frees
    /// nothing, and the next writer frees the old map on its own thread.
    #[test]
    fn remove_during_a_load() {
        let drops = Drops::default();
        let list = SubscriberList::default();
        list.add(7, probe(1, &drops), || ());
        let (to_writer, from_reader) = std::sync::mpsc::channel();
        let (to_reader, from_writer) = std::sync::mpsc::channel::<()>();
        let reader = thread::scope(|s| {
            let list = &list;
            let reader = s.spawn(move || {
                let guard = list.load();
                to_writer.send(()).unwrap();
                from_writer.recv().unwrap();
                assert_eq!(ids(&guard, 7), [1], "the guard's map changed");
                drop(guard);
                to_writer.send(()).unwrap();
                thread::current().id()
            });
            from_reader.recv().unwrap();
            let gone = list.retain(|_, p| p.id != 1);
            assert!(list.load().is_empty());
            drop(gone);
            to_reader.send(()).unwrap();
            from_reader.recv().unwrap();
            reader.join().unwrap()
        });
        let held = drops.lock().unwrap().len();
        list.add(7, probe(2, &drops), || ());
        let drops = drops.lock().unwrap().clone();
        assert!(drops.len() > held, "the next writer freed nothing");
        assert!(drops.iter().all(|(_, t)| *t != reader), "{drops:?}");
    }

    /// Two writers' copy-modify-stores serialise: no add is lost, and each
    /// writer's field keeps its attach order.
    #[test]
    fn two_writers_lose_no_add() {
        let drops = Drops::default();
        let list = SubscriberList::default();
        thread::scope(|s| {
            for field in [1u16, 2] {
                let (list, drops) = (&list, &drops);
                s.spawn(move || {
                    for id in 0..300 {
                        list.add(field, probe(id, drops), || ());
                    }
                });
            }
        });
        let map = list.load();
        assert_eq!(map.len(), 600);
        for field in [1, 2] {
            assert_eq!(ids(&map, field), (0..300).collect::<Vec<_>>());
        }
        drop(map);
        assert_eq!(list.retain(|f, _| f == 2).len(), 300);
        assert!(!list.is_empty());
        assert_eq!(list.retain(|_, _| false).len(), 300);
        assert!(list.is_empty());
    }

    /// A removed monitor's last reference sits in the map the removal
    /// replaced; the store that frees that map drops it after the writer
    /// mutex is released, from `add` and from `retain` alike.
    #[test]
    fn a_freed_map_drops_after_the_writer_mutex_is_released() {
        struct Chk;
        static LIST: std::sync::OnceLock<&'static SubscriberList<Chk>> = std::sync::OnceLock::new();
        static LOCKED_AT_DROP: StdMutex<Vec<bool>> = StdMutex::new(Vec::new());
        impl Drop for Chk {
            fn drop(&mut self) {
                if let Some(list) = LIST.get() {
                    LOCKED_AT_DROP.lock().unwrap().push(list.writer.is_locked());
                }
            }
        }
        let list = *LIST.get_or_init(|| Box::leak(Box::default()));
        // add's store frees the map that held the first.
        list.add(0, Arc::new(Chk), || ());
        drop(list.retain(|_, _| false));
        list.add(0, Arc::new(Chk), || ());
        assert_eq!(LOCKED_AT_DROP.lock().unwrap().len(), 1);
        // A guard keeps the second's map past add's sweep, so retain frees it.
        let guard = list.load();
        drop(list.retain(|_, _| false));
        list.add(0, Arc::new(Chk), || ());
        drop(guard);
        drop(list.retain(|_, _| false));
        assert_eq!(*LOCKED_AT_DROP.lock().unwrap(), [false, false]);
    }

    #[test]
    fn an_add_after_close_is_refused_and_publishes_nothing() {
        let list = SubscriberList::<u32>::default();
        assert_eq!(list.close().map(|g| g.len()), Some(0));
        assert!(list.is_closed());
        assert!(list.close().is_none(), "only the first close closes");
        MIDPOINTS.with(|m| m.take());
        let mut seeded = false;
        let added = list.try_add(0, Arc::new(1), |_| Ok::<(), Closed>(()), || seeded = true);
        assert_eq!(added, Err(Closed));
        // The owner's check is not consulted: the list refuses first.
        let checked = list.try_add(
            0,
            Arc::new(2),
            |_| -> Result<(), Closed> { panic!() },
            || (),
        );
        assert_eq!(checked, Err(Closed));
        assert!(!seeded, "a refused add ran its seed");
        assert!(
            MIDPOINTS.with(|m| m.take()).is_empty(),
            "a refused add stored"
        );
        assert!(list.is_empty() && list.load().is_empty());
    }

    #[test]
    fn an_add_before_close_is_in_what_close_returns() {
        let list = SubscriberList::<u32>::default();
        list.add(0, Arc::new(1), || ());
        let gone = list.close().expect("the first close");
        assert_eq!(gone.iter().map(|g| **g).collect::<Vec<_>>(), [1]);
        assert!(list.is_empty() && list.load().is_empty());
    }

    /// A close started while an add is in its writer section waits for the
    /// publish and returns the add: no live entry survives a close.
    #[test]
    fn a_close_racing_an_add_in_flight_returns_the_add() {
        let list = SubscriberList::<u32>::default();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let gone = thread::scope(|s| {
            let mut closer = None;
            let added = list.try_add(
                0,
                Arc::new(7),
                |_| -> Result<(), Closed> {
                    let list = &list;
                    let done_tx = done_tx.clone();
                    closer = Some(s.spawn(move || {
                        let gone = list.close();
                        done_tx.send(()).unwrap();
                        gone
                    }));
                    let raced = done_rx
                        .recv_timeout(std::time::Duration::from_millis(200))
                        .is_ok();
                    assert!(!raced, "the close finished inside the add's section");
                    Ok(())
                },
                || (),
            );
            assert_eq!(added, Ok(()));
            closer.unwrap().join().unwrap().expect("the first close")
        });
        assert_eq!(gone.iter().map(|g| **g).collect::<Vec<_>>(), [7]);
        assert!(list.load().is_empty(), "a live entry after close");
    }

    /// Unordered: every add is either refused or in close's set.
    #[test]
    fn a_close_racing_adds_leaves_none_live() {
        for _ in 0..300 {
            let list = SubscriberList::<u32>::default();
            let (added, gone) = thread::scope(|s| {
                let adder =
                    s.spawn(|| list.try_add(0, Arc::new(9), |_| Ok::<(), Closed>(()), || ()));
                let gone = list.close().expect("the first close");
                (adder.join().unwrap(), gone)
            });
            assert!(list.load().is_empty(), "a live entry after close");
            assert_eq!(added.is_ok(), gone.len() == 1, "{added:?} {gone:?}");
        }
    }

    /// The close's removed monitors reach the caller alive, and the map the
    /// close frees — the last owner of a monitor an earlier retain removed —
    /// drops after the writer mutex is released.
    #[test]
    fn close_drops_after_the_writer_mutex_is_released() {
        struct Chk(u16);
        static LIST: std::sync::OnceLock<&'static SubscriberList<Chk>> = std::sync::OnceLock::new();
        static DROPS: StdMutex<Vec<(u16, bool)>> = StdMutex::new(Vec::new());
        impl Drop for Chk {
            fn drop(&mut self) {
                if let Some(list) = LIST.get() {
                    DROPS
                        .lock()
                        .unwrap()
                        .push((self.0, list.writer.is_locked()));
                }
            }
        }
        let list = *LIST.get_or_init(|| Box::leak(Box::default()));
        list.add(0, Arc::new(Chk(1)), || ());
        list.add(0, Arc::new(Chk(2)), || ());
        // 1's last owner is now the map this retain displaced.
        drop(list.retain(|_, c| c.0 != 1));
        assert!(DROPS.lock().unwrap().is_empty());
        let gone = list.close().expect("the first close");
        assert_eq!(*DROPS.lock().unwrap(), [(1, false)], "close freed the map");
        assert_eq!(gone.iter().map(|g| g.0).collect::<Vec<_>>(), [2]);
        drop(gone);
        assert_eq!(
            *DROPS.lock().unwrap(),
            [(1, false)],
            "2 is still in a retired map"
        );
    }

    /// CANCEL / circuit close: the removed monitor's drop is its end of
    /// stream, so it reaches the caller alive and neither it nor the freed
    /// map drops under the writer mutex.
    #[test]
    fn retain_drops_after_the_writer_mutex_is_released() {
        struct Chk(u16);
        static LIST: std::sync::OnceLock<&'static SubscriberList<Chk>> = std::sync::OnceLock::new();
        static DROPS: StdMutex<Vec<(u16, bool)>> = StdMutex::new(Vec::new());
        impl Drop for Chk {
            fn drop(&mut self) {
                if let Some(list) = LIST.get() {
                    DROPS
                        .lock()
                        .unwrap()
                        .push((self.0, list.writer.is_locked()));
                }
            }
        }
        let list = *LIST.get_or_init(|| Box::leak(Box::default()));
        list.add(0, Arc::new(Chk(1)), || ());
        list.add(0, Arc::new(Chk(2)), || ());
        let gone = list.retain(|_, c| c.0 != 1);
        assert_eq!(gone.iter().map(|g| g.0).collect::<Vec<_>>(), [1]);
        assert!(DROPS.lock().unwrap().is_empty(), "dropped before return");
        drop(gone);
        // A removed monitor's last owner can be a map version the next
        // store frees.
        drop(list.retain(|_, _| false));
        list.add(0, Arc::new(Chk(3)), || ());
        assert_eq!(*DROPS.lock().unwrap(), [(1, false), (2, false)]);
    }

    /// A reader loading and walking the map while a writer adds and removes
    /// never frees a map version: every free lands on the writer's thread.
    #[test]
    fn a_racing_reader_never_frees_a_map() {
        let drops = Drops::default();
        let list = SubscriberList::default();
        let stop = AtomicBool::new(false);
        let (reader, writer) = thread::scope(|s| {
            let (list, stop, drops) = (&list, &stop, &drops);
            let reader = s.spawn(move || {
                crate::sync::prime_current_thread();
                let mut walked = 0usize;
                while !stop.load(Ordering::Acquire) {
                    let map = list.load();
                    walked += map.of(4).count();
                }
                std::hint::black_box(walked);
                thread::current().id()
            });
            let writer = s.spawn(move || {
                for id in 0..2000 {
                    list.add(4, probe(id, drops), || ());
                    if id % 2 == 1 {
                        drop(list.retain(|_, p| p.id != id - 1));
                    }
                }
                stop.store(true, Ordering::Release);
                thread::current().id()
            });
            (reader.join().unwrap(), writer.join().unwrap())
        });
        let drops = drops.lock().unwrap().clone();
        assert!(drops.iter().any(|(_, t)| *t == writer));
        assert!(drops.iter().all(|(_, t)| *t != reader), "a reader freed");
    }
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
    /// A sink closed while something still holds it — a retired
    /// `SubscriberList` map — refuses every later post, and its reader ends
    /// after what was queued before the close.
    #[test]
    fn a_closed_sink_refuses_posts_and_ends_after_its_queue() {
        let user = EventUser::new();
        let (sink, mut reader) = attach(&user, 1, LogKind::Val);
        assert!(matches!(sink.post(ev(1)), PostOutcome::Appended { .. }));
        sink.close();
        assert_eq!(sink.post(ev(2)), PostOutcome::Closed);
        assert!(
            reader.try_recv().is_ok(),
            "the entry queued before the close"
        );
        assert!(matches!(reader.try_recv(), Err(TryRecvError::Disconnected)));
        drop(sink);
        assert!(matches!(reader.try_recv(), Err(TryRecvError::Disconnected)));
    }

    /// A closed producer with its reader still attached reads closed, so the
    /// post walk's `reap_closed` drops the row rather than keep posting into
    /// a refused slot.
    #[epics_macros_rs::epics_test]
    async fn a_producer_closed_sink_reads_closed_while_its_reader_lives() {
        let user = EventUser::new();
        let (sink, _reader) = attach(&user, 1, LogKind::Val);
        assert!(!sink.is_closed());
        sink.close();
        assert!(sink.is_closed());
        assert_eq!(sink.post(ev(1)), PostOutcome::Closed);
    }

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

    /// C `eventsRemaining`: an entry is told whether its queue holds another,
    /// so the last of a drained queue reads `false`.
    #[test]
    fn the_last_entry_of_a_drained_queue_reports_none_remaining() {
        let user = EventUser::new();
        let (a, _ra) = attach(&user, 1, LogKind::Val);
        let (b, _rb) = attach(&user, 2, LogKind::Val);
        a.post(ev(1));
        b.post(ev(2));
        let mut got = Vec::new();
        while let std::task::Poll::Ready(CircuitEvent::Event {
            event, remaining, ..
        }) = circuit_now(&user)
        {
            got.push((val(&event), remaining));
        }
        assert_eq!(got, [(1, true), (2, false)]);
        b.post(ev(3));
        assert!(matches!(
            circuit_now(&user),
            std::task::Poll::Ready(CircuitEvent::Event {
                remaining: false,
                ..
            })
        ));
    }

    /// C reads `evque[getix]` before the dead-entry skip (`dbEvent.c:965`), so
    /// a cancelled monitor's entry next in the ring still counts as remaining.
    #[test]
    fn a_cancelled_monitors_next_entry_counts_as_remaining() {
        let user = EventUser::new();
        let (a, _ra) = attach(&user, 1, LogKind::Val);
        let (b, rb) = attach(&user, 2, LogKind::Val);
        a.post(ev(1));
        b.post(ev(2));
        drop(rb);
        assert!(matches!(
            circuit_now(&user),
            std::task::Poll::Ready(CircuitEvent::Event {
                sid: 1,
                remaining: true,
                ..
            })
        ));
        assert!(circuit_now(&user).is_pending());
    }

    /// Records whether `ppendsem` was held when it was woken.
    struct HeldAtWake(Arc<FlowCtrl>, std::sync::atomic::AtomicBool);

    impl std::task::Wake for HeldAtWake {
        fn wake(self: Arc<Self>) {
            let held = self.0.ppendsem.try_lock().is_none();
            self.1.store(held, Ordering::SeqCst);
        }
    }

    /// The wake runs with `ppendsem` released, so the woken consumer's park
    /// does not contend with the waker's release.
    #[test]
    fn wake_readers_wakes_after_releasing_ppendsem() {
        let user = EventUser::new();
        let (sink, mut reader) = attach(&user, 7, LogKind::Val);
        let probe = Arc::new(HeldAtWake(
            user.flow.clone(),
            std::sync::atomic::AtomicBool::new(true),
        ));
        let waker = std::task::Waker::from(probe.clone());
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(reader.poll_recv(&mut cx).is_pending());
        sink.post(ev(1));
        assert!(!probe.1.load(Ordering::SeqCst), "woken under ppendsem");
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
    /// Three laps of the ring, a sibling's entry taken ahead of the older one
    /// each time, keep both monitors' order.
    #[test]
    fn entries_keep_their_order_across_the_ring_wrap() {
        let user = EventUser::new();
        let (sink_a, mut reader_a) = attach(&user, 1, LogKind::Val);
        let (sink_b, mut reader_b) = attach(&user, 2, LogKind::Val);
        let size = reader_a.queue().report(reader_a.id()).ring_size;
        for v in 0..3 * size as i32 {
            sink_a.post(ev(v));
            sink_a.post(ev(v + 1));
            sink_b.post(ev(-v));
            assert_eq!(val(&reader_b.try_recv().unwrap()), -v);
            assert_eq!(val(&reader_a.try_recv().unwrap()), v);
            assert_eq!(val(&reader_a.try_recv().unwrap()), v + 1);
        }
        assert_eq!(reader_a.queue().report(reader_a.id()).ring_space, size);
    }

    /// Run `f` on its own thread; a deadlock fails the test instead of
    /// hanging it.
    fn finishes(f: impl FnOnce() + Send + 'static) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            f();
            let _ = tx.send(());
        });
        rx.recv_timeout(std::time::Duration::from_secs(10))
            .expect("deadlocked: user state dropped under the EvQue lock");
    }

    /// A PV whose monitor context is its own last owner, and a second
    /// monitor of it on the same queue whose sink closes when the PV drops.
    fn pv_owned_by_its_slot(user: &EventUser) -> (EventReader, EventReader) {
        use crate::server::pv::ProcessVariable;
        use crate::types::DbFieldType;
        let pv = Arc::new(ProcessVariable::new(
            "EVQ:LAST".into(),
            EpicsValue::Double(1.0),
        ));
        let mask = (EventMask::VALUE | EventMask::LOG).bits();
        let ctx: DeliveryContext = Arc::new(pv.clone());
        let owner = pv
            .add_subscriber_on(user, 1, DbFieldType::Double, mask, &ctx)
            .unwrap();
        let other = pv
            .add_subscriber_on(user, 2, DbFieldType::Double, mask, no_context())
            .unwrap();
        (owner, other)
    }

    /// The circuit consumer drops a cancelled monitor's last entry and frees
    /// its slot, whose context held the last PV reference (`poll_oldest` ->
    /// `pop` -> `release` -> `free_slot`).
    #[test]
    fn a_slot_freed_by_the_circuit_drops_its_context_after_the_lock() {
        let user = EventUser::new();
        let (owner, other) = pv_owned_by_its_slot(&user);
        let pv = user.context(owner.id()).unwrap();
        pv.downcast_ref::<Arc<crate::server::pv::ProcessVariable>>()
            .unwrap()
            .set(EpicsValue::Double(2.0));
        drop(pv);
        assert_eq!(owner.npend(), 1);
        drop(owner);
        finishes(move || {
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            while user.poll_next(&mut cx).is_ready() {}
            drop(other);
        });
    }

    /// A monitor cancelled with nothing queued frees its slot at once
    /// (`close_reader` -> `free_slot`).
    #[test]
    fn a_slot_freed_by_its_reader_drops_its_context_after_the_lock() {
        let user = EventUser::new();
        let (owner, other) = pv_owned_by_its_slot(&user);
        assert_eq!(owner.npend(), 0);
        finishes(move || {
            drop(owner);
            drop(other);
            drop(user);
        });
    }

    /// A parked waker displaced by the next park can be its task's last
    /// reference, whose drop drops the task's readers.
    #[test]
    fn a_displaced_waker_drops_after_the_lock() {
        struct HoldsReader(std::sync::Mutex<Option<EventReader>>);
        impl std::task::Wake for HoldsReader {
            fn wake(self: Arc<Self>) {
                self.0.lock().unwrap().take();
            }
        }
        let user = EventUser::new();
        let (_sink, mut reader) = attach(&user, 1, LogKind::Val);
        let (_held_sink, held) = attach(&user, 2, LogKind::Val);
        let first =
            std::task::Waker::from(Arc::new(HoldsReader(std::sync::Mutex::new(Some(held)))));
        assert!(
            reader
                .poll_recv(&mut std::task::Context::from_waker(&first))
                .is_pending()
        );
        drop(first);
        finishes(move || {
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(reader.poll_recv(&mut cx).is_pending());
        });
    }

    /// Records whether the queue lock was held when it was woken or dropped.
    struct HeldAtDrop(Arc<EvQue>, Arc<std::sync::atomic::AtomicBool>);

    impl HeldAtDrop {
        fn record(&self) {
            self.1
                .store(self.0.inner.try_lock().is_none(), Ordering::SeqCst);
        }
    }

    impl std::task::Wake for HeldAtDrop {
        fn wake(self: Arc<Self>) {
            self.record();
        }
    }

    impl Drop for HeldAtDrop {
        fn drop(&mut self) {
            self.record();
        }
    }

    /// The fast form of the test above: the displaced waker drops with the
    /// queue lock free, so a drop under it fails instead of deadlocking.
    #[test]
    fn a_displaced_waker_drops_with_the_queue_unlocked() {
        let user = EventUser::new();
        let (_sink, mut reader) = attach(&user, 1, LogKind::Val);
        let held = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let first = std::task::Waker::from(Arc::new(HeldAtDrop(reader.queue(), held.clone())));
        assert!(
            reader
                .poll_recv(&mut std::task::Context::from_waker(&first))
                .is_pending()
        );
        drop(first);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(reader.poll_recv(&mut cx).is_pending());
        assert!(
            !held.load(Ordering::SeqCst),
            "displaced waker dropped under the lock"
        );
    }

    #[test]
    fn only_the_post_that_fills_an_empty_ring_wakes() {
        let user = EventUser::new();
        let (sink, _reader) = attach(&user, 1, LogKind::Val);
        let (counter, waker) = count_waker();
        let cx = std::task::Context::from_waker(&waker);
        sink.que.lock_retiring().park(&cx);
        sink.post(ev(1));
        assert_eq!(
            counter.0.load(Ordering::SeqCst),
            1,
            "empty ring: first post wakes"
        );
        for v in 2..10 {
            sink.que.lock_retiring().park(&cx);
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
    /// A log built in its ring slot from the poster's parts is the log a
    /// built event would have queued, on append, replace and collapse.
    #[test]
    fn parts_land_as_the_log_they_make() {
        use crate::server::pv::{LogHead, NativeValue};
        let head = LogHead::of(&Snapshot::new(
            EpicsValue::Long(0),
            3,
            2,
            std::time::SystemTime::UNIX_EPOCH,
        ));
        let parts = |v: i32, mask| LogParts {
            head: &head,
            value: LogData::Val(NativeValue::Long(v)),
            origin: 9,
            mask,
        };
        for kind in [LogKind::Val, LogKind::Ref] {
            let taken = |by_parts: bool| {
                let user = EventUser::new();
                user.flow_ctrl_on();
                let (sink, mut reader) = attach(&user, 1, kind);
                let mut outcomes = Vec::new();
                for (v, mask) in [(1, EventMask::VALUE), (2, EventMask::LOG)] {
                    let mut burst = PostBurst::new();
                    outcomes.push(if by_parts {
                        sink.post_parts(&mut burst, parts(v, mask))
                    } else {
                        sink.post_in(&mut burst, MonitorEvent::of_log(parts(v, mask).into_log()))
                    });
                }
                user.flow_ctrl_off();
                (outcomes, format!("{:?}", reader.try_recv().unwrap()))
            };
            let (outcomes, log) = taken(true);
            assert_eq!((outcomes.clone(), log.clone()), taken(false), "{kind:?}");
            assert_eq!(
                outcomes[1],
                match kind {
                    LogKind::Val => PostOutcome::Replaced,
                    LogKind::Ref => PostOutcome::Collapsed,
                },
            );
        }
    }

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

    fn circuit_now_with(
        user: &EventUser,
        waker: &std::task::Waker,
    ) -> std::task::Poll<CircuitEvent> {
        user.poll_next(&mut std::task::Context::from_waker(waker))
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

    /// What a lock displaces is released once the lock is let go, not
    /// leaked: a freed slot's context, whether the drain of its last entry or
    /// its reader's drop freed it, and a parked waker the next park replaced.
    #[epics_macros_rs::epics_test]
    async fn a_displaced_context_or_waker_is_released() {
        let user = EventUser::new();
        let ctx: DeliveryContext = Arc::new(7u32);
        // Freed by the drain of its last entry.
        let (a, ra) = attach_with(&user, 1, LogKind::Val, &ctx);
        a.post(ev(10));
        drop(ra);
        drop(a);
        assert!(circuit_now(&user).is_pending());
        assert_eq!(
            Arc::strong_count(&ctx),
            1,
            "a drained slot kept its context"
        );
        // Freed by its reader's drop with nothing queued.
        let (b, rb) = attach_with(&user, 2, LogKind::Val, &ctx);
        drop(rb);
        drop(b);
        assert_eq!(
            Arc::strong_count(&ctx),
            1,
            "a cancelled slot kept its context"
        );
        // A parked waker replaced by the next park.
        let (counter, waker) = count_waker();
        assert!(circuit_now_with(&user, &waker).is_pending());
        drop(waker);
        assert!(circuit_now(&user).is_pending());
        assert_eq!(Arc::strong_count(&counter), 1, "a displaced waker leaked");
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

    /// `unreported_ends` counts exactly the monitors `take_ended` would find,
    /// on every path a slot can leave by: reported, freed by its reader's
    /// drop with nothing queued, or freed by the drain of its last entry.
    #[epics_macros_rs::epics_test]
    async fn unreported_ends_counts_every_ended_slot_once() {
        fn check(que: &EvQue) -> usize {
            let q = que.lock();
            let scanned = q
                .subs
                .iter()
                .flatten()
                .filter(|s| s.producer_gone() && !s.end_reported)
                .count();
            assert_eq!(q.unreported_ends, scanned);
            scanned
        }
        let user = EventUser::new();
        // Reported after its backlog.
        let (a, ra) = attach(&user, 1, LogKind::Val);
        let que = ra.queue();
        a.post(ev(10));
        drop(a);
        assert_eq!(check(&que), 1);
        assert_eq!(circuit_event(&user), (1, 10));
        assert!(matches!(
            circuit_now(&user),
            std::task::Poll::Ready(CircuitEvent::Ended(_))
        ));
        assert_eq!(check(&que), 0);
        drop(ra);
        // Freed by the reader's drop with nothing queued.
        let (b, rb) = attach(&user, 2, LogKind::Val);
        drop(b);
        assert_eq!(check(&que), 1);
        drop(rb);
        assert_eq!(check(&que), 0);
        // Freed by the drain of its last entry, never reported.
        let (c, rc) = attach(&user, 3, LogKind::Val);
        c.post(ev(30));
        drop(c);
        drop(rc);
        assert_eq!(check(&que), 1);
        assert!(circuit_now(&user).is_pending());
        assert_eq!(check(&que), 0);
        // Reader first, entry queued: the producer's close still counts.
        let (d, rd) = attach(&user, 4, LogKind::Val);
        d.post(ev(40));
        drop(rd);
        drop(d);
        assert_eq!(check(&que), 1);
        assert!(circuit_now(&user).is_pending());
        assert_eq!(check(&que), 0);
        assert_eq!(que.quota(), 0);
    }

    /// [`QueInner::advance`] leaves the retired entry in its slot for the
    /// caller: every path out of the head — delivered to the circuit, dropped
    /// for a cancelled reader, taken by a monitor's own reader — empties that
    /// slot, so the ring holds exactly `used` entries.
    #[epics_macros_rs::epics_test]
    async fn every_retired_head_slot_is_emptied() {
        fn occupied(que: &EvQue) -> (usize, usize) {
            let q = que.lock();
            (q.ring.iter().filter(|e| e.is_some()).count(), q.used)
        }
        let user = EventUser::new();
        let (a, ra) = attach(&user, 1, LogKind::Val);
        let (b, rb) = attach(&user, 2, LogKind::Val);
        let (c, mut rc) = attach(&user, 3, LogKind::Val);
        let que = ra.queue();
        b.post(ev(20));
        a.post(ev(10));
        c.post(ev(30));
        a.post(ev(11));
        drop(rb);
        // A cancelled head entry dropped on the way to a monitor's own take.
        assert_eq!(val(&rc.try_recv().unwrap()), 30);
        assert_eq!(occupied(&que), (2, 2));
        assert_eq!(circuit_event(&user), (1, 10));
        assert_eq!(occupied(&que), (1, 1));
        b.post(ev(21));
        let (d, rd) = attach(&user, 4, LogKind::Val);
        d.post(ev(40));
        drop(rd);
        // A cancelled entry behind a live one, dropped on the circuit's drain.
        assert_eq!(circuit_event(&user), (1, 11));
        assert!(circuit_now(&user).is_pending());
        assert_eq!(occupied(&que), (0, 0));
        drop((a, c, d, ra, rc));
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

    /// A consumer parked before the quota chained a second queue is not woken
    /// by the chaining (`db_add_event` `dbEvent.c:426-503`) but by the first
    /// post on the new queue, and then drains `firstque` before `nextque`
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
        assert_eq!(
            count.0.load(Ordering::SeqCst),
            woken,
            "chaining a queue does not wake the parked consumer"
        );
        late.post(ev(99));
        assert_eq!(
            count.0.load(Ordering::SeqCst),
            woken + 1,
            "the first post on the chained queue wakes it"
        );
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

    /// An empty [`Retired`] — every lock that displaced nothing — is one
    /// pointer, so its drop is a null test, not a call.
    #[test]
    fn an_empty_retired_is_one_pointer() {
        assert_eq!(std::mem::size_of::<Retired>(), 8);
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
        user.ques.lock().ques[0].lock().free.len()
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
