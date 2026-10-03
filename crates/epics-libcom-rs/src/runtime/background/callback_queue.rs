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
//! This queue removes the holder from that path instead. `push` is one CAS
//! onto a LIFO inbox: it owns nothing, so a requester descheduled anywhere in
//! it delays nobody and there is no priority to inherit.
//!
//! # Inbox and ready, not one FIFO
//!
//! Two stacks, following the shape epics-base PR #996 arrived at by
//! measurement:
//!
//! - `inbox` — where requesters push, one CAS, newest first.
//! - `ready` — where workers pop, one CAS per entry.
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
//! - **One CAS per push, not two.** MS needs a `next` link CAS plus a `tail`
//!   swing, and the requester path is the band's only hot path, so every
//!   request pays for both.
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
use std::sync::OnceLock;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::thread::Thread;

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

/// A growable, never-shrinking arena with a tagged Treiber free list.
///
/// Chunks are published by index reservation (`fetch_add` on `reserved`), so
/// two growers never contend for the same chunk and neither has to retry. A
/// published chunk pointer is never cleared and never freed before [`Drop`],
/// which is what makes resolving a stale index defined rather than a
/// use-after-free.
struct Pool<T> {
    chunks: [AtomicPtr<Node<T>>; MAX_CHUNKS],
    /// Chunks handed out to growers — may briefly exceed the number actually
    /// published. Nothing resolves an index through it.
    reserved: AtomicUsize,
    /// Tagged head of the free list.
    free: AtomicU64,
}

impl<T> Pool<T> {
    fn new(min_elements: usize) -> Self {
        let pool = Pool {
            chunks: std::array::from_fn(|_| AtomicPtr::new(std::ptr::null_mut())),
            reserved: AtomicUsize::new(0),
            free: AtomicU64::new(pack(IDX_NONE, 0)),
        };
        // Preallocate the steady state, so neither the request path nor a
        // worker allocates once the band is running.
        let mut have = 0usize;
        while have < min_elements && pool.grow() {
            have = chunk_base(pool.reserved.load(Ordering::Relaxed));
        }
        pool
    }

