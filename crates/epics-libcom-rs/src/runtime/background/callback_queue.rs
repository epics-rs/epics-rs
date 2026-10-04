//! The callback bands' queue: a lock-free submission path plus the
//! park/unpark registry the bands wake their workers through.
//!
//! # Why there is no lock here
//!
//! A callback band is crossed by every priority in the IOC: `scanIoRequest`
//! from a device-support thread, the delayed timer, monitor tails, and the
//! band's own workers at `epicsThreadPriorityScanHigh + 1`. A mutex on the
//! submission path makes the band a priority-inversion site — a low-priority
//! requester holding the lock blocks a high-priority one for as long as the
//! holder is off CPU, which on a non-RT kernel is unbounded and on an RT
//! kernel is bounded only once the lock carries `PTHREAD_PRIO_INHERIT`. C
//! closes this with inheritance (`epicsSpin` is a PI `pthread_mutex` on Linux
//! whenever POSIX thread priority scheduling is available, `osdSpin.c:126`),
//! which bounds the inversion by boosting whoever holds the lock.
//!
//! This queue removes the holder from that path instead. A requester takes a
//! slot and links it into a LIFO inbox — two CAS, owning nothing in between —
//! so a requester descheduled anywhere in it delays nobody and there is no
//! priority to inherit.
//!
//! # Inbox and ready, not one FIFO
//!
//! Two stacks, following the shape epics-base PR #996 arrived at by
//! measurement:
//!
//! - `inbox` — where requesters push, newest first.
//! - `ready` — where workers pop, one CAS per entry.
//!
//! A request costs the slot CAS, the publish CAS, and a `fetch_add` on the
//! band's `used` statistic — the profile #996 measured, with the difference
//! that the slot CAS is also the capacity check. The bound lives in the slot
//! supply (see [`Pool`]), so no requester CASes a counter to find out whether
//! there was room, and no counter can disagree with the queue about it.
//!
//! A worker pops `ready`. Finding it empty, it takes the *whole* inbox in one
//! CAS, reverses that chain into submission order, keeps the oldest entry for
//! itself and publishes the rest as `ready` with one more CAS.
//!
//! # Order
//!
//! A band with one worker — `callbackThreadsDefault` (`callback.c:66`) — is
//! strictly FIFO: it is the only thread that refills `ready`, so every batch
//! it publishes lands on an empty root, and a batch is in submission order.
//!
//! Above one worker, two workers can each be holding a batch, and the second
//! to publish prepends onto the first's remainder: its entries run ahead of
//! entries submitted before them. Only cross-batch order is affected, and only
//! at a worker count where dequeue order decides nothing anyway, since the
//! workers then run their callbacks concurrently.
//!
//! An earlier version closed that window by having a worker claim the `ready`
//! root before touching the inbox, which made the band strictly FIFO at any
//! worker count. It was withdrawn: a claimed root is a window in which one
//! thread's preemption stops every other thread that needs the role, for the
//! full length of the preemption. Measured in C against a 300 µs hog, a
//! requester's p99 goes 67 µs to 271 µs and its over-100 µs count 42 to 1637;
//! against a 5 ms hog, 117 requests in a run stall the whole 4 ms cap, where
//! the version without the role stalls none. It also cost 13% of the
//! throughput at one worker — the configuration the claim was buying nothing
//! in, since one worker can never contend for the role.
//!
//! Two properties this buys over a single Michael–Scott FIFO, which was
//! implemented first and withdrawn:
//!
//! - **One CAS to publish, not two.** MS needs a `next` link CAS plus a
//!   `tail` swing on top of taking the node, and the requester path is the
//!   band's only hot path, so every request pays for both.
//! - **The requester's line is not the worker's line.** MS keeps `head` and
//!   `tail` on the same node whenever the queue is short — the IOC's normal
//!   state — so a requester and a worker contend on one cache line however
//!   carefully the two roots are separated. Here they contend only while a
//!   batch is being handed over.
//!
//! It also needs strictly less machinery: a stack pop owns the node it
//! unlinked, so the payload can live in the node. MS has to read the payload
//! of the node it is *about to* unlink, before the CAS that gives it
//! ownership, which forces the payload into a second arena with a lifetime of
//! its own.
//!
//! What a queue shape does not answer is when to wake a sleeping worker; that
//! is [`Parking`], and it is the same either way.
//!
//! # Index arena, not raw pointers
//!
//! Nodes live in a chunked arena and are addressed by `u32` index. An index
//! read out of a recycled node is a *stale index*, never a dangling pointer:
//! the chunk it names is still mapped (chunks are freed only when the band is
//! dropped), so resolving it is defined, and the CAS that would have acted on
//! it fails because every location carries a tag bumped on every store. That
//! is the construction PR #996 uses for its free list (`CB_PACK(index, tag)`),
//! applied to both stacks as well.
//!
//! # What is lock-free and what is not
//!
//! Both sides are lock-free: there is no state a thread can be preempted in
//! that blocks another thread. A worker refilling `ready` holds nothing — it
//! owns the batch it took out of the inbox, and a worker preempted mid-refill
//! leaves the other workers free to take the inbox themselves and to pop
//! whatever is already in `ready`. That is the property the withdrawn
//! root-claiming version gave up, and the reason it was withdrawn.
//!
//! An entry becomes some worker's property only when that worker pops it, so a
//! callback that blocks strands nothing: everything behind it is still in
//! `ready`, reachable by every other worker of the band
//! (`a_blocked_callback_does_not_strand_its_neighbours`).

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};

use crate::runtime::sync::{Event, EventWaiter, Signalled};

/// The empty-stack index. No arena can hold this many elements — [`MAX_CHUNKS`]
/// stops 64 short of it — so it cannot collide with a real node.
const IDX_NONE: u32 = u32::MAX;

/// Pack an arena index and its location's ABA tag into one word, so a CAS
/// moves both at once. 32 bits of index, 32 of tag — exactly #996's split on a
/// 64-bit host (`CB_IDX_BITS`).
#[inline]
fn pack(idx: u32, tag: u32) -> u64 {
    ((tag as u64) << 32) | idx as u64
}

#[inline]
fn idx_of(v: u64) -> u32 {
    v as u32
}

#[inline]
fn tag_of(v: u64) -> u32 {
    (v >> 32) as u32
}

/// The value to store into a location currently holding `prev`, pointing it at
/// `idx`. Every store to a tagged location goes through this, which is what
/// makes a stale read of that location impossible to confuse with a current
/// one.
#[inline]
fn bump(prev: u64, idx: u32) -> u64 {
    pack(idx, tag_of(prev).wrapping_add(1))
}

/// Elements of the first chunk. Chunk `c` holds `CHUNK0 << c`, so the arena
/// reaches a given size in a logarithmic number of allocations and the index
/// of an element resolves with a `leading_zeros`.
const CHUNK0: usize = 64;

/// `CHUNK0 * (2^26 - 1)` is just over `u32::MAX`, so the index space is the
/// binding limit, not the table.
const MAX_CHUNKS: usize = 26;

/// Chunk holding global index `g`, and the offset within it.
#[inline]
fn locate(g: u32) -> (usize, usize) {
    let t = g as usize / CHUNK0 + 1;
    let c = (usize::BITS - 1 - t.leading_zeros()) as usize;
    (c, g as usize - CHUNK0 * ((1usize << c) - 1))
}

/// Elements in chunk `c`.
#[inline]
fn chunk_len(c: usize) -> usize {
    CHUNK0 << c
}

/// Global index of chunk `c`'s first element — equivalently, the number of
/// elements in all the chunks before it.
#[inline]
fn chunk_base(c: usize) -> usize {
    CHUNK0 * ((1usize << c) - 1)
}

/// One queued entry, and the tagged link that threads it onto whichever stack
/// holds it — `inbox`, `ready`, or the free list. Reuse therefore bumps the
/// same tag a stale popper would compare against.
struct Node<T> {
    link: AtomicU64,
    /// Written by the pusher before the node is linked, taken by the one
    /// worker whose pop CAS unlinked it. Nobody else can reach it in between.
    value: UnsafeCell<Option<T>>,
}

impl<T> Default for Node<T> {
    fn default() -> Self {
        Node {
            link: AtomicU64::new(0),
            value: UnsafeCell::new(None),
        }
    }
}

/// A never-shrinking arena with two tagged Treiber free lists.
///
/// Chunks are published by index reservation (`fetch_add` on `reserved`), so
/// two growers never contend for the same chunk and neither has to retry. A
/// published chunk pointer is never cleared and never freed before [`Drop`],
/// which is what makes resolving a stale index defined rather than a
/// use-after-free.
///
/// The arena is split by index at `ring_slots`: below it are the ring's slots,
/// above it the nodes that serve run-queue entries. **That split is what
/// bounds the band.** A ring entry exists only if a ring node was free, so
/// "the ring is full" is one fact in one place — not a counter a requester has
/// to CAS and then reconcile with an arena that could also run out.
struct Pool<T> {
    chunks: [AtomicPtr<Node<T>>; MAX_CHUNKS],
    /// Chunks handed out to growers — may briefly exceed the number actually
    /// published. Nothing resolves an index through it.
    reserved: AtomicUsize,
    /// Tagged head of the ring's slot supply. Empty means the band is full.
    free_ring: AtomicU64,
    /// Ring slots currently out — C `epicsRingPointerGetUsed`.
    ///
    /// **Why it lives here and nowhere else.** It is raised by the
    /// [`Pool::alloc_ring`] that took the slot and lowered by the
    /// [`Pool::dealloc`] that gave it back, which are the real forward and
    /// reverse of one ring entry. Counted from outside instead — a requester
    /// incrementing after its push, a worker decrementing after its pop — the
    /// decrement can land before its own increment, because the entry is
    /// visible to a worker from the moment it is linked: the band then reads
    /// one below zero, which is `usize::MAX`.
    ring_used: AtomicUsize,
    /// Tagged head of the run-queue supply, grown on demand: a run-queue entry
    /// is never refused for capacity.
    free_task: AtomicU64,
    /// First index that is not a ring slot.
    ring_slots: usize,
}

impl<T> Pool<T> {
    /// An arena whose ring supply is exactly `ring_slots` nodes, allocated up
    /// front the way C allocates its whole ring in `callbackInit` — so neither
    /// the request path nor a worker allocates once the band is running.
    ///
    /// Chunks are allocated whole, so the nodes past `ring_slots` in the last
    /// chunk become the initial run-queue supply instead of being wasted. One
    /// node costs more than C's one ring pointer, which is what a band of a
    /// configured `callbackQueueSize` costs over C.
    fn new(ring_slots: usize) -> Self {
        let pool = Pool {
            chunks: std::array::from_fn(|_| AtomicPtr::new(std::ptr::null_mut())),
            reserved: AtomicUsize::new(0),
            free_ring: AtomicU64::new(pack(IDX_NONE, 0)),
            ring_used: AtomicUsize::new(0),
            free_task: AtomicU64::new(pack(IDX_NONE, 0)),
            ring_slots,
        };
        while chunk_base(pool.reserved.load(Ordering::Relaxed)) < ring_slots {
            let Some((base, len)) = pool.grow() else {
                break;
            };
            let split = (base + len).min(ring_slots).max(base);
            pool.link(&pool.free_ring, base, split);
            pool.link(&pool.free_task, split, base + len);
        }
        pool
    }