    /// Reserve, allocate and publish one more chunk, then hand its elements to
    /// the free list as a single pre-linked chain (one CAS for the whole
    /// chunk). `false` once the index space is exhausted.
    fn grow(&self) -> bool {
        let c = self.reserved.fetch_add(1, Ordering::AcqRel);
        if c >= MAX_CHUNKS {
            self.reserved.fetch_sub(1, Ordering::AcqRel);
            return false;
        }
        let len = chunk_len(c);
        let base = chunk_base(c);
        let elements: Vec<Node<T>> = (0..len).map(|_| Node::default()).collect();
        let ptr = Box::leak(elements.into_boxed_slice()).as_mut_ptr();
        // We reserved `c`, so this slot is ours alone.
        self.chunks[c].store(ptr, Ordering::Release);

        // Chain the chunk internally; the tail link closes over the current
        // free head on each attempt.
        for off in 0..len - 1 {
            let node = unsafe { &*ptr.add(off) };
            node.link
                .store(pack((base + off + 1) as u32, 1), Ordering::Relaxed);
        }
        let last = unsafe { &*ptr.add(len - 1) };
        loop {
            let head = self.free.load(Ordering::Acquire);
            let prev = last.link.load(Ordering::Relaxed);
            last.link.store(bump(prev, idx_of(head)), Ordering::Release);
            if self
                .free
                .compare_exchange_weak(
                    head,
                    bump(head, base as u32),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return true;
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

    /// Take one node off the free list, growing the arena if it is empty.
    /// `None` only when the index space itself is exhausted.
    fn alloc(&self) -> Option<u32> {
        loop {
            let head = self.free.load(Ordering::Acquire);
            let i = idx_of(head);
            if i == IDX_NONE {
                if !self.grow() {
                    return None;
                }
                continue;
            }
            // Possibly stale — only the CAS below decides, and the tag makes a
            // stale head impossible to confuse with the current one.
            let next = unsafe { self.get(i) }.link.load(Ordering::Acquire);
            if self
                .free
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

    /// Return `i` to the free list. The caller must own it — off both stacks
    /// and with its value taken.
    fn dealloc(&self, i: u32) {
        let node = unsafe { self.get(i) };
        loop {
            let head = self.free.load(Ordering::Acquire);
            let prev = node.link.load(Ordering::Relaxed);
            node.link.store(bump(prev, idx_of(head)), Ordering::Release);
            if self
                .free
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
    /// A queue preallocated for `capacity` entries.
    pub(super) fn with_capacity(capacity: usize) -> Self {
        BandQueue {
            inbox: Root(AtomicU64::new(pack(IDX_NONE, 0))),
            ready: Root(AtomicU64::new(pack(IDX_NONE, 0))),
            nodes: Pool::new(capacity),
        }
    }

    /// Submit `v`. One CAS, owning nothing. `Err(v)` hands the value back only
    /// when the arena's index space is exhausted — 4 G entries on one band.
    ///
    /// The CAS that publishes the node is sequentially consistent, not merely
    /// release: the caller goes on to test whether a worker is parked, and a
    /// store followed by a load of another location is exactly the pair that
    /// release ordering does not constrain. On armv7 — RTEMS — the load may
    /// then complete while the push sits in the store buffer, and the pusher
    /// decides not to wake the worker that is deciding not to see the entry.
    /// Lock `cmpxchg` already carries this on x86.
    pub(super) fn push(&self, v: T) -> Result<(), T> {
        let Some(i) = self.nodes.alloc() else {
            return Err(v);
        };
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
                return Ok(());
            }
        }
    }

    /// Take the value out of a node this thread has just unlinked, and return
    /// the node to the arena.
    ///
    /// # Safety
    ///
    /// `i` must be a node whose unlink CAS this thread won, so no other thread
    /// can reach it.
    #[inline]
    unsafe fn consume(&self, i: u32) -> Option<T> {
        let v = unsafe { (*self.nodes.get(i).value.get()).take() };
        self.nodes.dealloc(i);
        v
    }

    /// Take the oldest submitted entry, or `None` when both stacks are empty.
    pub(super) fn pop(&self) -> Option<T> {
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
                    if let Some(v) = self.refill() {
                        return Some(v);
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
                        return unsafe { self.consume(i) };
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
    fn refill(&self) -> Option<T> {
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
        unsafe { self.consume(oldest) }
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
    /// is already drained and `ready` not yet published. The entries are that
    /// worker's to publish, and the band's worker loop re-tests this after
    /// every pop (`callback.c:224`), so the publish is followed by a wake —
    /// which is what keeps a worker that parks inside this window from
    /// sleeping on a queue that has work in it.
    pub(super) fn is_empty(&self) -> bool {
        idx_of(self.ready.0.load(Ordering::SeqCst)) == IDX_NONE
            && idx_of(self.inbox.0.load(Ordering::SeqCst)) == IDX_NONE
    }
}

/// Park state of one worker slot.
const SLOT_FREE: u32 = 0;
const SLOT_AWAKE: u32 = 1;
const SLOT_SLEEPING: u32 = 2;

struct Slot {
    state: AtomicU32,
    /// Published by the worker that claims the slot, before it can ever
    /// announce `SLOT_SLEEPING`, so a waker that observes sleeping observes a
    /// handle.
    thread: OnceLock<Thread>,
}

impl Default for Slot {
    fn default() -> Self {
        Slot {
            state: AtomicU32::new(SLOT_FREE),
            thread: OnceLock::new(),
        }
    }
}

/// The band's wake-up path: one park slot per worker, and no lock.
///
/// C signals a counting event per push and re-triggers it per pop that leaves
/// work behind (`callback.c:375`, `:224`); #996 replaces that with a wake
/// token per worker. This is the same shape — a pusher wakes at most one
/// worker, and only one that is actually parked.
///
/// The announce/poll pair is sequentially consistent on both sides, which is
/// what closes the lost-wake-up window a plain flag would leave: a pusher
/// stores the entry then loads `sleepers`, a worker stores `SLOT_SLEEPING`
/// then loads the queue, so at least one of them sees the other.
///
/// Given its own cache-line block, for the same reason [`Root`] has one:
/// `sleepers` is written by a worker on every park and unpark, while the band's
/// ring counter is written by every requester. Measured in C with the two in one
/// 64-byte line, a band loses 46% of its throughput at one worker, 33% at four
/// and 9% at eight.
#[repr(align(128))]
pub(super) struct Parking {
    /// Workers inside [`Parking::park_until`] — the pusher's test for whether
    /// a scan is worth anything at all.
    sleepers: AtomicUsize,
    /// One slot per worker, indexed by the worker's ordinal — the same `j` the
    /// band names its thread after. A worker therefore cannot end up sharing a
    /// slot with another, which would turn one worker's "I am awake" into the
    /// other's lost wake-up.
    slots: Box<[Slot]>,
}

impl Parking {
    pub(super) fn new(workers: usize) -> Self {
        Parking {
            sleepers: AtomicUsize::new(0),
            slots: (0..workers.max(1)).map(|_| Slot::default()).collect(),
        }
    }

    /// Publish this thread's handle in its slot. Called once by each worker
    /// before it can park, so a waker that finds a slot sleeping finds a
    /// handle in it.
    pub(super) fn register(&self, slot: usize) {
        let slot = &self.slots[slot];
        let _ = slot.thread.set(std::thread::current());
        slot.state.store(SLOT_AWAKE, Ordering::SeqCst);
    }

    /// Park until `ready` holds. Announces before every poll, so a pusher that
    /// misses the announcement is a pusher whose entry this poll sees.
    pub(super) fn park_until(&self, slot: usize, mut ready: impl FnMut() -> bool) {
        let state = &self.slots[slot].state;
        self.sleepers.fetch_add(1, Ordering::SeqCst);
        loop {
            state.store(SLOT_SLEEPING, Ordering::SeqCst);
            if ready() {
                break;
            }
            std::thread::park();
        }
        state.store(SLOT_AWAKE, Ordering::SeqCst);
        self.sleepers.fetch_sub(1, Ordering::SeqCst);
    }

    /// Wake one parked worker, if any is parked. A push with every worker of
    /// the band running pays one load and no syscall.
    pub(super) fn wake_one(&self) {
        if self.sleepers.load(Ordering::SeqCst) == 0 {
            return;
        }
        for slot in &self.slots {
            if slot
                .state
                .compare_exchange(
                    SLOT_SLEEPING,
                    SLOT_AWAKE,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                if let Some(t) = slot.thread.get() {
                    t.unpark();
                }
                return;
            }
        }
    }

    /// Wake every parked worker — the shutdown path, where each has to re-test
    /// its own exit condition.
    pub(super) fn wake_all(&self) {
        for slot in &self.slots {
            if let Some(t) = slot.thread.get() {
                slot.state.store(SLOT_AWAKE, Ordering::SeqCst);
                t.unpark();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    /// The two states the emptiness test has to separate, and the transition
    /// back — `pop` on an empty queue must not consume the dummy that makes
    /// the next push valid.
    #[test]
    fn empty_and_non_empty_are_the_two_boundaries_of_is_empty() {
        let q = BandQueue::<u32>::with_capacity(4);
        assert!(q.is_empty());
        assert_eq!(q.pop(), None);
        assert!(q.is_empty());
        q.push(7).unwrap();
        assert!(!q.is_empty());
        assert_eq!(q.pop(), Some(7));
        assert!(q.is_empty());
        q.push(8).unwrap();
        assert_eq!(q.pop(), Some(8));
    }

    /// `CHUNK0` is the first chunk boundary and every later one doubles, so a
    /// queue preallocated for two elements has to cross several of them. The
    /// index arithmetic is what is on trial: a mislocated element shows up as
    /// a value that comes back in the wrong order or not at all.
    #[test]
    fn the_queue_grows_across_chunk_boundaries_in_order() {
        let q = BandQueue::<usize>::with_capacity(1);
        // Miri interprets every one of these, so it gets the smallest span
        // that still crosses several boundaries.
        let n = if cfg!(miri) { CHUNK0 * 3 } else { CHUNK0 * 40 };
        for i in 0..n {
            q.push(i).unwrap();
        }
        for i in 0..n {
            assert_eq!(q.pop(), Some(i), "entry {i} came back out of order");
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
            q.push(round).unwrap();
            q.push(round + 1_000_000).unwrap();
            assert_eq!(q.pop(), Some(round));
            assert_eq!(q.pop(), Some(round + 1_000_000));
        }
        assert!(q.is_empty());
    }

    /// Four producers against four consumers on an arena of eight: every
    /// entry is delivered exactly once. Order is not asserted here — above one
    /// worker a batch can be published onto the previous batch's remainder,
    /// which is the band's documented order at that worker count.
    #[test]
    fn every_entry_is_delivered_exactly_once_under_many_consumers() {
        const PRODUCERS: usize = 4;
        let per: usize = if cfg!(miri) { 60 } else { 5_000 };
        let q = Arc::new(BandQueue::<(usize, usize)>::with_capacity(8));
        let taken = Arc::new(std::sync::Mutex::new(vec![Vec::new(); PRODUCERS]));
        let done = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicUsize::new(0));

        std::thread::scope(|s| {
            for p in 0..PRODUCERS {
                let q = Arc::clone(&q);
                let finished = Arc::clone(&finished);
                s.spawn(move || {
                    for i in 0..per {
                        q.push((p, i)).unwrap();
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
                        match q.pop() {
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
            let q = BandQueue::<Arc<()>>::with_capacity(4);
            for _ in 0..5 {
                q.push(Arc::clone(&live)).unwrap();
            }
            assert_eq!(Arc::strong_count(&live), 6);
            assert!(q.pop().is_some());
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
                p.register(0);
                // Already true: returns without ever parking.
                p.park_until(0, || gate.load(Ordering::SeqCst));
                announced2.store(true, Ordering::SeqCst);
                // False until the main thread flips it.
                p.park_until(0, || go2.load(Ordering::SeqCst));
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

    /// `wake_one` with nobody parked is a load and nothing else — the
    /// property the band's per-push wake-up depends on for its cost.
    #[test]
    fn waking_a_band_with_no_sleeper_is_a_no_op() {
        let parking = Parking::new(2);
        parking.wake_one();
        parking.wake_all();
        parking.register(0);
        parking.wake_one();
    }

    /// Submission order at one worker, the band's default. Pushes land while
    /// earlier entries are still in `ready`, so the queue refills several
    /// times and the batch boundaries fall at varying depths — the boundary a
    /// stack would reorder entries at if a refill published onto a `ready`
    /// that was not empty.
    #[test]
    fn entries_leave_in_submission_order_across_batches() {
        let q = BandQueue::<usize>::with_capacity(8);
        let mut pushed = 0usize;
        let mut popped = 0usize;
        for round in 0..200usize {
            for _ in 0..=(round % 5) {
                q.push(pushed).unwrap();
                pushed += 1;
            }
            for _ in 0..=(round % 3) {
                if popped == pushed {
                    break;
                }
                assert_eq!(q.pop(), Some(popped), "entry {popped} left out of turn");
                popped += 1;
            }
        }
        while popped < pushed {
            assert_eq!(q.pop(), Some(popped));
            popped += 1;
        }
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
            q.push(v).unwrap();
        }
        // Refills, hands back the oldest and leaves 1 and 2 in `ready`.
        assert_eq!(q.pop(), Some(0));
        for v in 3..6 {
            q.push(v).unwrap();
        }
        assert_eq!(q.refill(), Some(3), "the second batch's oldest entry");
        assert_eq!(q.pop(), Some(4));
        assert_eq!(q.pop(), Some(5));
        assert_eq!(q.pop(), Some(1));
        assert_eq!(q.pop(), Some(2));
        assert!(q.is_empty());
        assert_eq!(q.pop(), None);
    }

    /// The layout the 46%/33%/9% loss above was measured against. The
    /// guarantee is that no field of the enclosing band can be placed in
    /// `Parking`'s extent, which is what the 128-byte alignment buys — where
    /// `sleepers` sits inside that extent does not matter, since the whole
    /// block is the park state's. Asserted rather than commented, because an
    /// attribute is easy to drop and nothing else makes alignment visible.
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