    /// Reserve, allocate and publish one more chunk, returning its index range.
    /// The elements are on no list yet — [`Pool::link`] decides which supply
    /// they join. `None` once the index space is exhausted.
    fn grow(&self) -> Option<(usize, usize)> {
        let c = self.reserved.fetch_add(1, Ordering::AcqRel);
        if c >= MAX_CHUNKS {
            self.reserved.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        let len = chunk_len(c);
        let base = chunk_base(c);
        let elements: Vec<Node<T>> = (0..len).map(|_| Node::default()).collect();
        let ptr = Box::leak(elements.into_boxed_slice()).as_mut_ptr();
        // We reserved `c`, so this slot is ours alone.
        self.chunks[c].store(ptr, Ordering::Release);
        Some((base, len))
    }

    /// Hand `[from, to)` to `list` as a single pre-linked chain — one CAS for
    /// the whole range, however long it is.
    fn link(&self, list: &AtomicU64, from: usize, to: usize) {
        if from >= to {
            return;
        }
        for g in from..to - 1 {
            let node = unsafe { self.get(g as u32) };
            node.link.store(pack((g + 1) as u32, 1), Ordering::Relaxed);
        }
        // The tail link closes over the current head on each attempt.
        let last = unsafe { self.get((to - 1) as u32) };
        loop {
            let head = list.load(Ordering::Acquire);
            let prev = last.link.load(Ordering::Relaxed);
            last.link.store(bump(prev, idx_of(head)), Ordering::Release);
            if list
                .compare_exchange_weak(
                    head,
                    bump(head, from as u32),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return;
            }
        }
    }

    /// # Safety
    ///
    /// `g` must be an index this arena has published — every index reachable
    /// from a link or from [`Pool::alloc`] is.
    #[inline]
    unsafe fn get(&self, g: u32) -> &Node<T> {
        let (c, off) = locate(g);
        let base = self.chunks[c].load(Ordering::Acquire);
        debug_assert!(!base.is_null(), "index {g} names an unpublished chunk");
        unsafe { &*base.add(off) }
    }

    /// Take one of the ring's slots, with the ring's depth including it. One
    /// CAS plus one `fetch_add`, and `None` is the band being full — the
    /// supply is the bound, so nothing else has to be consulted.
    ///
    /// The depth is returned rather than read back later because this is the
    /// only moment it is this entry's: a load after the push can already have
    /// been lowered by the worker that ran it.
    fn alloc_ring(&self) -> Option<(u32, usize)> {
        let i = self.pop(&self.free_ring)?;
        let depth = self.ring_used.fetch_add(1, Ordering::AcqRel) + 1;
        Some((i, depth))
    }

    /// Ring slots out right now — C `epicsRingPointerGetUsed`.
    fn ring_used(&self) -> usize {
        self.ring_used.load(Ordering::Acquire)
    }

    /// Take one run-queue node, growing the arena when the supply is empty.
    /// `None` only when the index space itself is exhausted.
    fn alloc_task(&self) -> Option<u32> {
        loop {
            if let Some(i) = self.pop(&self.free_task) {
                return Some(i);
            }
            let (base, len) = self.grow()?;
            self.link(&self.free_task, base, base + len);
        }
    }

    fn pop(&self, list: &AtomicU64) -> Option<u32> {
        loop {
            let head = list.load(Ordering::Acquire);
            let i = idx_of(head);
            if i == IDX_NONE {
                return None;
            }
            // Possibly stale — only the CAS below decides, and the tag makes a
            // stale head impossible to confuse with the current one.
            let next = unsafe { self.get(i) }.link.load(Ordering::Acquire);
            if list
                .compare_exchange_weak(
                    head,
                    bump(head, idx_of(next)),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return Some(i);
            }
        }
    }

    /// Return `i` to the supply it came from — its index says which. The
    /// caller must own it: off both stacks and with its value taken.
    /// Give a whole chain of ring nodes back in one CAS — the reverse of
    /// `RETURN_EVERY` [`Pool::alloc_ring`] calls paid once. `head` through
    /// `tail` must already be linked head-to-tail and owned by this thread,
    /// and `n` must be their number.
    ///
    /// The count comes down with the slots and not before: it is lowered here,
    /// so for as long as a worker holds a chain the band both reports those
    /// entries queued and refuses requests for their slots. Lowering the count
    /// where the callback returns instead would report slots free that no
    /// requester can get.
    fn dealloc_chain(&self, head: u32, tail: u32, n: usize) {
        self.ring_used.fetch_sub(n, Ordering::AcqRel);
        let tail_node = unsafe { self.get(tail) };
        loop {
            let old = self.free_ring.load(Ordering::Acquire);
            let prev = tail_node.link.load(Ordering::Relaxed);
            tail_node
                .link
                .store(bump(prev, idx_of(old)), Ordering::Release);
            if self
                .free_ring
                .compare_exchange_weak(old, bump(old, head), Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    fn dealloc(&self, i: u32) {
        let list = if (i as usize) < self.ring_slots {
            self.ring_used.fetch_sub(1, Ordering::AcqRel);
            &self.free_ring
        } else {
            &self.free_task
        };
        let node = unsafe { self.get(i) };
        loop {
            let head = list.load(Ordering::Acquire);
            let prev = node.link.load(Ordering::Relaxed);
            node.link.store(bump(prev, idx_of(head)), Ordering::Release);
            if list
                .compare_exchange_weak(head, bump(head, i), Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }
}

impl<T> Drop for Pool<T> {
    fn drop(&mut self) {
        // Sole owner here, so the chunks can be reclaimed; dropping a chunk
        // drops whatever values were still queued in its nodes.
        for c in 0..MAX_CHUNKS {
            let ptr = *self.chunks[c].get_mut();
            if ptr.is_null() {
                continue;
            }
            unsafe {
                drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                    ptr,
                    chunk_len(c),
                )));
            }
        }
    }
}

/// One stack root on its own cache line. The requester's root and the
/// worker's root sharing a line is what made the first measured version of
/// this twice as slow at one worker as the band it replaced.
#[repr(align(128))]
struct Root(AtomicU64);

/// One band's queue: a LIFO inbox requesters push to, and a `ready` stack
/// workers pop from, in submission order. See the module docs.
pub(super) struct BandQueue<T> {
    inbox: Root,
    ready: Root,
    nodes: Pool<T>,
}

// SAFETY: every field is reached through atomics, and the one piece of
// interior mutability — `Node::value` — is written by the pusher before the
// node is linked and read by the single worker whose pop CAS unlinked it.
// `T: Send` because a value crosses threads between the two.
unsafe impl<T: Send> Send for BandQueue<T> {}
unsafe impl<T: Send> Sync for BandQueue<T> {}

impl<T> BandQueue<T> {
    /// A queue whose ring holds exactly `capacity` entries.
    pub(super) fn with_capacity(capacity: usize) -> Self {
        BandQueue {
            inbox: Root(AtomicU64::new(pack(IDX_NONE, 0))),
            ready: Root(AtomicU64::new(pack(IDX_NONE, 0))),
            nodes: Pool::new(capacity),
        }
    }

    /// Submit `v` into one of the band's `capacity` ring slots, answering with
    /// the ring's depth including this entry. Two CAS — one takes the slot,
    /// one publishes it — and the requester owns nothing in between. `Err(v)`
    /// hands the value back when no slot is free, which is the band being
    /// full.
    ///
    /// The depth comes back from the push because it is only this entry's
    /// before the entry is published: the band's high-water mark is latched
    /// from it, and a load afterwards would read a depth a worker has already
    /// lowered.
    ///
    /// `recruit` is called by the push that *began* a batch — see
    /// [`BandQueue::publish`].
    pub(super) fn push_ring(&self, v: T, recruit: impl FnOnce()) -> Result<usize, T> {
        match self.nodes.alloc_ring() {
            Some((i, depth)) => {
                self.publish(i, v, recruit);
                Ok(depth)
            }
            None => Err(v),
        }
    }

    /// Ring entries queued right now — C `epicsRingPointerGetUsed`.
    pub(super) fn ring_used(&self) -> usize {
        self.nodes.ring_used()
    }

    /// Submit `v` without taking a ring slot — the run-queue entries that
    /// share the band's FIFO but not its bound. `Err(v)` only when the arena's
    /// index space is exhausted, which is 4 G entries on one band.
    pub(super) fn push_task(&self, v: T, recruit: impl FnOnce()) -> Result<(), T> {
        match self.nodes.alloc_task() {
            Some(i) => {
                self.publish(i, v, recruit);
                Ok(())
            }
            None => Err(v),
        }
    }

    /// Put `v` in node `i`, link it into the inbox, and call `recruit` when
    /// this entry *began* the batch the inbox is accumulating.
    ///
    /// Only the CAS that links the node can answer that, which is why the
    /// recruitment decision lives here rather than in the requester: an empty
    /// inbox read before the push is a different claim, and a batch taken
    /// between that read and this CAS turns it into the wrong one — the
    /// requester declines to recruit for a batch that has nobody coming, and
    /// a worker already inside a callback never looks again. An inbox that is
    /// *not* empty at this CAS is a batch no worker has taken yet, because a
    /// refill takes the whole chain in one CAS; whoever answers that batch's
    /// recruitment therefore answers for this entry too.
    ///
    /// The CAS is sequentially consistent, not merely release: `recruit` goes
    /// on to test whether a worker is parked, and a store followed by a load
    /// of another location is exactly the pair that release ordering does not
    /// constrain. On armv7 — RTEMS — the load may then complete while the push
    /// sits in the store buffer, and the pusher decides not to wake the worker
    /// that is deciding not to see the entry. Lock `cmpxchg` already carries
    /// this on x86.
    fn publish(&self, i: u32, v: T, recruit: impl FnOnce()) {
        let node = unsafe { self.nodes.get(i) };
        // Ours until the CAS below links it.
        unsafe { *node.value.get() = Some(v) };
        loop {
            let head = self.inbox.0.load(Ordering::Acquire);
            let prev = node.link.load(Ordering::Relaxed);
            node.link.store(bump(prev, idx_of(head)), Ordering::Release);
            if self
                .inbox
                .0
                .compare_exchange_weak(head, bump(head, i), Ordering::SeqCst, Ordering::Acquire)
                .is_ok()
            {
                if idx_of(head) == IDX_NONE {
                    recruit();
                }
                return;
            }
        }
    }

    /// Take the value out of a node this thread has just unlinked, leaving the
    /// node itself for the caller to return (see [`Returns`]).
    ///
    /// # Safety
    ///
    /// `i` must be a node whose unlink CAS this thread won, so no other thread
    /// can reach it.
    #[inline]
    unsafe fn take(&self, i: u32) -> Option<T> {
        unsafe { (*self.nodes.get(i).value.get()).take() }
    }

    /// Open a consumer's chain of nodes to give back — one per worker, since a
    /// chain is single-threaded until it is published. `workers` is the band's
    /// width, which with the ring size decides how many nodes the chain holds
    /// (see [`return_batch`]).
    pub(super) fn returns(&self, workers: usize) -> Returns<'_, T> {
        Returns {
            queue: self,
            head: IDX_NONE,
            tail: IDX_NONE,
            n: 0,
            batch: return_batch(workers, self.nodes.ring_slots),
        }
    }

    /// Take the oldest submitted entry, or `None` when both stacks are empty,
    /// and hand its node to `returns`.
    ///
    /// Whether work was left behind comes out of the pop's own CAS, which is
    /// what spares a worker a fresh read of either root per callback — C's
    /// `*more` out-parameter (`callback.c:388-452`).
    ///
    /// With `BATCHED` the node is chained instead of going straight back —
    /// `callback.c:563-570` as of #996, where a worker returns `CB_FREE_EVERY`
    /// of them in one CAS, so both the free-list CAS and the ring count are
    /// paid once per chain rather than once per entry. That is worth a third
    /// of four workers' drain and a sixth of eight workers'; it costs a lone
    /// worker 5.6%, which is why the band decides ([`return_batch`]) and
    /// decides it as a constant.
    pub(super) fn pop_into<const BATCHED: bool>(
        &self,
        returns: &mut Returns<'_, T>,
    ) -> Option<Popped<T>> {
        loop {
            let head = self.ready.0.load(Ordering::Acquire);
            match idx_of(head) {
                IDX_NONE => {
                    // Nothing published and nothing submitted. A batch another
                    // worker is mid-refill with reads as submitted, since it
                    // comes out of the inbox in one CAS.
                    if idx_of(self.inbox.0.load(Ordering::Acquire)) == IDX_NONE {
                        return None;
                    }
                    if let Some(popped) = self.refill::<BATCHED>(returns) {
                        return Some(popped);
                    }
                }
                i => {
                    // Possibly stale; the tag decides.
                    let next = unsafe { self.nodes.get(i) }.link.load(Ordering::Acquire);
                    if self
                        .ready
                        .0
                        .compare_exchange_weak(
                            head,
                            bump(head, idx_of(next)),
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        let v = unsafe { self.take(i) };
                        returns.stage::<BATCHED>(i);
                        return v.map(|value| Popped {
                            value,
                            more: idx_of(next) != IDX_NONE,
                        });
                    }
                }
            }
        }
    }

    /// Take the inbox, turn it into submission order, publish all but the
    /// oldest entry onto `ready`, and return that oldest entry.
    ///
    /// `None` means another worker took the inbox first — the caller retries
    /// its pop rather than reporting the band empty.
    fn refill<const BATCHED: bool>(&self, returns: &mut Returns<'_, T>) -> Option<Popped<T>> {
        // Take the whole inbox in one CAS. Pushers and other workers can
        // contend; exactly one of them comes away with the chain.
        let newest = loop {
            let batch = self.inbox.0.load(Ordering::Acquire);
            if self
                .inbox
                .0
                .compare_exchange_weak(
                    batch,
                    bump(batch, IDX_NONE),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                break idx_of(batch);
            }
        };
        if newest == IDX_NONE {
            // Another worker took the chain this one had seen.
            return None;
        }
        // Reverse in place: the inbox is newest-first, submission order is the
        // other way round. Every node in the chain is this thread's.
        let mut oldest = IDX_NONE;
        let mut cursor = newest;
        while cursor != IDX_NONE {
            let node = unsafe { self.nodes.get(cursor) };
            let link = node.link.load(Ordering::Relaxed);
            let next = idx_of(link);
            node.link.store(bump(link, oldest), Ordering::Relaxed);
            oldest = cursor;
            cursor = next;
        }
        let rest = idx_of(
            unsafe { self.nodes.get(oldest) }
                .link
                .load(Ordering::Relaxed),
        );
        if rest != IDX_NONE {
            // `newest` is the chain's last node after the reversal.
            self.publish_ready(rest, newest);
        }
        let v = unsafe { self.take(oldest) };
        returns.stage::<BATCHED>(oldest);
        v.map(|value| Popped {
            value,
            more: rest != IDX_NONE,
        })
    }

    /// Link a batch's tail onto `ready` and swing the root to its first entry.
    ///
    /// Sequentially consistent for the same reason as [`BandQueue::push`]: the
    /// publisher's own next step is to check whether a worker is parked next to
    /// the batch it just published.
    fn publish_ready(&self, first: u32, tail: u32) {
        // The chain is this thread's until the CAS below links it, so its tail
        // can be re-pointed on every attempt.
        let tail_node = unsafe { self.nodes.get(tail) };
        loop {
            let head = self.ready.0.load(Ordering::Acquire);
            let prev = tail_node.link.load(Ordering::Relaxed);
            tail_node
                .link
                .store(bump(prev, idx_of(head)), Ordering::Release);
            if self
                .ready
                .0
                .compare_exchange_weak(head, bump(head, first), Ordering::SeqCst, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    /// Whether the queue looks empty. Sequentially consistent, because the
    /// worker's sleep decision pairs this against a pusher's wake decision
    /// (see [`Parking`]). It may read non-empty for an entry another worker is
    /// already taking, which costs one extra poll and never a lost entry.
    ///
    /// It also reads *empty* for a batch a worker holds mid-refill: the inbox
    /// is already drained and `ready` not yet published. What keeps a worker
    /// that parks inside that window from sleeping on a queue with work in it
    /// is the refilling worker's own [`Popped::more`], which is true exactly
    /// when its refill published something — so the publish is followed by a
    /// wake, and a batch of one publishes nothing and leaves the band as empty
    /// as this said it was.
    pub(super) fn is_empty(&self) -> bool {
        idx_of(self.ready.0.load(Ordering::SeqCst)) == IDX_NONE
            && idx_of(self.inbox.0.load(Ordering::SeqCst)) == IDX_NONE
    }
}

/// One entry a worker took, and whether the pop that took it left another
/// behind — C's `nextNode` and its `*more` (`callback.c:438-452`).
///
/// A worker recruits another on `more` and nothing else. It is the pop's own
/// CAS speaking, so it can be behind the queue by the time it is read: a batch
/// that arrived since is one whose own first push already answered for it (see
/// [`Parking`]), and an entry this worker drained in the meantime needs no
/// second worker. What the band must never do is let a published entry wait
/// for a *later* push to be noticed, and that is the requester's rule, not
/// this one.
pub(super) struct Popped<T> {
    pub(super) value: T,
    pub(super) more: bool,
}

/// Nodes a consumer has run and not yet given back, chained and returned
/// `RETURN_EVERY` at a time — C's `CB_FREE_EVERY` done-chain
/// (`callback.c:563-570`, #996).
///
/// **Every node this stages is returned to the pool before the handle goes
/// away**, and the handle is the only way to stage one, so no exit path —
/// early return, a panicking callback, a worker shutting down — can strand a
/// slot. The chain is single-threaded until the CAS that publishes it, which
/// is why it is a handle per consumer and not shared state.
///
/// The price of holding nodes is paid in the band's own currency: for as long
/// as a chain is unflushed those slots are neither free to a requester nor
/// counted as drained, so a saturated band refuses that many requests per
/// worker earlier than one that returns every node at once, and
/// `callbackQueueStatus` reports them queued until the chain goes back. A
/// worker flushes before it sleeps, so a band that has caught up holds none,
/// and [`return_batch`] is 1 for a one-worker band, which is every band until
/// `callbackParallelThreads` widens one — so the default band's count stays
/// exactly the entries queued.
pub(super) struct Returns<'a, T> {
    queue: &'a BandQueue<T>,
    /// Newest staged node, or `IDX_NONE`.
    head: u32,
    /// Oldest staged node — the end the free list is linked onto.
    tail: u32,
    n: usize,
    /// Nodes to chain before giving them back — [`return_batch`].
    batch: usize,
}

/// Most nodes a worker may chain before giving them back — C's
/// `CB_FREE_EVERY`.
const RETURN_EVERY: usize = 16;

/// How many nodes a worker of this band chains before returning them.
///
/// Two inputs, each for its own reason.
///
/// **The worker count, because batching only pays where workers contend.** A
/// chain trades one free-list CAS and one count update per entry for one per
/// chain, and that trade is a loss when nobody is competing for either: on this
/// box, measured against the same binary's unchained arm, a single worker's
/// drain is 5.6% *slower* at a chain of 16 (62.5 → 65.9 ns per entry) while
/// four workers are 32.6% faster (580.2 → 391.0) and eight 17.8% (567.5 →
/// 466.4). A band with one worker — the `callbackParallelThreads` default —
/// therefore returns every node as it dequeues it, which is also what keeps
/// its ring count exactly the entries queued. #996 chains 16 whatever the
/// band's width.
///
/// **The ring size, because a worker must not be able to starve a requester.**
/// Staged slots are neither free nor drained, so a chain that could hold a
/// band's whole ring would let a worker blocked inside one callback refuse
/// every request until it returns. Half the ring, shared out over the workers,
/// bounds that by construction instead of by a check at the boundary.
fn return_batch(workers: usize, ring_slots: usize) -> usize {
    if workers < 2 {
        return 1;
    }
    (ring_slots / (2 * workers)).clamp(1, RETURN_EVERY)
}

impl<T> Returns<'_, T> {
    /// Whether this band's workers chain at all — [`return_batch`] of 1 means
    /// a node goes back as it is dequeued, which is both what a band with one
    /// worker measures fastest and what keeps its ring count exactly the
    /// entries queued.
    pub(super) fn is_batched(&self) -> bool {
        self.batch > 1
    }

    /// Chain node `i`, which this thread has just unlinked and emptied.
    #[inline]
    fn stage<const BATCHED: bool>(&mut self, i: u32) {
        if !BATCHED || (i as usize) >= self.queue.nodes.ring_slots {
            // A band that does not chain gives the node straight back, with
            // the test folded out of its loop entirely.
            //
            // Run-queue nodes likewise are not ring slots: nothing waits on
            // them, and the supply they come from grows, so there is nothing
            // to batch.
            self.queue.nodes.dealloc(i);
            return;
        }
        let node = unsafe { self.queue.nodes.get(i) };
        let prev = node.link.load(Ordering::Relaxed);
        node.link.store(bump(prev, self.head), Ordering::Relaxed);
        self.head = i;
        if self.tail == IDX_NONE {
            self.tail = i;
        }
        self.n += 1;
        if self.n == self.batch {
            self.flush();
        }
    }

    /// Give back whatever is staged. Called at the batch size, before a worker
    /// sleeps, and on drop.
    #[inline]
    pub(super) fn flush(&mut self) {
        if self.head == IDX_NONE {
            return;
        }
        self.queue.nodes.dealloc_chain(self.head, self.tail, self.n);
        self.head = IDX_NONE;
        self.tail = IDX_NONE;
        self.n = 0;
    }
}

impl<T> Drop for Returns<'_, T> {
    fn drop(&mut self) {
        self.flush();
    }
}

/// The band's wake-up path: one [`Event`] per worker, and no lock.
///
/// C signals a counting event per push and re-triggers it per pop that leaves
/// work behind (`callback.c:375`, `:224`); #996 replaces that with a wake
/// token per worker. This is the same shape — a pusher wakes at most one
/// worker, and only one that is actually parked.
///
/// What this adds over one bare `Event` is the `sleepers` count: with several
/// workers the signaller would otherwise have to walk every slot to learn
/// that none of them is parked, and a band running flat out is exactly the
/// case where none is.
///
/// ## Who wakes a worker
///
/// **Every published entry has a committed observer** — a worker that reads the
/// queue after the entry is visible and before it next sleeps. One rule gives
/// every entry one: *the push that begins a batch wakes a sleeper, if any
/// worker is asleep*. Whether a push began a batch is not an opinion about the
/// band's state but a property of the push's own linking CAS — the inbox head
/// it displaced ([`BandQueue::publish`]) — and a refill takes the whole inbox
/// chain in one CAS, so a push that joined a batch is taken by whoever takes
/// that batch, whose own starter already answered for it.
///
/// What a requester must *not* do is infer an observer from a worker being
/// awake. An awake worker's last queue read may already be behind it, and a
/// worker inside a callback that blocks never reads again — so an entry
/// credited to it waits for the next push. The wake therefore has to come out
/// of a worker's sleep ([`Parking::wake_one`], which is why a claim another
/// signaller holds does not end its scan), and the `sleepers` load that gates
/// it is paired with the SeqCst push: either the parked worker's poll sees the
/// entry or the requester's load sees it parked. That pair is why
/// [`BandQueue::publish`] and [`BandQueue::is_empty`] are both SeqCst.
///
/// Spreading the batch over more workers belongs to the workers — each pop
/// that leaves work behind wakes another ([`Parking::wake_one`] in the band's
/// loop), so the cascade is paid for on the band's threads and not on the
/// requester's. A scan thread pushing into a band that is keeping up issues no
/// syscall at all.
///
/// #996 declines the recruiting wake on exactly the inference above: it skips
/// it while some awake worker sits between callbacks and so will read the
/// inbox next (`anyReady`, `callback.c:519`). That worker is trusted without
/// being verified, which is why #996 then needs `CB_STALE_US` — no worker
/// finishing a batch for 20 µs is read as the trusted worker having been
/// preempted, and the requester wakes a sleeper after all
/// (`callback.c:808-822`). Neither is here. The window `anyReady` reports is
/// the few instructions between a worker's `busy = 0` and its next queue read,
/// so what the rule saves is a wake the band almost never needed to skip, and
/// declining on it is precisely what made a timeout necessary to get liveness
/// back. One wake per batch, decided by the batch's own first push, needs no
/// clock on the request path, no progress counter and no per-worker `busy`
/// flag.
///
/// Given its own cache-line block, for the same reason [`Root`] has one:
/// `sleepers` is written by a worker on every park and unpark, while the band's
/// ring counter is written by every requester. Measured in C with the two in one
/// 64-byte line, a band loses 46% of its throughput at one worker, 33% at four
/// and 9% at eight.
///
/// The slots themselves are *not* blocked off, where C aligns each `cbWorker`
/// to 128 bytes (`callback.c:144`). What C separates there is its `busy` flag,
/// written twice per callback; the only traffic on a slot here is one store per
/// park and one per wake, and four slots in one line is what lets
/// [`Parking::wake_one`]'s scan fetch them together. Measured in one binary
/// with only the stride switched, one slot per 128-byte block costs a drain
/// 5.6% at two workers and 6.1% at four, and nothing measurable at one.
#[repr(align(128))]
pub(super) struct Parking {
    /// Workers inside [`ParkSlot::park_until`] — the pusher's test for whether
    /// a scan is worth anything at all.
    sleepers: AtomicUsize,
    /// One event per worker, indexed by the worker's ordinal — the same `j` the
    /// band names its thread after. A worker therefore cannot end up sharing a
    /// slot with another, which would turn one worker's "I am awake" into the
    /// other's lost wake-up.
    slots: Box<[Event]>,
}

impl Parking {
    pub(super) fn new(workers: usize) -> Self {
        Parking {
            sleepers: AtomicUsize::new(0),
            slots: (0..workers.max(1)).map(|_| Event::new()).collect(),
        }
    }

    /// The band's width — how many workers share this queue.
    pub(super) fn workers(&self) -> usize {
        self.slots.len()
    }

    /// Claim worker `slot`'s park slot for as long as the worker runs. Holding
    /// the token is what publishes the worker's thread handle, so a signaller
    /// that finds a slot parked finds a handle in it without the worker having
    /// had to remember to announce itself first.
    pub(super) fn waiter(&self, slot: usize) -> ParkSlot<'_> {
        ParkSlot {
            sleepers: &self.sleepers,
            waiter: self.slots[slot].waiter(),
        }
    }

    /// Take one parked worker out of its sleep, if any is parked. A push with
    /// every worker of the band running pays one load and no syscall.
    ///
    /// Only a slot this call claims itself ends the scan. A slot another
    /// signaller has already claimed is a worker that is *going* to wake, but
    /// one whose next read of the queue may be ordered before this entry was
    /// published — the earlier signaller's entry is what it is bound to find.
    /// Counting it would spend this entry's one recruitment on a worker that
    /// owes nothing to this entry and leave a genuinely parked worker asleep
    /// beside it, which is the band's one way to strand an entry: a worker
    /// that then blocks inside its callback never looks again.
    pub(super) fn wake_one(&self) {
        if self.sleepers.load(Ordering::SeqCst) == 0 {
            return;
        }
        for slot in &self.slots {
            if slot.signal() == Signalled::Claimed {
                return;
            }
        }
    }

    /// Wake every parked worker — the shutdown path, where each has to re-test
    /// its own exit condition.
    pub(super) fn wake_all(&self) {
        for slot in &self.slots {
            slot.wake();
        }
    }
}

/// One worker's claim on its park slot — see [`Parking::waiter`].
pub(super) struct ParkSlot<'a> {
    sleepers: &'a AtomicUsize,
    waiter: EventWaiter<'a>,
}

impl ParkSlot<'_> {
    /// Park until `ready` holds, counted in `sleepers` for the whole sleep so
    /// a pusher's one-load fast path is only taken when nobody is there.
    pub(super) fn park_until(&self, ready: impl FnMut() -> bool) {
        self.sleepers.fetch_add(1, Ordering::SeqCst);
        self.waiter.wait_until(ready);
        self.sleepers.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pop one entry and give its slot straight back — what a worker does over
    /// `RETURN_EVERY` entries, done one at a time so a test can speak about
    /// the ring after every pop.
    fn pop1<T>(q: &BandQueue<T>) -> Option<T> {
        q.pop_into::<true>(&mut q.returns(1)).map(|p| p.value)
    }
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    /// The two states the emptiness test has to separate, and the transition
    /// back — `pop` on an empty queue must not consume the dummy that makes
    /// the next push valid.
    #[test]
    fn empty_and_non_empty_are_the_two_boundaries_of_is_empty() {
        let q = BandQueue::<u32>::with_capacity(4);
        assert!(q.is_empty());
        assert_eq!(pop1(&q), None);
        assert!(q.is_empty());
        q.push_ring(7, || {}).unwrap();
        assert!(!q.is_empty());
        assert_eq!(pop1(&q), Some(7));
        assert!(q.is_empty());
        q.push_ring(8, || {}).unwrap();
        assert_eq!(pop1(&q), Some(8));
    }

    /// `CHUNK0` is the first chunk boundary and every later one doubles, so a
    /// ring of this many slots spans several of them. The index arithmetic is
    /// what is on trial: a mislocated element shows up as a value that comes
    /// back in the wrong order or not at all.
    #[test]
    fn a_ring_spanning_several_chunks_keeps_its_order() {
        // Miri interprets every one of these, so it gets the smallest span
        // that still crosses several boundaries.
        let n = if cfg!(miri) { CHUNK0 * 3 } else { CHUNK0 * 40 };
        let q = BandQueue::<usize>::with_capacity(n);
        for i in 0..n {
            q.push_ring(i, || {}).unwrap();
        }
        for i in 0..n {
            assert_eq!(pop1(&q), Some(i), "entry {i} came back out of order");
        }
        assert!(q.is_empty());
    }

    /// An arena sized well below the traffic recycles every node many
    /// times over, which is the only way the `(index, tag)` packing is
    /// exercised: a tag that failed to move would show up here as a lost or
    /// duplicated entry.
    #[test]
    fn nodes_survive_being_recycled() {
        let q = BandQueue::<usize>::with_capacity(2);
        let rounds = if cfg!(miri) { 40usize } else { 2000 };
        for round in 0..rounds {
            q.push_ring(round, || {}).unwrap();
            q.push_ring(round + 1_000_000, || {}).unwrap();
            assert_eq!(pop1(&q), Some(round));
            assert_eq!(pop1(&q), Some(round + 1_000_000));
        }
        assert!(q.is_empty());
    }

    /// Four producers against four consumers: every entry is delivered
    /// exactly once. Order is not asserted here — above one
    /// worker a batch can be published onto the previous batch's remainder,
    /// which is the band's documented order at that worker count.
    #[test]
    fn every_entry_is_delivered_exactly_once_under_many_consumers() {
        const PRODUCERS: usize = 4;
        let per: usize = if cfg!(miri) { 60 } else { 5_000 };
        let q = Arc::new(BandQueue::<(usize, usize)>::with_capacity(PRODUCERS * per));
        let taken = Arc::new(std::sync::Mutex::new(vec![Vec::new(); PRODUCERS]));
        let done = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicUsize::new(0));

        std::thread::scope(|s| {
            for p in 0..PRODUCERS {
                let q = Arc::clone(&q);
                let finished = Arc::clone(&finished);
                s.spawn(move || {
                    for i in 0..per {
                        q.push_ring((p, i), || {}).unwrap();
                    }
                    finished.fetch_add(1, Ordering::SeqCst);
                });
            }
            for _ in 0..4 {
                let q = Arc::clone(&q);
                let taken = Arc::clone(&taken);
                let done = Arc::clone(&done);
                s.spawn(move || {
                    let mut mine = vec![Vec::new(); PRODUCERS];
                    loop {
                        match pop1(&q) {
                            Some((p, i)) => mine[p].push(i),
                            None => {
                                if done.load(Ordering::SeqCst) {
                                    break;
                                }
                                std::thread::yield_now();
                            }
                        }
                    }
                    let mut all = taken.lock().unwrap();
                    for p in 0..PRODUCERS {
                        all[p].extend_from_slice(&mine[p]);
                    }
                });
            }
            // Consumers may leave only once no producer can push again and
            // the queue has drained — an empty queue on its own means
            // nothing while a producer is still running.
            s.spawn({
                let q = Arc::clone(&q);
                let done = Arc::clone(&done);
                let finished = Arc::clone(&finished);
                move || {
                    while finished.load(Ordering::SeqCst) < PRODUCERS || !q.is_empty() {
                        std::thread::yield_now();
                    }
                    done.store(true, Ordering::SeqCst);
                }
            });
        });

        let all = taken.lock().unwrap();
        for p in 0..PRODUCERS {
            let mut got = all[p].clone();
            got.sort_unstable();
            assert_eq!(got.len(), per, "producer {p} lost or duplicated entries");
            assert!(
                got.iter().copied().eq(0..per),
                "producer {p} entries are not the set it pushed"
            );
        }
    }

    /// A queue dropped with entries still in it owns those values — the band
    /// relies on that to finalize the task entries left over at shutdown.
    #[test]
    fn dropping_the_queue_drops_the_entries_still_in_it() {
        let live = Arc::new(());
        {
            let q = BandQueue::<Arc<()>>::with_capacity(5);
            for _ in 0..5 {
                q.push_ring(Arc::clone(&live), || {}).unwrap();
            }
            assert_eq!(Arc::strong_count(&live), 6);
            assert!(pop1(&q).is_some());
            assert_eq!(Arc::strong_count(&live), 5);
        }
        assert_eq!(
            Arc::strong_count(&live),
            1,
            "entries still queued were leaked rather than dropped"
        );
    }

    /// The two sides of the sleep decision, at the boundary that matters: a
    /// condition already true must not park at all, and a condition made true
    /// after the worker has decided to sleep must still reach it — including
    /// when the waker gets there before the announcement, which is the window
    /// a plain flag would lose.
    #[test]
    fn park_until_returns_on_a_condition_already_true_and_on_a_later_one() {
        let parking = Arc::new(Parking::new(1));
        let gate = Arc::new(AtomicBool::new(true));
        let go = Arc::new(AtomicBool::new(false));
        let announced = Arc::new(AtomicBool::new(false));
        let woke = Arc::new(AtomicBool::new(false));

        std::thread::scope(|s| {
            let (p, gate, go2, announced2, woke2) = (
                Arc::clone(&parking),
                Arc::clone(&gate),
                Arc::clone(&go),
                Arc::clone(&announced),
                Arc::clone(&woke),
            );
            s.spawn(move || {
                let parked = p.waiter(0);
                // Already true: returns without ever parking.
                parked.park_until(|| gate.load(Ordering::SeqCst));
                announced2.store(true, Ordering::SeqCst);
                // False until the main thread flips it.
                parked.park_until(|| go2.load(Ordering::SeqCst));
                woke2.store(true, Ordering::SeqCst);
            });
            while !announced.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            go.store(true, Ordering::SeqCst);
            parking.wake_one();
        });

        assert!(
            woke.load(Ordering::SeqCst),
            "the worker never left its park"
        );
    }

    /// The requester's two rules over every boundary of the count they turn
    /// on: nothing parked owes nothing, every worker parked owes a wake
    /// whatever the push found, and in between only a push that begins a batch
    /// owes one — spreading that batch is the workers' cascade.
    /// Every boundary of the chain size the band derives: a single worker
    /// returns each node as it dequeues it, and no wider band may chain more
    /// than half its ring between them.
    #[test]
    fn a_worker_chains_nothing_alone_and_never_half_the_ring() {
        assert_eq!(return_batch(1, 4096), 1, "nothing to amortize alone");
        assert_eq!(return_batch(0, 4096), 1, "a band with no worker, likewise");
        assert_eq!(return_batch(2, 4096), RETURN_EVERY, "C's cap, reached");
        assert_eq!(return_batch(2, 64), 16, "half of 64, split two ways");
        assert_eq!(return_batch(2, 63), 15, "just under it");
        assert_eq!(return_batch(8, 64), 4, "half of 64, split eight ways");
        assert_eq!(return_batch(8, 16), 1, "a ring too small to share out");
        assert_eq!(return_batch(8, 1), 1, "and one that cannot be shared");
        for workers in 2..=8usize {
            for ring_slots in 1..=64usize {
                let batch = return_batch(workers, ring_slots);
                assert!(
                    batch * workers * 2 <= ring_slots.max(2 * workers),
                    "{workers} workers chaining {batch} of {ring_slots} slots                      can starve a requester"
                );
            }
        }
    }

    /// A wake must come out of a worker's sleep, not out of a claim another
    /// signaller already holds: the claimed worker's next read of the queue
    /// can be ordered before this entry was published, so crediting it leaves
    /// this entry with no observer and a parked worker beside it.
    #[test]
    fn a_wake_passes_over_a_slot_another_signaller_has_already_claimed() {
        let parking = Parking::new(2);
        // Both workers parked — the count a requester reads.
        parking.slots[0].announce_for_test();
        parking.slots[1].announce_for_test();
        parking.sleepers.store(2, Ordering::SeqCst);

        // One signaller claims the first slot and is stopped before its
        // `unpark`.
        assert_eq!(parking.slots[0].signal(), Signalled::Claimed);

        parking.wake_one();
        assert_eq!(
            parking.slots[1].signal(),
            Signalled::Pending,
            "the wake stopped at a claim it did not make and left the second \
             worker asleep"
        );
    }

    /// The push that begins a batch is the one that recruits a worker for it,
    /// and only the linking CAS can say which push that was: the second of
    /// two pushes joins a batch no worker has taken yet, and a push that
    /// lands after a worker's take-all begins one of its own. Run-queue
    /// entries share the inbox, so they share the rule.
    #[test]
    fn only_the_push_that_begins_a_batch_recruits_for_it() {
        let q = BandQueue::<u32>::with_capacity(4);
        let mut recruited = 0usize;

        q.push_ring(1, || recruited += 1).unwrap();
        assert_eq!(recruited, 1, "a fresh band's push has nobody coming");
        q.push_ring(2, || recruited += 1).unwrap();
        assert_eq!(recruited, 1, "this one joined the batch the first began");

        assert_eq!(pop1(&q), Some(1), "one pop takes the whole inbox");
        q.push_ring(3, || recruited += 1).unwrap();
        assert_eq!(
            recruited, 2,
            "a worker holds the last batch, so this push begins one of its own"
        );
        q.push_task(4, || recruited += 1).unwrap();
        assert_eq!(recruited, 2, "a task entry joins the batch in the inbox");
    }

    /// `wake_one` with nobody parked is a load and nothing else — the
    /// property the band's per-push wake-up depends on for its cost.
    #[test]
    fn waking_a_band_with_no_sleeper_is_a_no_op() {
        let parking = Parking::new(2);
        parking.wake_one();
        parking.wake_all();
        let _parked = parking.waiter(0);
        parking.wake_one();
    }

    /// Submission order at one worker, the band's default. Pushes land while
    /// earlier entries are still in `ready`, so the queue refills several
    /// times and the batch boundaries fall at varying depths — the boundary a
    /// stack would reorder entries at if a refill published onto a `ready`
    /// that was not empty.
    #[test]
    fn entries_leave_in_submission_order_across_batches() {
        let q = BandQueue::<usize>::with_capacity(2_000);
        let mut pushed = 0usize;
        let mut popped = 0usize;
        for round in 0..200usize {
            for _ in 0..=(round % 5) {
                q.push_ring(pushed, || {}).unwrap();
                pushed += 1;
            }
            for _ in 0..=(round % 3) {
                if popped == pushed {
                    break;
                }
                assert_eq!(pop1(&q), Some(popped), "entry {popped} left out of turn");
                popped += 1;
            }
        }
        while popped < pushed {
            assert_eq!(pop1(&q), Some(popped));
            popped += 1;
        }
        assert!(q.is_empty());
    }

    /// The invariant the band's bound rests on: a ring entry exists only if a
    /// ring slot was free, and the supply is exactly `capacity` slots. The
    /// run-queue supply is a different list, so a full ring does not refuse a
    /// run-queue entry — and a slot returns to the ring the moment its entry
    /// is consumed, not when the band next looks at a counter.
    #[test]
    fn the_ring_supply_is_the_bound_and_a_task_entry_sits_outside_it() {
        let q = BandQueue::<usize>::with_capacity(3);
        for v in 0..3 {
            q.push_ring(v, || {}).unwrap();
        }
        assert_eq!(
            q.push_ring(99, || {}),
            Err(99),
            "a fourth entry fit a ring of three"
        );
        q.push_task(7, || {}).unwrap();
        assert_eq!(pop1(&q), Some(0));
        q.push_ring(100, || {}).unwrap();
        assert_eq!(pop1(&q), Some(1));
        assert_eq!(pop1(&q), Some(2));
        assert_eq!(pop1(&q), Some(7));
        assert_eq!(pop1(&q), Some(100));
        assert!(q.is_empty());
    }

    /// The other side of the refill boundary: a batch published onto a `ready`
    /// that still holds the previous batch's remainder. Reachable only above
    /// one worker, so it is driven here by calling `refill` directly. Nothing
    /// may be lost or duplicated, and the order is the documented one — the
    /// newer batch ahead of the older batch's tail.
    #[test]
    fn a_batch_published_onto_a_non_empty_ready_keeps_every_entry() {
        let q = BandQueue::<usize>::with_capacity(8);
        for v in 0..3 {
            q.push_ring(v, || {}).unwrap();
        }
        // Refills, hands back the oldest and leaves 1 and 2 in `ready`.
        assert_eq!(pop1(&q), Some(0));
        for v in 3..6 {
            q.push_ring(v, || {}).unwrap();
        }
        assert_eq!(
            q.refill::<true>(&mut q.returns(1)).map(|p| p.value),
            Some(3),
            "the second batch's oldest entry"
        );
        assert_eq!(pop1(&q), Some(4));
        assert_eq!(pop1(&q), Some(5));
        assert_eq!(pop1(&q), Some(1));
        assert_eq!(pop1(&q), Some(2));
        assert!(q.is_empty());
        assert_eq!(pop1(&q), None);
    }

    /// **Invariant:** `Popped::more` is true exactly when the pop's own CAS
    /// left an entry on `ready`.
    ///
    /// One case per boundary rather than one per story, and the fourth is the
    /// one that matters: a pop that empties `ready` while the inbox holds a
    /// batch reports no more work, which a fresh read of both roots would call
    /// non-empty. That is deliberate — the batch in the inbox has a worker
    /// owed to it by its own first push — and it is the whole difference
    /// between this and the two root loads it replaced.
    #[test]
    fn more_is_the_entry_the_pop_left_on_ready_and_nothing_else() {
        let q = BandQueue::<usize>::with_capacity(8);

        // A one-entry batch: the refill publishes nothing.
        q.push_ring(10, || {}).unwrap();
        let p = q.pop_into::<true>(&mut q.returns(1)).unwrap();
        assert_eq!((p.value, p.more), (10, false));

        // A three-entry batch: the refill publishes two.
        for v in 20..23 {
            q.push_ring(v, || {}).unwrap();
        }
        let p = q.pop_into::<true>(&mut q.returns(1)).unwrap();
        assert_eq!((p.value, p.more), (20, true), "two left on ready");
        let p = q.pop_into::<true>(&mut q.returns(1)).unwrap();
        assert_eq!((p.value, p.more), (21, true), "one left on ready");

        // The last of the batch, with a fresh batch sitting in the inbox.
        q.push_ring(30, || {}).unwrap();
        let p = q.pop_into::<true>(&mut q.returns(1)).unwrap();
        assert_eq!(
            (p.value, p.more),
            (22, false),
            "the inbox batch is its own first push's to answer for"
        );
        assert!(!q.is_empty(), "and it really is still queued");
        assert_eq!(pop1(&q), Some(30));
        assert_eq!(pop1(&q), None);
    }

    /// The layout the 46%/33%/9% loss above was measured against. The
    /// guarantee is that no field of the enclosing band can be placed in
    /// `Parking`'s extent, which is what the 128-byte alignment buys — where
    /// `sleepers` sits inside that extent does not matter, since the whole
    /// block is the park state's, and the ring counter the loss was measured
    /// against cannot land there wherever it is declared. Asserted rather
    /// than commented, because an attribute is easy to drop and nothing else
    /// makes alignment visible.
    #[test]
    fn the_park_counter_keeps_a_cache_line_to_itself() {
        assert!(
            std::mem::align_of::<Parking>() >= 128,
            "Parking lost its cache-line alignment"
        );
        assert_eq!(
            std::mem::size_of::<Parking>() % 128,
            0,
            "Parking no longer occupies whole cache-line blocks"
        );
    }
}
